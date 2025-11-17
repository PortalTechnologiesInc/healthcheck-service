## Healthcheck Service

Portal's health monitoring service built with Rocket. It serves a Tailwind dashboard, polls downstream services, and emits Discord alerts when downtime or degradations occur. Runtime state is persisted to SQLite so incident history survives restarts, exposed via JSON APIs plus a lightweight client-side UI rendered through Rocket templates.

### Features
- Tokio-driven polling engine with per-service headers/timeouts, backed by `reqwest` + rustls.
- Shared snapshot (`Arc<RwLock<StateSnapshot>>`) capturing live status, latency, incidents, and poll cadence.
- Discord webhook notifications emitted on every health transition (`Up`, `Degraded`, `Down`).
- SQLite-backed incident + snapshot history for cross-restart continuity (default file `healthcheck.db`).
- Reminder worker that pings the same Discord webhook when outages exceed a configurable duration.
- REST API surface: `/api/status`, `/api/incidents`, `/api/refresh`.
- Tailwind dashboard + dedicated incident log page rendered via `rocket_dyn_templates`, consuming `/api/status` + `/api/incidents` and reflecting the live poll interval.

### Architecture
1. **Config loader** reads `services.json` (or env) into typed `ServiceConfig` instances.
2. **Polling task** loops at a configurable interval, calling each service, deriving status, latency, and incident events.
3. **State manager** (`Arc<RwLock<State>>`) stores latest `ServiceStatus` records plus incident history.
4. **Discord notifier** sends webhook payloads whenever services change state.
5. **Rocket routes** expose the current snapshot via JSON and serve the dashboard UI + static assets.

```
[Config] -> [Polling Engine] -> [State Store] -> [API/UI]
                               -> [Discord Webhook]
```

### Requirements
- Rust 1.75+ (stable toolchain)
- OpenSSL is **not** required (reqwest is built with `rustls-tls`)
- Discord webhook URL (optional but recommended)

### Configuration
1. **Services**: edit `static/services.json` (temporary home) to describe each upstream. Supported fields:
   - `expected_status`, `offline_signals`, `headers`, `timeout_ms`, etc.
2. **Environment**: copy `env.example` to `.env` (already gitignored) and adjust values. Supported variables:
   - `DISCORD_WEBHOOK_URL` – Discord channel webhook.
   - `POLL_INTERVAL_SECS` – default 60; minimum enforced at 5.
   - `REQUEST_TIMEOUT_MS` – fallback request timeout (per-service `timeout_ms` wins).
   - `HEALTHCHECK_DB_PATH` – optional SQLite file path (defaults to `healthcheck.db` in the workspace).
   - `OUTAGE_REMINDERS_ENABLED` – set to `false`/`0`/`off` to disable Discord reminder pings.
   - `OUTAGE_REMINDER_MINUTES` – outage age before reminders start (default 30 minutes).
   - `OUTAGE_REMINDER_REPEAT_MINUTES` – cadence for subsequent reminders (default matches threshold).
   - `OUTAGE_REMINDER_CHECK_SECS` – how often the reminder worker scans open incidents (default 60 seconds).
3. Dotenv is loaded automatically on boot, so `cargo run` picks up `.env`. Override per run with standard env exports if needed.

### Running Locally
```bash
cargo run
# open http://localhost:8000
```
The overview dashboard fetches `/api/status` every interval reported by the backend. Click the “Open incidents” tile (or visit `/incidents`) to view the incident log UI, which reads `/api/incidents` directly in the browser. Use `/api/refresh` (POST) to force an immediate poll.

### Persistence

- Incidents and snapshots are persisted to a local SQLite file (default `healthcheck.db`). Override the location via `HEALTHCHECK_DB_PATH`.
- Inspect data with any SQLite client, e.g.:
  ```bash
  sqlite3 healthcheck.db 'SELECT service, started_at, ended_at FROM incidents'
  ```

### API Surface
- `GET /api/status` → `StateSnapshot` { `services`, `incidents`, `poll_interval_secs`, ... }.
- `GET /api/incidents` → historical incident records (persisted in SQLite across restarts).
- `POST /api/refresh` → manually trigger the polling loop (no auth yet).

### Testing & Checks
- Format/lint: `cargo fmt && cargo clippy --all-targets`
- Manual webhook test:
  ```bash
  curl -XPOST -H "Content-Type: application/json" \
    -d '{"content":"healthcheck ping"}' "$DISCORD_WEBHOOK_URL"
  ```
  Confirm the message hits Discord before relying on automated notifications.

### Contributing
1. Fork + clone.
2. Create feature branch: `git checkout -b feature/my-change`.
3. Run `cargo fmt && cargo clippy` before committing.
4. Submit PR with description of changes/tests.

### License
MIT © Portal Technologies Inc.

