//! Konfiguration (TOML). Siehe config.example.toml im Repo-Root.

use praxis_common::{Mode, Role};
use praxis_policy::{BidConfig, BudgetConfig, IdleConfig, LimitsConfig, SlotPolicyCfg, SwapConfig};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub router: RouterCfg,
    #[serde(default)]
    pub vast: VastCfg,
    #[serde(default)]
    pub netbird: NetbirdCfg,
    #[serde(default)]
    pub stt: SttCfg,
    #[serde(default = "default_budget")]
    pub budget: BudgetConfig,
    #[serde(default = "default_limits")]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub slots: Vec<SlotCfg>,
    #[serde(default)]
    pub alerts: AlertsCfg,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouterCfg {
    /// Bind-IP für alle Ports (Produktion: NetBird-IP; Test: 127.0.0.1).
    #[serde(default = "d_bind")]
    pub bind_ip: String,
    #[serde(default = "d_port")]
    pub dashboard_port: u16,
    #[serde(default = "d_data")]
    pub data_dir: PathBuf,
    /// Token für API/Dashboard (env ROUTER_TOKEN überschreibt).
    #[serde(default)]
    pub token: String,
    #[serde(default = "d_tz")]
    pub tz: String,
    /// `X-Router-Wait`-Maximum in Sekunden (Hold auf kaltem Slot).
    #[serde(default = "d_wait")]
    pub wait_for_backend_max_s: u64,
}

impl Default for RouterCfg {
    fn default() -> Self {
        Self {
            bind_ip: d_bind(),
            dashboard_port: d_port(),
            data_dir: d_data(),
            token: String::new(),
            tz: d_tz(),
            wait_for_backend_max_s: d_wait(),
        }
    }
}

fn d_bind() -> String {
    "127.0.0.1".into()
}
fn d_port() -> u16 {
    8080
}
fn d_data() -> PathBuf {
    "/data".into()
}
fn d_tz() -> String {
    "Europe/Berlin".into()
}
fn d_wait() -> u64 {
    600
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct VastCfg {
    /// Vast API-Key (env VAST_API_KEY überschreibt).
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "d_poll")]
    pub poll_interval_s: u64,
    #[serde(default = "d_offer_poll")]
    pub offer_poll_s: u64,
}

fn d_poll() -> u64 {
    30
}
fn d_offer_poll() -> u64 {
    60
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NetbirdCfg {
    /// Management-API-Token (ephemerere Setup-Keys minten). Leer = statischer Key.
    #[serde(default)]
    pub api_token: String,
    /// Management-API-Base (Cloud: https://api.netbird.io; self-hosted: eigene URL).
    #[serde(default = "d_nb_api")]
    pub api_url: String,
    /// Management-URL für PEER-ENROLLMENT (NB_MANAGEMENT_URL auf den Boxen).
    /// Pflicht bei self-hosted NetBird, sonst enrollt die Box in die Cloud!
    #[serde(default = "d_nb_mgmt")]
    pub management_url: String,
    /// Statischer Setup-Key als Fallback.
    #[serde(default)]
    pub setup_key: String,
    #[serde(default = "d_group")]
    pub group: String,
    /// Auto-Gruppen für gemintete Setup-Keys (Namen ODER IDs). Boxen müssen in
    /// der Zugriffs-Gruppe der Router-/Dev-Policy sein (z. B. "servers", damit
    /// developers→servers greift) UND in der GPU-Gruppe für gpu→router:8080.
    #[serde(default)]
    pub groups: Vec<String>,
    /// NetBird-IP des Router-Hosts (wie Agents ihn erreichen).
    #[serde(default)]
    pub router_nb_ip: String,
    /// Port des Router-Dashboards aus Agent-Sicht (Call-home).
    #[serde(default = "d_nb_port")]
    pub router_nb_port: u16,
}

fn d_nb_api() -> String {
    "https://api.netbird.io".into()
}

fn d_nb_mgmt() -> String {
    "https://api.netbird.io".into()
}

fn d_group() -> String {
    "gpu".into()
}
fn d_nb_port() -> u16 {
    8080
}

#[derive(Debug, Clone, Deserialize)]
pub struct SttCfg {
    /// `local` = Sidecar im Router-Compose, `media_slot` = per Passthrough.
    #[serde(default = "d_stt_mode")]
    pub mode: String,
    #[serde(default = "d_stt_url")]
    pub url: String,
    /// Lokaler Listen-Port für STT (Router-Bind-IP).
    #[serde(default = "d_stt_port")]
    pub listen_port: u16,
}

fn d_stt_port() -> u16 {
    2700
}

impl Default for SttCfg {
    fn default() -> Self {
        Self { mode: d_stt_mode(), url: d_stt_url(), listen_port: d_stt_port() }
    }
}

fn d_stt_mode() -> String {
    "local".into()
}
fn d_stt_url() -> String {
    "http://127.0.0.1:2700".into()
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AlertsCfg {
    /// Optional: Webhook (ntfy/Telegram/Discord) für Alerts.
    #[serde(default)]
    pub webhook_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SlotCfg {
    pub id: i64,
    pub role: Role,
    #[serde(default)]
    pub name: String,
    /// Image, das auf Vast gebootet wird.
    #[serde(default)]
    pub image: String,
    #[serde(default = "d_disk")]
    pub disk_gb: i64,
    /// Vast-Suchpredicate (bundles q=) für den Slot.
    #[serde(default)]
    pub search_query: String,
    /// Vast on-demand-Suchpredicate (falls anders, sonst search_query).
    #[serde(default)]
    #[allow(dead_code)]
    pub search_query_on_demand: Option<String>,
    /// Passthrough: lokaler Router-Port (als String im TOML) → Servicename.
    #[serde(default)]
    pub passthrough: HashMap<String, String>,
    #[serde(default)]
    pub services: HashMap<String, ServiceCfg>,
    #[serde(default)]
    pub bid: BidConfig,
    #[serde(default)]
    pub idle: Option<IdleConfig>,
    #[serde(default)]
    pub swap: SwapConfig,
    /// Extra-Env beim Instanz-Create (z. B. LLAMA_MODEL).
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Geschätzter Model-Traffic (GB) fürs Offer-Scoring.
    #[serde(default = "d_traffic")]
    pub traffic_gb: f64,
    /// Warm-Fenster "09:00-18:00" → gewünscht-Status automatisch.
    #[serde(default)]
    pub warm_hours: Option<String>,
}

fn d_disk() -> i64 {
    60
}
fn d_traffic() -> f64 {
    5.0
}

impl SlotCfg {
    pub fn idle_cfg(&self, role: Role) -> IdleConfig {
        self.idle.clone().unwrap_or(match role {
            Role::Llm => IdleConfig { stop_after_s: 900, destroy_after_stopped_s: 172_800 },
            Role::Media => IdleConfig { stop_after_s: 600, destroy_after_stopped_s: 3600 },
        })
    }
    /// (Port, Service) sortiert.
    pub fn passthrough_ports(&self) -> Vec<(u16, String)> {
        let mut out: Vec<(u16, String)> = self
            .passthrough
            .iter()
            .filter_map(|(p, s)| p.parse::<u16>().ok().map(|p| (p, s.clone())))
            .collect();
        out.sort_by_key(|(p, _)| *p);
        out
    }

    pub fn policy(&self) -> SlotPolicyCfg {
        SlotPolicyCfg {
            bid: self.bid.clone(),
            idle: self.idle_cfg(self.role),
            swap: self.swap.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceCfg {
    pub port: u16,
    /// HTTP-Health-Pfad (relativ zum Service).
    #[serde(default)]
    #[allow(dead_code)]
    pub health: Option<String>,
    #[serde(default)]
    pub busy: BusyKind,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum BusyKind {
    /// Nur Router-In-flight zählt.
    #[default]
    Inflight,
    /// ComfyUI: GET /prompt → exec_info.queue_remaining.
    ComfyQueue,
    /// Vosk-WS: offene Sessions im Router.
    WsSessions,
    /// GPU-Util über Agent (Fallback > 20 %).
    Agent,
    None,
}

fn default_budget() -> BudgetConfig {
    BudgetConfig { daily_soft_eur: 2.0, daily_hard_eur: 2.4, monthly_eur: 50.0, usd_per_eur: 1.08 }
}

fn default_limits() -> LimitsConfig {
    LimitsConfig { max_instances: 3, max_per_slot: 2, max_total_rate_usd_h: 0.60 }
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&raw)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.slots.is_empty() {
            anyhow::bail!("keine Slots konfiguriert");
        }
        for s in &self.slots {
            if s.name.is_empty() {
                anyhow::bail!("slot {} ohne name", s.id);
            }
        }
        Ok(())
    }

    pub fn slot(&self, id: i64) -> Option<&SlotCfg> {
        self.slots.iter().find(|s| s.id == id)
    }

    #[allow(dead_code)]
    pub fn slot_by_role(&self, role: Role) -> Option<&SlotCfg> {
        self.slots.iter().find(|s| s.role == role)
    }

    pub fn policy_config(&self) -> praxis_policy::PolicyConfig {
        let mut slots = HashMap::new();
        for s in &self.slots {
            slots.insert(s.id, s.policy());
        }
        praxis_policy::PolicyConfig {
            budget: self.budget.clone(),
            limits: self.limits.clone(),
            slots,
        }
    }

    /// Router-Token: env > config. Pflicht für API/Dashboard.
    pub fn router_token(&self) -> String {
        std::env::var("ROUTER_TOKEN").unwrap_or_else(|_| self.router.token.clone())
    }

    pub fn vast_api_key(&self) -> String {
        std::env::var("VAST_API_KEY").unwrap_or_else(|_| self.vast.api_key.clone())
    }
}

/// Wie der Agent den Router nennt (Call-home URL).
pub fn router_call_url(cfg: &Config) -> String {
    format!(
        "ws://{}:{}/api/v1/node",
        cfg.netbird.router_nb_ip, cfg.netbird.router_nb_port
    )
}

/// Modus-Hilfsanzeige.
#[allow(dead_code)]
pub fn mode_label(m: Mode) -> &'static str {
    match m {
        Mode::Interruptible => "interruptible",
        Mode::OnDemand => "on-demand",
        Mode::Manual => "manual",
    }
}