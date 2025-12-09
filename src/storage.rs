use crate::models::{Incident, ServiceHealth, StateSnapshot};
use rusqlite::{Connection, params};
use std::collections::HashMap;
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

#[derive(Debug, Clone, Default)]
pub struct ServiceWeeklyStats {
    pub avg_latency_ms: Option<f64>,
    pub uptime_percentage: f64,
    #[allow(dead_code)]
    pub total_checks: u64,
    #[allow(dead_code)]
    pub up_checks: u64,
}

#[derive(Debug, Clone)]
pub struct WeeklyStats {
    pub services: HashMap<String, ServiceWeeklyStats>,
    pub total_incidents: usize,
    pub longest_outage: Option<LongestOutage>,
}

#[derive(Debug, Clone)]
pub struct LongestOutage {
    pub service: String,
    pub duration_secs: u64,
}

impl Storage {
    pub fn new(path: impl Into<PathBuf>) -> rusqlite::Result<Self> {
        let path = path.into();
        match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                let parent_buf = parent.to_path_buf();
                fs::create_dir_all(&parent_buf)
                    .map_err(|_| rusqlite::Error::InvalidPath(parent_buf.clone()))?;
            }
            _ => {}
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

    #[allow(dead_code)]
    pub fn get_incidents_in_range(&self, start: u64, end: u64) -> rusqlite::Result<Vec<Incident>> {
        let conn = self.conn.lock().expect("storage mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, service, started_at, ended_at, summary, remediation
             FROM incidents
             WHERE started_at >= ?1 AND started_at <= ?2
             ORDER BY started_at ASC",
        )?;

        let rows = stmt.query_map(params![start as i64, end as i64], |row| {
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

    pub fn get_weekly_stats(&self, since: u64, until: u64) -> rusqlite::Result<WeeklyStats> {
        let conn = self.conn.lock().expect("storage mutex poisoned");

        // Get all snapshots in the time range
        let mut stmt = conn.prepare(
            "SELECT payload FROM snapshots WHERE generated_at >= ?1 AND generated_at <= ?2",
        )?;

        let rows = stmt.query_map(params![since as i64, until as i64], |row| {
            row.get::<_, String>(0)
        })?;

        // Aggregate stats per service
        let mut service_latencies: HashMap<String, Vec<u128>> = HashMap::new();
        let mut service_up_checks: HashMap<String, u64> = HashMap::new();
        let mut service_total_checks: HashMap<String, u64> = HashMap::new();

        for row in rows {
            let payload = row?;
            if let Ok(snapshot) = serde_json::from_str::<StateSnapshot>(&payload) {
                for svc in &snapshot.services {
                    *service_total_checks.entry(svc.service.clone()).or_insert(0) += 1;

                    if svc.health == ServiceHealth::Up {
                        *service_up_checks.entry(svc.service.clone()).or_insert(0) += 1;
                    }

                    if let Some(latency) = svc.latency_ms {
                        service_latencies
                            .entry(svc.service.clone())
                            .or_default()
                            .push(latency);
                    }
                }
            }
        }

        // Build per-service stats
        let mut services: HashMap<String, ServiceWeeklyStats> = HashMap::new();
        for (name, total) in &service_total_checks {
            let up = *service_up_checks.get(name).unwrap_or(&0);
            let uptime_percentage = if *total > 0 {
                (up as f64 / *total as f64) * 100.0
            } else {
                100.0
            };

            let avg_latency_ms = service_latencies.get(name).map(|latencies| {
                if latencies.is_empty() {
                    0.0
                } else {
                    latencies.iter().sum::<u128>() as f64 / latencies.len() as f64
                }
            });

            services.insert(
                name.clone(),
                ServiceWeeklyStats {
                    avg_latency_ms,
                    uptime_percentage,
                    total_checks: *total,
                    up_checks: up,
                },
            );
        }

        // Get incidents and find longest outage
        drop(stmt);
        let incidents = self.get_incidents_in_range_internal(&conn, since, until)?;
        let total_incidents = incidents.len();

        let longest_outage = incidents
            .iter()
            .filter_map(|inc| {
                let end = inc.ended_at.unwrap_or(until);
                let duration = end.saturating_sub(inc.started_at);
                Some(LongestOutage {
                    service: inc.service.clone(),
                    duration_secs: duration,
                })
            })
            .max_by_key(|o| o.duration_secs);

        Ok(WeeklyStats {
            services,
            total_incidents,
            longest_outage,
        })
    }

    fn get_incidents_in_range_internal(
        &self,
        conn: &Connection,
        start: u64,
        end: u64,
    ) -> rusqlite::Result<Vec<Incident>> {
        let mut stmt = conn.prepare(
            "SELECT id, service, started_at, ended_at, summary, remediation
             FROM incidents
             WHERE started_at >= ?1 AND started_at <= ?2
             ORDER BY started_at ASC",
        )?;

        let rows = stmt.query_map(params![start as i64, end as i64], |row| {
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
}

pub fn database_path() -> PathBuf {
    std::env::var("HEALTHCHECK_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("healthcheck.db"))
}
