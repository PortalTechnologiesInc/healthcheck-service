## Healthcheck Service

Portal's health monitoring service built with Rocket. It serves a status dashboard, polls downstream services, and emits Discord alerts when downtime or degradations occur.

### Features
- Background polling engine driven by Tokio, with per-service configuration.
- Central state store tracking live status, latency, and incident windows.
- Discord webhook notifications on status transitions and recoveries.
- REST API endpoints (`/api/status`, `/api/incidents`) for UI and integrations.
- Tailwind-based dashboard served alongside the API (server-rendered or minimal-JS fetch).

### Architecture
1. **Config loader** reads `services.json` (or env) into typed `ServiceConfig` instances.
2. **Polling task** loops at a configurable interval, calling each service, deriving status, latency, and incident events.
3. **State manager** (`Arc<RwLock<State>>`) stores latest `ServiceStatus` records plus incident history.
4. **Discord notifier** sends webhook payloads whenever services change state.
5. **Rocket routes** expose the current snapshot via JSON and render the dashboard UI.

```
[Config] -> [Polling Engine] -> [State Store] -> [API/UI]
                               -> [Discord Webhook]
```

### Getting Started
1. Install Rust toolchain (1.75+ recommended).
2. Clone repository and install dependencies:
   ```bash
   cargo build
   ```
3. Configure services in `static/services.json` (temporary) or planned `config/services.toml`.
4. Set environment variables:
   - `DISCORD_WEBHOOK_URL` (optional until notifier implemented)
   - `POLL_INTERVAL_SECS` (defaults to 60)
5. Run the server:
   ```bash
   cargo run
   ```
6. Open `http://localhost:8000` to view the dashboard.

### Roadmap
- Implement shared state structs and mock `/api/status`.
- Build polling engine with incident tracking.
- Integrate Discord notifications and persistence layer.
- Convert static HTML into Rocket templates or server-fed JSON.
- Add tests (unit + integration) and deployment docs.

### Contributing
1. Fork + clone.
2. Create feature branch: `git checkout -b feature/my-change`.
3. Run `cargo fmt && cargo clippy` before committing.
4. Submit PR with description of changes/tests.

### License
MIT © Portal Technologies Inc.

