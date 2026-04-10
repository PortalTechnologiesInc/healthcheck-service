use rocket::serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Configuration loaded from `static/services.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub name: String,
    pub url: String,
    pub host: String,
    pub icon: String,
    pub expected_status: Vec<u16>,
    pub offline_signals: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assertions: Option<Vec<HealthAssertion>>,
}

/// Assertion to evaluate against a JSON health-check response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HealthAssertion {
    /// Check that a JSON field equals an expected string value.
    #[serde(rename = "field_equals")]
    FieldEquals {
        field: String,
        value: String,
        #[serde(default = "default_down")]
        on_fail: HealthAssertionSeverity,
    },
    /// Check that one numeric field is >= another numeric field.
    #[serde(rename = "field_gte")]
    FieldGte {
        field: String,
        other_field: String,
        #[serde(default = "default_degraded")]
        on_fail: HealthAssertionSeverity,
    },
    /// Check that a numeric field is > 0.
    #[serde(rename = "field_positive")]
    FieldPositive {
        field: String,
        #[serde(default = "default_down")]
        on_fail: HealthAssertionSeverity,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthAssertionSeverity {
    Degraded,
    Down,
}

fn default_down() -> HealthAssertionSeverity {
    HealthAssertionSeverity::Down
}

fn default_degraded() -> HealthAssertionSeverity {
    HealthAssertionSeverity::Degraded
}

/// Health state exposed to the UI/API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceHealth {
    Up,
    Degraded,
    Down,
    Unknown,
}

/// Current runtime view of each service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub service: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord_message_id: Option<String>,
    pub health: ServiceHealth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_changed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incident_id: Option<String>,
}

/// Incident history for downtime tracking + notifications.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incident {
    pub id: String,
    pub service: String,
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

/// Shared snapshot stored behind `Arc<RwLock<...>>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub generated_at: u64,
    pub services: Vec<ServiceStatus>,
    pub incidents: Vec<Incident>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval_secs: Option<u64>,
}

pub type SharedState = Arc<RwLock<StateSnapshot>>;
