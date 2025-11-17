use crate::models::{Incident, StateSnapshot};
use rusqlite::{Connection, params};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug)]
pub struct Storage {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct IncidentWithMeta {
    pub incident: Incident,
    pub last_reminded_at: Option<u64>,
}

impl Storage {
    pub fn new(path: impl Into<PathBuf>) -> rusqlite::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let parent_buf = parent.to_path_buf();
            fs::create_dir_all(&parent_buf)
                .map_err(|_| rusqlite::Error::InvalidPath(parent_buf.clone()))?;
        }

        let conn = Connection::open(path)?;
        let storage = Storage {
            conn: Mutex::new(conn),
        };
        storage.run_migrations()?;
        Ok(storage)
    }

    fn run_migrations(&self) -> rusqlite::Result<()> {
        let conn = self.conn.lock().expect("storage mutex poisoned");
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS incidents (
                id TEXT PRIMARY KEY,
                service TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                ended_at INTEGER,
                summary TEXT,
                remediation TEXT,
                last_reminded_at INTEGER
            );

            CREATE TABLE IF NOT EXISTS snapshots (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                generated_at INTEGER NOT NULL,
                payload TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    pub fn load_incidents(&self) -> rusqlite::Result<Vec<Incident>> {
        let conn = self.conn.lock().expect("storage mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, service, started_at, ended_at, summary, remediation
             FROM incidents
             ORDER BY started_at ASC",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(Incident {
                id: row.get::<_, String>(0)?,
                service: row.get::<_, String>(1)?,
                started_at: row.get::<_, i64>(2)? as u64,
                ended_at: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                summary: row.get::<_, Option<String>>(4)?,
                remediation: row.get::<_, Option<String>>(5)?,
            })
        })?;

        let mut incidents = Vec::new();
        for row in rows {
            incidents.push(row?);
        }
        Ok(incidents)
    }

    pub fn load_open_incidents(&self) -> rusqlite::Result<Vec<IncidentWithMeta>> {
        let conn = self.conn.lock().expect("storage mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, service, started_at, summary, remediation, last_reminded_at
             FROM incidents
             WHERE ended_at IS NULL",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(IncidentWithMeta {
                incident: Incident {
                    id: row.get::<_, String>(0)?,
                    service: row.get::<_, String>(1)?,
                    started_at: row.get::<_, i64>(2)? as u64,
                    ended_at: None,
                    summary: row.get::<_, Option<String>>(3)?,
                    remediation: row.get::<_, Option<String>>(4)?,
                },
                last_reminded_at: row.get::<_, Option<i64>>(5)?.map(|v| v as u64),
            })
        })?;

        let mut incidents = Vec::new();
        for row in rows {
            incidents.push(row?);
        }
        Ok(incidents)
    }

    pub fn upsert_incident(&self, incident: &Incident) -> rusqlite::Result<()> {
        let conn = self.conn.lock().expect("storage mutex poisoned");
        conn.execute(
            r#"
            INSERT INTO incidents (id, service, started_at, ended_at, summary, remediation)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(id) DO UPDATE SET
                service = excluded.service,
                started_at = excluded.started_at,
                ended_at = excluded.ended_at,
                summary = excluded.summary,
                remediation = excluded.remediation
            "#,
            params![
                incident.id,
                incident.service,
                incident.started_at as i64,
                incident.ended_at.map(|v| v as i64),
                incident.summary,
                incident.remediation
            ],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn close_incident(&self, id: &str, ended_at: u64) -> rusqlite::Result<()> {
        let conn = self.conn.lock().expect("storage mutex poisoned");
        conn.execute(
            "UPDATE incidents SET ended_at = ?2 WHERE id = ?1",
            params![id, ended_at as i64],
        )?;
        Ok(())
    }

    pub fn update_last_reminded(&self, id: &str, timestamp: u64) -> rusqlite::Result<()> {
        let conn = self.conn.lock().expect("storage mutex poisoned");
        conn.execute(
            "UPDATE incidents SET last_reminded_at = ?2 WHERE id = ?1",
            params![id, timestamp as i64],
        )?;
        Ok(())
    }

    pub fn record_snapshot(&self, snapshot: &StateSnapshot) -> rusqlite::Result<()> {
        let payload = serde_json::to_string(snapshot)
            .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?;
        let conn = self.conn.lock().expect("storage mutex poisoned");
        conn.execute(
            "INSERT INTO snapshots (generated_at, payload) VALUES (?1, ?2)",
            params![snapshot.generated_at as i64, payload],
        )?;
        Ok(())
    }
}

pub fn database_path() -> PathBuf {
    std::env::var("HEALTHCHECK_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("healthcheck.db"))
}
