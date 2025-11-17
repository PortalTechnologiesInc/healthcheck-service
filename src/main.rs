#[macro_use]
extern crate rocket;

mod models;

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
use std::env;
use std::fs;
use std::sync::{Arc, RwLock};
use std::time::{Duration as StdDuration, Instant, SystemTime, UNIX_EPOCH};
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

#[get("/api/status")]
fn status(state: &rocket::State<SharedState>) -> Json<StateSnapshot> {
    let snapshot = state.read().expect("state poisoned").clone();
    Json(snapshot)
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
    let configs = Arc::new(load_service_configs());
    let poll_settings = Arc::new(load_poll_settings());
    let snapshot = build_initial_snapshot(&configs, poll_settings.poll_interval.as_secs());
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
        .manage(http_client.clone())
        .manage(Arc::clone(&notifier))
        .mount("/", FileServer::from(relative!("static")))
        .mount("/", routes![status, incident_history, refresh])
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
            let rx = poll_rx;

            Box::pin(async move {
                tokio::spawn(async move {
                    run_polling_loop(state, configs, settings, client, notifier, rx).await;
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

impl DiscordNotifier {
    fn new(client: Client) -> Self {
        let webhook_url = env::var("DISCORD_WEBHOOK_URL").ok();
        Self {
            client,
            webhook_url,
        }
    }

    async fn notify_transition(&self, context: TransitionContext<'_>) {
        let Some(webhook) = &self.webhook_url else {
            return;
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

        if let Err(err) = self.client.post(webhook).json(&payload).send().await {
            warn!("Failed to send Discord notification: {err}");
        }
    }
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
    mut receiver: mpsc::Receiver<PollCommand>,
) {
    info!(
        "Starting polling engine for {} services (interval: {:?})",
        configs.len(),
        settings.poll_interval
    );
    let mut ticker = interval(settings.poll_interval);

    loop {
        select! {
            _ = ticker.tick() => {
                poll_and_update(&state, &configs, &settings, &client, &notifier).await;
            }
            cmd = receiver.recv() => {
                match cmd {
                    Some(PollCommand::RefreshNow) => {
                        info!("Manual refresh requested");
                        poll_and_update(&state, &configs, &settings, &client, &notifier).await;
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

            notifier
                .notify_transition(TransitionContext {
                    service: &cfg.name,
                    previous: prev_health,
                    current: status.health,
                    timestamp: now,
                    latency_ms: status.latency_ms,
                    incident_id: status.incident_id.clone(),
                    error: status.error.clone(),
                })
                .await;
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
        *guard = next_snapshot;
    }
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

struct CheckResult {
    health: ServiceHealth,
    latency_ms: Option<u128>,
    error: Option<String>,
}

impl ServiceStatus {
    fn from_config(cfg: &ServiceConfig) -> Self {
        ServiceStatus {
            service: cfg.name.clone(),
            health: ServiceHealth::Unknown,
            latency_ms: None,
            last_checked: None,
            last_changed: None,
            error: None,
            incident_id: None,
        }
    }
}
