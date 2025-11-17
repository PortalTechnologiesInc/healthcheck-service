#[macro_use]
extern crate rocket;

mod models;
mod storage;

use chrono::{DateTime, Utc};
use dotenvy::dotenv;
use models::{Incident, ServiceConfig, ServiceHealth, ServiceStatus, SharedState, StateSnapshot};
use reqwest::Client;
use rocket::fairing::AdHoc;
use rocket::fs::{FileServer, relative};
use rocket::http::Status as HttpStatus;
use rocket::serde::json::Json;
use rocket::tokio::{
    self, select,
    sync::mpsc,
    time::{Duration as TokioDuration, interval},
};
use rocket_dyn_templates::{Template, context};
use std::env;
use std::fs;
use std::sync::{Arc, RwLock};
use std::time::{Duration as StdDuration, Instant, SystemTime, UNIX_EPOCH};
use storage::{Storage, database_path};
use tracing::{error, info, warn};
use uuid::Uuid;

const SERVICES_PATH: &str = "static/services.json";
const DEFAULT_POLL_INTERVAL_SECS: u64 = 60;
const DEFAULT_TIMEOUT_MS: u64 = 5_000;

#[derive(Clone)]
struct PollSettings {
    poll_interval: TokioDuration,
    request_timeout: StdDuration,
}

#[derive(Clone)]
struct ReminderSettings {
    enabled: bool,
    check_interval: TokioDuration,
    outage_threshold: TokioDuration,
    repeat_interval: TokioDuration,
}

#[derive(Clone)]
struct DiscordNotifier {
    client: Client,
    webhook_url: Option<String>,
}

enum PollCommand {
    RefreshNow,
}

#[derive(Clone)]
struct PollerHandle {
    sender: mpsc::Sender<PollCommand>,
}

struct TransitionContext<'a> {
    service: &'a str,
    previous: ServiceHealth,
    current: ServiceHealth,
    timestamp: u64,
    latency_ms: Option<u128>,
    incident_id: Option<String>,
    error: Option<String>,
}

struct ReminderContext<'a> {
    service: &'a str,
    incident_id: &'a str,
    duration_minutes: u64,
}

enum NotificationResult {
    Created(String),
    Updated,
    Failed,
}

#[get("/api/status")]
fn status(state: &rocket::State<SharedState>) -> Json<StateSnapshot> {
    let snapshot = state.read().expect("state poisoned").clone();
    Json(snapshot)
}

#[get("/")]
fn index() -> Template {
    Template::render("index", context! {})
}

#[get("/incidents")]
fn incidents() -> Template {
    Template::render("incidents", context! {})
}

#[get("/api/incidents")]
fn incident_history(state: &rocket::State<SharedState>) -> Json<Vec<Incident>> {
    let incidents = state.read().expect("state poisoned").incidents.clone();
    Json(incidents)
}

#[post("/api/refresh")]
async fn refresh(handle: &rocket::State<PollerHandle>) -> HttpStatus {
    match handle.sender.send(PollCommand::RefreshNow).await {
        Ok(_) => HttpStatus::Accepted,
        Err(err) => {
            error!("Failed to enqueue refresh request: {err}");
            HttpStatus::InternalServerError
        }
    }
}

#[launch]
fn rocket() -> _ {
    dotenv().ok();
    init_tracing();
    let storage =
        Arc::new(Storage::new(database_path()).expect("failed to initialize SQLite storage"));
    let configs = Arc::new(load_service_configs());
    let poll_settings = Arc::new(load_poll_settings());
    let reminder_settings = Arc::new(load_reminder_settings());
    let mut snapshot = build_initial_snapshot(&configs, poll_settings.poll_interval.as_secs());
    match storage.load_incidents() {
        Ok(persisted) => {
            snapshot.incidents = persisted;
        }
        Err(err) => warn!("Failed to hydrate incidents from storage: {err}"),
    }
    let shared_state: SharedState = Arc::new(RwLock::new(snapshot));
    let http_client = Client::builder()
        .user_agent("PortalHealthcheck/0.1")
        .build()
        .expect("failed to build reqwest client");
    let notifier = Arc::new(DiscordNotifier::new(http_client.clone()));
    let (poll_tx, poll_rx) = mpsc::channel(4);

    rocket::build()
        .manage(shared_state.clone())
        .manage(Arc::clone(&configs))
        .manage(PollerHandle {
            sender: poll_tx.clone(),
        })
        .manage(Arc::clone(&poll_settings))
        .manage(Arc::clone(&reminder_settings))
        .manage(http_client.clone())
        .manage(Arc::clone(&notifier))
        .manage(Arc::clone(&storage))
        .mount("/static", FileServer::from(relative!("static")))
        .mount("/", routes![index, incidents, status, incident_history, refresh])
        .attach(Template::fairing())
        .attach(AdHoc::on_liftoff("Polling Engine", move |rocket| {
            let state = rocket
                .state::<SharedState>()
                .expect("shared state missing")
                .clone();
            let configs = rocket
                .state::<Arc<Vec<ServiceConfig>>>()
                .expect("configs missing")
                .clone();
            let settings = rocket
                .state::<Arc<PollSettings>>()
                .expect("poll settings missing")
                .clone();
            let client = rocket
                .state::<Client>()
                .expect("http client missing")
                .clone();
            let notifier = rocket
                .state::<Arc<DiscordNotifier>>()
                .expect("notifier missing")
                .clone();
            let storage = rocket
                .state::<Arc<Storage>>()
                .expect("storage missing")
                .clone();
            let reminder_settings = rocket
                .state::<Arc<ReminderSettings>>()
                .expect("reminder settings missing")
                .clone();
            let rx = poll_rx;

            Box::pin(async move {
                let poll_storage = storage.clone();
                let poll_notifier = notifier.clone();
                let reminder_storage = storage.clone();
                let reminder_notifier = notifier.clone();
                let reminder_settings = reminder_settings.clone();

                tokio::spawn(async move {
                    run_polling_loop(
                        state,
                        configs,
                        settings,
                        client,
                        poll_notifier,
                        poll_storage,
                        rx,
                    )
                    .await;
                });

                tokio::spawn(async move {
                    reminder_watcher(reminder_storage, reminder_notifier, reminder_settings).await;
                });
            })
        }))
}

fn load_service_configs() -> Vec<ServiceConfig> {
    match fs::read_to_string(SERVICES_PATH) {
        Ok(contents) => match serde_json::from_str::<Vec<ServiceConfig>>(&contents) {
            Ok(configs) => configs,
            Err(err) => {
                error!("Failed to parse {}: {err}", SERVICES_PATH);
                Vec::new()
            }
        },
        Err(err) => {
            error!("Failed to read {}: {err}", SERVICES_PATH);
            Vec::new()
        }
    }
}

fn build_initial_snapshot(configs: &[ServiceConfig], poll_interval_secs: u64) -> StateSnapshot {
    let generated_at = current_timestamp();
    let services = configs
        .iter()
        .map(ServiceStatus::from_config)
        .collect::<Vec<_>>();

    StateSnapshot {
        generated_at,
        services,
        incidents: Vec::new(),
        config_version: Some(format!("{} services", configs.len())),
        poll_interval_secs: Some(poll_interval_secs),
    }
}

fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn init_tracing() {
    let filter = env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn load_poll_settings() -> PollSettings {
    let poll_interval_secs = env::var("POLL_INTERVAL_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECS);
    let timeout_ms = env::var("REQUEST_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_MS);

    PollSettings {
        poll_interval: TokioDuration::from_secs(poll_interval_secs.max(5)),
        request_timeout: StdDuration::from_millis(timeout_ms.max(500)),
    }
}

fn load_reminder_settings() -> ReminderSettings {
    let enabled = env::var("OUTAGE_REMINDERS_ENABLED")
        .map(|raw| {
            let lowered = raw.to_lowercase();
            !matches!(lowered.as_str(), "0" | "false" | "off")
        })
        .unwrap_or(true);

    let threshold_minutes = env::var("OUTAGE_REMINDER_MINUTES")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(30)
        .max(1);

    let repeat_minutes = env::var("OUTAGE_REMINDER_REPEAT_MINUTES")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(threshold_minutes)
        .max(1);

    let check_secs = env::var("OUTAGE_REMINDER_CHECK_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(60)
        .max(30);

    ReminderSettings {
        enabled,
        check_interval: TokioDuration::from_secs(check_secs),
        outage_threshold: TokioDuration::from_secs(threshold_minutes * 60),
        repeat_interval: TokioDuration::from_secs(repeat_minutes * 60),
    }
}

impl DiscordNotifier {
    fn new(client: Client) -> Self {
        let webhook_url = env::var("DISCORD_WEBHOOK_URL").ok();
        Self {
            client,
            webhook_url,
        }
    }

    fn base_url(webhook: &str) -> &str {
        webhook.split('?').next().unwrap_or(webhook)
    }

    fn wait_url(webhook: &str) -> String {
        if webhook.contains('?') {
            format!("{webhook}&wait=true")
        } else {
            format!("{webhook}?wait=true")
        }
    }

    fn is_configured(&self) -> bool {
        self.webhook_url.is_some()
    }

    async fn notify_transition(
        &self,
        context: TransitionContext<'_>,
        existing_message_id: Option<&str>,
    ) -> NotificationResult {
        let Some(webhook) = &self.webhook_url else {
            return NotificationResult::Failed;
        };

        let payload = DiscordPayload {
            username: "Portal Healthcheck",
            content: format!(
                "**{}** status changed: `{:?}` → `{:?}` at {}",
                context.service,
                context.previous,
                context.current,
                iso_timestamp(context.timestamp)
            ),
            embeds: vec![DiscordEmbed {
                title: "Status Transition".to_string(),
                description: context
                    .error
                    .clone()
                    .unwrap_or_else(|| "No error reported".to_string()),
                color: status_color(context.current),
                fields: vec![
                    DiscordField {
                        name: "Latency (ms)".to_string(),
                        value: context
                            .latency_ms
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "n/a".to_string()),
                        inline: true,
                    },
                    DiscordField {
                        name: "Incident".to_string(),
                        value: context
                            .incident_id
                            .clone()
                            .unwrap_or_else(|| "n/a".to_string()),
                        inline: true,
                    },
                ],
            }],
        };

        if let Some(message_id) = existing_message_id
            && self
                .edit_message(webhook, message_id, &payload)
                .await
                .unwrap_or(false)
        {
            return NotificationResult::Updated;
        }

        match self.create_message(webhook, &payload).await {
            Ok(Some(message_id)) => NotificationResult::Created(message_id),
            Ok(None) => NotificationResult::Failed,
            Err(err) => {
                warn!("Failed to send Discord notification: {err}");
                NotificationResult::Failed
            }
        }
    }

    async fn notify_reminder(&self, context: ReminderContext<'_>) {
        let Some(webhook) = &self.webhook_url else {
            return;
        };

        let payload = DiscordPayload {
            username: "Portal Healthcheck",
            content: format!(
                ":warning: Incident `{}` for **{}** still open ({} min).",
                context.incident_id, context.service, context.duration_minutes
            ),
            embeds: vec![DiscordEmbed {
                title: "Prolonged outage reminder".to_string(),
                description: "This service remains impaired. Please review ongoing remediation."
                    .to_string(),
                color: status_color(ServiceHealth::Down),
                fields: vec![DiscordField {
                    name: "Duration (minutes)".to_string(),
                    value: context.duration_minutes.to_string(),
                    inline: true,
                }],
            }],
        };

        if let Err(err) = self.client.post(webhook).json(&payload).send().await {
            warn!("Failed to send Discord reminder: {err}");
        }
    }

    async fn edit_message(
        &self,
        webhook: &str,
        message_id: &str,
        payload: &DiscordPayload,
    ) -> reqwest::Result<bool> {
        let base = Self::base_url(webhook).trim_end_matches('/');
        let url = format!("{base}/messages/{message_id}");
        let resp = self.client.patch(url).json(payload).send().await?;
        if resp.status().is_success() {
            Ok(true)
        } else {
            warn!("Discord webhook edit failed with status {}", resp.status());
            Ok(false)
        }
    }

    async fn create_message(
        &self,
        webhook: &str,
        payload: &DiscordPayload,
    ) -> reqwest::Result<Option<String>> {
        let url = Self::wait_url(webhook);
        let resp = self.client.post(url).json(payload).send().await?;
        if resp.status().is_success() {
            let message: DiscordMessage = resp.json().await?;
            Ok(Some(message.id))
        } else {
            warn!("Discord webhook send failed with status {}", resp.status());
            Ok(None)
        }
    }
}

#[derive(serde::Deserialize)]
struct DiscordMessage {
    id: String,
}

#[derive(serde::Serialize)]
struct DiscordPayload {
    username: &'static str,
    content: String,
    embeds: Vec<DiscordEmbed>,
}

#[derive(serde::Serialize)]
struct DiscordEmbed {
    title: String,
    description: String,
    color: u32,
    fields: Vec<DiscordField>,
}

#[derive(serde::Serialize)]
struct DiscordField {
    name: String,
    value: String,
    inline: bool,
}

fn status_color(health: ServiceHealth) -> u32 {
    match health {
        ServiceHealth::Up => 0x4ade80,
        ServiceHealth::Degraded => 0xfacc15,
        ServiceHealth::Down => 0xf87171,
        ServiceHealth::Unknown => 0x94a3b8,
    }
}

async fn run_polling_loop(
    state: SharedState,
    configs: Arc<Vec<ServiceConfig>>,
    settings: Arc<PollSettings>,
    client: Client,
    notifier: Arc<DiscordNotifier>,
    storage: Arc<Storage>,
    mut receiver: mpsc::Receiver<PollCommand>,
) {
    info!(
        "Starting polling engine for {} services (interval: {:?})",
        configs.len(),
        settings.poll_interval
    );
    let mut ticker = interval(settings.poll_interval);
    let mut bootstrapped = false;

    loop {
        select! {
            _ = ticker.tick() => {
                poll_and_update(
                    &state,
                    &configs,
                    &settings,
                    &client,
                    &notifier,
                    &storage,
                    bootstrapped,
                ).await;
                bootstrapped = true;
            }
            cmd = receiver.recv() => {
                match cmd {
                    Some(PollCommand::RefreshNow) => {
                        info!("Manual refresh requested");
                        poll_and_update(
                            &state,
                            &configs,
                            &settings,
                            &client,
                            &notifier,
                            &storage,
                            bootstrapped,
                        ).await;
                        bootstrapped = true;
                    }
                    None => {
                        warn!("Polling command channel closed; stopping loop");
                        break;
                    }
                }
            }
        }
    }
}

async fn poll_and_update(
    state: &SharedState,
    configs: &Arc<Vec<ServiceConfig>>,
    settings: &Arc<PollSettings>,
    client: &Client,
    notifier: &Arc<DiscordNotifier>,
    storage: &Arc<Storage>,
    emit_notifications: bool,
) {
    if configs.is_empty() {
        warn!("No service configs loaded; skipping poll");
        return;
    }

    let previous_snapshot = state.read().expect("state poisoned").clone();
    let now = current_timestamp();
    let mut incidents = previous_snapshot.incidents.clone();
    let mut new_services = Vec::with_capacity(configs.len());

    for cfg in configs.iter() {
        let prev_status = previous_snapshot
            .services
            .iter()
            .find(|svc| svc.service == cfg.name)
            .cloned()
            .unwrap_or_else(|| ServiceStatus::from_config(cfg));
        let prev_health = prev_status.health;

        let result = check_service(client, cfg, settings.request_timeout).await;
        let mut status = prev_status;
        status.health = result.health;
        status.latency_ms = result.latency_ms;
        status.last_checked = Some(now);
        status.error = result.error.clone();

        if status.health != prev_health {
            status.last_changed = Some(now);
            handle_incident_transition(
                cfg,
                status.health,
                prev_health,
                now,
                &mut status,
                &mut incidents,
            );

            if emit_notifications {
                match notifier
                    .notify_transition(
                        TransitionContext {
                            service: &cfg.name,
                            previous: prev_health,
                            current: status.health,
                            timestamp: now,
                            latency_ms: status.latency_ms,
                            incident_id: status.incident_id.clone(),
                            error: status.error.clone(),
                        },
                        status.discord_message_id.as_deref(),
                    )
                    .await
                {
                    NotificationResult::Created(message_id) => {
                        status.discord_message_id = Some(message_id);
                    }
                    NotificationResult::Updated => {}
                    NotificationResult::Failed => {
                        status.discord_message_id = None;
                    }
                }
            }
        } else if status.health != ServiceHealth::Up {
            // Preserve existing open incident linkage
            if let Some(open) = find_open_incident(&cfg.name, incidents.as_mut_slice()) {
                status.incident_id = Some(open.id.clone());
            }
        } else {
            status.incident_id = None;
        }

        new_services.push(status);
    }

    let mut next_snapshot = previous_snapshot;
    next_snapshot.generated_at = now;
    next_snapshot.services = new_services;
    next_snapshot.incidents = incidents;
    next_snapshot.poll_interval_secs = Some(settings.poll_interval.as_secs());

    if let Ok(mut guard) = state.write() {
        *guard = next_snapshot.clone();
    }

    persist_snapshot(storage, &next_snapshot);
}

async fn check_service(
    client: &Client,
    config: &ServiceConfig,
    fallback_timeout: StdDuration,
) -> CheckResult {
    let start = Instant::now();
    let mut request = client.get(&config.url);

    if let Some(headers) = &config.headers {
        let mut map = reqwest::header::HeaderMap::new();
        for (key, value) in headers {
            if let (Ok(name), Ok(val)) = (
                reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                map.insert(name, val);
            }
        }
        request = request.headers(map);
    }

    let timeout = config
        .timeout_ms
        .map(StdDuration::from_millis)
        .unwrap_or(fallback_timeout);
    request = request.timeout(timeout);

    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            let latency = start.elapsed().as_millis();
            let health = classify_status(status, config);
            let error = if matches!(health, ServiceHealth::Up) {
                None
            } else {
                Some(format!("Unexpected status {} from {}", status, config.host))
            };
            CheckResult {
                health,
                latency_ms: Some(latency),
                error,
            }
        }
        Err(err) => {
            warn!("Request failure for {}: {err}", config.name);
            CheckResult {
                health: ServiceHealth::Down,
                latency_ms: None,
                error: Some(err.to_string()),
            }
        }
    }
}

fn classify_status(code: u16, config: &ServiceConfig) -> ServiceHealth {
    if config.expected_status.contains(&code) {
        ServiceHealth::Up
    } else if config.offline_signals.contains(&code) {
        ServiceHealth::Down
    } else {
        ServiceHealth::Degraded
    }
}

fn handle_incident_transition(
    config: &ServiceConfig,
    new_health: ServiceHealth,
    previous: ServiceHealth,
    timestamp: u64,
    status: &mut ServiceStatus,
    incidents: &mut Vec<Incident>,
) {
    match new_health {
        ServiceHealth::Up => {
            if let Some(open) = find_open_incident(&config.name, incidents.as_mut_slice()) {
                open.ended_at = Some(timestamp);
            }
            status.incident_id = None;
        }
        ServiceHealth::Degraded | ServiceHealth::Down => {
            if previous == new_health {
                return;
            }

            if let Some(open) = find_open_incident(&config.name, incidents.as_mut_slice()) {
                status.incident_id = Some(open.id.clone());
                return;
            }

            let incident = Incident {
                id: Uuid::new_v4().to_string(),
                service: config.name.clone(),
                started_at: timestamp,
                ended_at: None,
                summary: status.error.clone(),
                remediation: None,
            };
            status.incident_id = Some(incident.id.clone());
            incidents.push(incident);
        }
        ServiceHealth::Unknown => {
            status.incident_id = None;
        }
    }
}

fn find_open_incident<'a>(
    service: &str,
    incidents: &'a mut [Incident],
) -> Option<&'a mut Incident> {
    incidents
        .iter_mut()
        .rev()
        .find(|incident| incident.service == service && incident.ended_at.is_none())
}

fn iso_timestamp(ts: u64) -> String {
    DateTime::<Utc>::from_timestamp(ts as i64, 0)
        .unwrap_or_else(Utc::now)
        .to_rfc3339()
}

fn persist_snapshot(storage: &Arc<Storage>, snapshot: &StateSnapshot) {
    for incident in &snapshot.incidents {
        if let Err(err) = storage.upsert_incident(incident) {
            warn!("Failed to upsert incident {}: {err}", incident.id);
        }
    }

    if let Err(err) = storage.record_snapshot(snapshot) {
        warn!("Failed to persist snapshot: {err}");
    }
}

async fn reminder_watcher(
    storage: Arc<Storage>,
    notifier: Arc<DiscordNotifier>,
    settings: Arc<ReminderSettings>,
) {
    if !settings.enabled {
        info!("Outage reminders disabled; watcher not started");
        return;
    }

    info!(
        "Starting reminder watcher (threshold: {:?}, repeat: {:?})",
        settings.outage_threshold, settings.repeat_interval
    );

    let mut ticker = interval(settings.check_interval);
    loop {
        ticker.tick().await;

        if !notifier.is_configured() {
            continue;
        }

        let open = match storage.load_open_incidents() {
            Ok(data) => data,
            Err(err) => {
                warn!("Failed to load open incidents for reminders: {err}");
                continue;
            }
        };

        let now = current_timestamp();
        let threshold_secs = settings.outage_threshold.as_secs();
        let repeat_secs = settings.repeat_interval.as_secs();

        for meta in open {
            let age_secs = now.saturating_sub(meta.incident.started_at);
            if age_secs < threshold_secs {
                continue;
            }

            let last = meta.last_reminded_at.unwrap_or(meta.incident.started_at);
            if now.saturating_sub(last) < repeat_secs {
                continue;
            }

            let duration_minutes = (age_secs / 60).max(1);
            notifier
                .notify_reminder(ReminderContext {
                    service: &meta.incident.service,
                    incident_id: &meta.incident.id,
                    duration_minutes,
                })
                .await;

            if let Err(err) = storage.update_last_reminded(&meta.incident.id, now) {
                warn!(
                    "Failed to update reminder timestamp for incident {}: {err}",
                    meta.incident.id
                );
            }
        }
    }
}

struct CheckResult {
    health: ServiceHealth,
    latency_ms: Option<u128>,
    error: Option<String>,
}

impl ServiceStatus {
    fn from_config(cfg: &ServiceConfig) -> Self {
        ServiceStatus {
            service: cfg.name.clone(),
            icon: Some(cfg.icon.clone()),
            discord_message_id: None,
            health: ServiceHealth::Unknown,
            latency_ms: None,
            last_checked: None,
            last_changed: None,
            error: None,
            incident_id: None,
        }
    }
}
