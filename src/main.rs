#[macro_use]
extern crate rocket;

mod models;
mod storage;

use chrono::{DateTime, Datelike, Timelike, Utc, Weekday};
use dotenvy::dotenv;
use models::{Incident, ServiceConfig, ServiceHealth, ServiceStatus, SharedState, StateSnapshot};
use storage::WeeklyStats;
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
use std::collections::HashMap;
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
const DEFAULT_GRACE_PERIOD_SECS: u64 = 120;

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
struct DebounceSettings {
    grace_period_secs: u64,
}

#[derive(Clone)]
struct WeeklyReportSettings {
    enabled: bool,
    day: Weekday,
    hour: u32,
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
    let static_assets_dir =
        env::var("STATIC_ASSETS_DIR").unwrap_or_else(|_| relative!("static").to_string());
    let storage =
        Arc::new(Storage::new(database_path()).expect("failed to initialize SQLite storage"));
    let configs = Arc::new(load_service_configs());
    let poll_settings = Arc::new(load_poll_settings());
    let reminder_settings = Arc::new(load_reminder_settings());
    let debounce_settings = Arc::new(load_debounce_settings());
    let weekly_report_settings = Arc::new(load_weekly_report_settings());
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
        .manage(Arc::clone(&debounce_settings))
        .manage(Arc::clone(&weekly_report_settings))
        .manage(http_client.clone())
        .manage(Arc::clone(&notifier))
        .manage(Arc::clone(&storage))
        .mount("/static", FileServer::from(static_assets_dir))
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
            let debounce_settings = rocket
                .state::<Arc<DebounceSettings>>()
                .expect("debounce settings missing")
                .clone();
            let weekly_report_settings = rocket
                .state::<Arc<WeeklyReportSettings>>()
                .expect("weekly report settings missing")
                .clone();
            let rx = poll_rx;

            Box::pin(async move {
                let poll_storage = storage.clone();
                let poll_notifier = notifier.clone();
                let poll_debounce = debounce_settings.clone();
                let reminder_storage = storage.clone();
                let reminder_notifier = notifier.clone();
                let reminder_settings = reminder_settings.clone();
                let weekly_storage = storage.clone();
                let weekly_notifier = notifier.clone();
                let weekly_settings = weekly_report_settings.clone();

                tokio::spawn(async move {
                    run_polling_loop(
                        state,
                        configs,
                        settings,
                        client,
                        poll_notifier,
                        poll_storage,
                        poll_debounce,
                        rx,
                    )
                    .await;
                });

                tokio::spawn(async move {
                    reminder_watcher(reminder_storage, reminder_notifier, reminder_settings).await;
                });

                tokio::spawn(async move {
                    weekly_report_watcher(weekly_storage, weekly_notifier, weekly_settings).await;
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

fn load_debounce_settings() -> DebounceSettings {
    let grace_period_secs = env::var("INCIDENT_GRACE_PERIOD_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(DEFAULT_GRACE_PERIOD_SECS);

    DebounceSettings { grace_period_secs }
}

fn load_weekly_report_settings() -> WeeklyReportSettings {
    let enabled = env::var("WEEKLY_REPORT_ENABLED")
        .map(|raw| {
            let lowered = raw.to_lowercase();
            !matches!(lowered.as_str(), "0" | "false" | "off")
        })
        .unwrap_or(true);

    let day = env::var("WEEKLY_REPORT_DAY")
        .ok()
        .and_then(|raw| match raw.to_lowercase().as_str() {
            "monday" | "mon" | "1" => Some(Weekday::Mon),
            "tuesday" | "tue" | "2" => Some(Weekday::Tue),
            "wednesday" | "wed" | "3" => Some(Weekday::Wed),
            "thursday" | "thu" | "4" => Some(Weekday::Thu),
            "friday" | "fri" | "5" => Some(Weekday::Fri),
            "saturday" | "sat" | "6" => Some(Weekday::Sat),
            "sunday" | "sun" | "0" | "7" => Some(Weekday::Sun),
            _ => None,
        })
        .unwrap_or(Weekday::Sun);

    let hour = env::var("WEEKLY_REPORT_HOUR")
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .unwrap_or(9)
        .min(23);

    WeeklyReportSettings { enabled, day, hour }
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

        if existing_message_id.is_some()
            && self
                .edit_message(webhook, existing_message_id.unwrap(), &payload)
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

    async fn notify_weekly_report(&self, stats: &WeeklyStats, week_start: u64, week_end: u64) {
        let Some(webhook) = &self.webhook_url else {
            return;
        };

        // Build per-service fields
        let mut fields: Vec<DiscordField> = stats
            .services
            .iter()
            .map(|(name, svc_stats)| {
                let latency_str = svc_stats
                    .avg_latency_ms
                    .map(|l| format!("{:.1}ms", l))
                    .unwrap_or_else(|| "n/a".to_string());
                DiscordField {
                    name: name.clone(),
                    value: format!(
                        "Uptime: {:.2}%\nAvg Latency: {}",
                        svc_stats.uptime_percentage, latency_str
                    ),
                    inline: true,
                }
            })
            .collect();

        // Add summary fields
        fields.push(DiscordField {
            name: "Total Incidents".to_string(),
            value: stats.total_incidents.to_string(),
            inline: true,
        });

        if let Some(ref longest) = stats.longest_outage {
            let duration_str = format_duration(longest.duration_secs);
            fields.push(DiscordField {
                name: "Longest Outage".to_string(),
                value: format!("{}: {}", longest.service, duration_str),
                inline: true,
            });
        }

        let payload = DiscordPayload {
            username: "Portal Healthcheck",
            content: format!(
                ":bar_chart: **Weekly Status Report** ({} - {})",
                iso_timestamp(week_start),
                iso_timestamp(week_end)
            ),
            embeds: vec![DiscordEmbed {
                title: "Service Health Summary".to_string(),
                description: "Weekly performance metrics for all monitored services.".to_string(),
                color: 0x3b82f6, // Blue color for reports
                fields,
            }],
        };

        if let Err(err) = self.client.post(webhook).json(&payload).send().await {
            warn!("Failed to send weekly report: {err}");
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
    debounce: Arc<DebounceSettings>,
    mut receiver: mpsc::Receiver<PollCommand>,
) {
    info!(
        "Starting polling engine for {} services (interval: {:?}, grace period: {}s)",
        configs.len(),
        settings.poll_interval,
        debounce.grace_period_secs
    );
    let mut ticker = interval(settings.poll_interval);
    let mut bootstrapped = false;
    // Track pending outages: service name -> timestamp when first detected as down
    let mut pending_outages: HashMap<String, u64> = HashMap::new();

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
                    &debounce,
                    &mut pending_outages,
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
                            &debounce,
                            &mut pending_outages,
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
    debounce: &Arc<DebounceSettings>,
    pending_outages: &mut HashMap<String, u64>,
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

        // Debounce logic: delay incident creation until grace period elapses
        let is_unhealthy = matches!(status.health, ServiceHealth::Down | ServiceHealth::Degraded);
        let was_healthy = matches!(prev_health, ServiceHealth::Up | ServiceHealth::Unknown);

        if is_unhealthy {
            if was_healthy {
                // Service just went down - start tracking pending outage
                pending_outages.entry(cfg.name.clone()).or_insert(now);
                info!(
                    "Service {} detected as {:?}, starting grace period",
                    cfg.name, status.health
                );
            }

            // Check if grace period has elapsed
            if let Some(&outage_start) = pending_outages.get(&cfg.name) {
                let elapsed = now.saturating_sub(outage_start);
                if elapsed >= debounce.grace_period_secs {
                    // Grace period elapsed - create incident if not already exists
                    if find_open_incident(&cfg.name, incidents.as_mut_slice()).is_none() {
                        status.last_changed = Some(outage_start);
                        handle_incident_transition(
                            cfg,
                            status.health,
                            ServiceHealth::Up, // Treat as transition from Up
                            outage_start,
                            &mut status,
                            &mut incidents,
                        );

                        if emit_notifications {
                            match notifier
                                .notify_transition(
                                    TransitionContext {
                                        service: &cfg.name,
                                        previous: ServiceHealth::Up,
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
                    } else {
                        // Already have an open incident
                        if let Some(open) = find_open_incident(&cfg.name, incidents.as_mut_slice()) {
                            status.incident_id = Some(open.id.clone());
                        }
                    }
                }
                // Else: still within grace period, don't create incident yet
            }
        } else if status.health == ServiceHealth::Up {
            // Service is back up
            if pending_outages.remove(&cfg.name).is_some() {
                info!(
                    "Service {} recovered within grace period, no incident created",
                    cfg.name
                );
            }

            // Close any open incident
            if let Some(open) = find_open_incident(&cfg.name, incidents.as_mut_slice()) {
                open.ended_at = Some(now);
                status.last_changed = Some(now);

                if emit_notifications {
                    match notifier
                        .notify_transition(
                            TransitionContext {
                                service: &cfg.name,
                                previous: prev_health,
                                current: status.health,
                                timestamp: now,
                                latency_ms: status.latency_ms,
                                incident_id: Some(open.id.clone()),
                                error: None,
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
            }
            status.incident_id = None;
        } else {
            // Unknown state
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

fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else {
        format!("{}m", minutes)
    }
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

async fn weekly_report_watcher(
    storage: Arc<Storage>,
    notifier: Arc<DiscordNotifier>,
    settings: Arc<WeeklyReportSettings>,
) {
    if !settings.enabled {
        info!("Weekly reports disabled; watcher not started");
        return;
    }

    info!(
        "Starting weekly report watcher (day: {:?}, hour: {})",
        settings.day, settings.hour
    );

    loop {
        // Calculate time until next report
        let now = Utc::now();
        let target_weekday = settings.day;
        let target_hour = settings.hour;

        // Find days until target weekday
        let current_weekday = now.weekday();
        let days_until = days_until_weekday(current_weekday, target_weekday);

        // Calculate target datetime
        let target = if days_until == 0 && now.hour() < target_hour {
            // Same day, but before target hour
            now.date_naive()
                .and_hms_opt(target_hour, 0, 0)
                .unwrap()
                .and_utc()
        } else if days_until == 0 && now.hour() >= target_hour {
            // Same day but past target hour, go to next week
            (now + chrono::Duration::days(7))
                .date_naive()
                .and_hms_opt(target_hour, 0, 0)
                .unwrap()
                .and_utc()
        } else {
            // Future day this week
            (now + chrono::Duration::days(days_until as i64))
                .date_naive()
                .and_hms_opt(target_hour, 0, 0)
                .unwrap()
                .and_utc()
        };

        let sleep_duration = (target - now).to_std().unwrap_or(StdDuration::from_secs(3600));
        info!(
            "Next weekly report scheduled for {} (in {:?})",
            target.to_rfc3339(),
            sleep_duration
        );

        tokio::time::sleep(sleep_duration).await;

        if !notifier.is_configured() {
            warn!("Discord webhook not configured, skipping weekly report");
            continue;
        }

        // Calculate week range (7 days back from now)
        let week_end = current_timestamp();
        let week_start = week_end.saturating_sub(7 * 24 * 3600);

        match storage.get_weekly_stats(week_start, week_end) {
            Ok(stats) => {
                info!("Sending weekly report with {} services", stats.services.len());
                notifier.notify_weekly_report(&stats, week_start, week_end).await;
            }
            Err(err) => {
                warn!("Failed to gather weekly stats: {err}");
            }
        }
    }
}

fn days_until_weekday(current: Weekday, target: Weekday) -> u32 {
    let current_num = current.num_days_from_sunday();
    let target_num = target.num_days_from_sunday();
    if target_num >= current_num {
        target_num - current_num
    } else {
        7 - (current_num - target_num)
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
