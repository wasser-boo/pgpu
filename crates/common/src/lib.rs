//! Protokoll- und Typdefinitionen für den Praxis GPU-Router.
//!
//! Der Agent (Repo `praxis-gpu-agent`) spiegelt die WS-Nachrichten aus
//! `node` als eigenes, kleines serde-Modul — die JSON-Formen sind stabil.

pub mod node;
pub mod performance;

/// Rolle eines Slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Llm,
    Media,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::Llm => f.write_str("llm"),
            Role::Media => f.write_str("media"),
        }
    }
}

/// Instanz-Modus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Interruptible,
    OnDemand,
    Manual,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mode::Interruptible => f.write_str("interruptible"),
            Mode::OnDemand => f.write_str("on_demand"),
            Mode::Manual => f.write_str("manual"),
        }
    }
}

/// Lifecycle-Regel einer Instanz.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Lifecycle {
    /// Standard: idle → stop → destroy nach `destroy_after_stopped_s`.
    Auto,
    /// Läuft bis `until` (RFC3339), dann stop (destroy wenn `destroy=true`).
    Until { until: String, destroy: bool },
    /// Läuft `ttl_s` Sekunden ab Erstellung, dann stop/destroy.
    Ttl { ttl_s: i64, destroy: bool },
    /// Zeitfenster, z. B. "MonFri 09:00-18:00" (prewarm_s Vorlauf).
    Schedule { spec: String, prewarm_s: i64, destroy: bool },
    /// Stop nach Inaktivität, resume zu Zeitpunkt oder nach Dauer.
    Sleep {
        stop_after_idle_s: i64,
        resume_at: Option<String>,
        resume_after_s: Option<i64>,
    },
}

impl Default for Lifecycle {
    fn default() -> Self {
        Lifecycle::Auto
    }
}

/// Zustand einer Instanz aus Sicht des Routers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    Requested,
    /// Retained contract start requested; provider allocation not yet confirmed.
    StartRequested,
    /// Provider reports a launch error before allocation; retain disk for explicit retry.
    StartFailed,
    /// Waiting for provider GPU capacity, not a dead/unreachable host.
    Scheduling,
    Provisioning,
    Booting,
    AgentConnected,
    Healthy,
    Draining,
    Stopped,
    Destroyed,
    Preempted,
    Unreachable,
    Failed,
}

impl InstanceState {
    pub fn parse(value:&str)->Self {
        match value {
            "requested"=>Self::Requested,"start_requested"=>Self::StartRequested,"start_failed"=>Self::StartFailed,
            "scheduling"=>Self::Scheduling,"provisioning"=>Self::Provisioning,
            "booting"=>Self::Booting,"agent_connected"=>Self::AgentConnected,
            "healthy"=>Self::Healthy,"draining"=>Self::Draining,"stopped"=>Self::Stopped,
            "destroyed"=>Self::Destroyed,"preempted"=>Self::Preempted,
            "unreachable"=>Self::Unreachable,_=>Self::Failed,
        }
    }
    /// No confirmed allocation in this attempt, including a failed/unconfirmed start.
    pub fn awaiting_allocation(self)->bool {
        matches!(self,Self::Requested|Self::StartRequested|Self::StartFailed|Self::Scheduling|Self::Provisioning)
    }
    /// Includes reservations/pending starts, NOT a claim that a backend serves traffic.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            InstanceState::Requested
                | InstanceState::StartRequested
                | InstanceState::StartFailed
                | InstanceState::Scheduling
                | InstanceState::Healthy
                | InstanceState::AgentConnected
                | InstanceState::Booting
                | InstanceState::Provisioning
                | InstanceState::Draining
        )
    }
    pub fn is_serving(self) -> bool {
        self == InstanceState::Healthy
    }
}

/// Agent-Health gemeldet via Heartbeat.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentHealth {
    Starting,
    Downloading { pct: f64, eta_s: Option<i64> },
    Healthy,
    Degraded { reason: String },
}

impl Default for AgentHealth {
    fn default() -> Self {
        AgentHealth::Starting
    }
}

/// Eine Policy-Aktion (reines Ergebnis von `policy::decide`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Create {
        slot_id: i64,
        offer_id: i64,
        mode: Mode,
        price_usd_h: Option<f64>,
        disk_gb: Option<i64>,
        reason: String,
    },
    Start {
        instance_id: i64,
        reason: String,
    },
    Stop {
        instance_id: i64,
        reason: String,
    },
    Destroy {
        instance_id: i64,
        reason: String,
    },
    ChangeBid {
        instance_id: i64,
        price_usd_h: f64,
        reason: String,
    },
    /// Slot auf neue Instanz flippen (neue healthy, alte drainen).
    FlipSlot {
        slot_id: i64,
        from_instance: i64,
        to_instance: i64,
        reason: String,
    },
    /// Slot-Instanz backen (flippen) + alte stoppen/destroyen.
    SwapOut {
        instance_id: i64,
        destroy: bool,
        reason: String,
    },
    Alert {
        kind: String,
        message: String,
    },
}