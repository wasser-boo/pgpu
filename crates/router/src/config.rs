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
    /// Zeitgesteuerte Aktionen (Cron light): s. SchedRule. Wird pro
    /// Reconciler-Tick geprüft, Dedup pro Tag in der DB — persistent,
    /// überlebt Router-Restarts.
    #[serde(default)]
    pub schedule: Vec<SchedRule>,
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

#[derive(Debug, Clone, Deserialize)]
pub struct VastCfg {
    #[serde(default = "d_true")]
    pub activate_blacklist: bool,
    #[serde(default)]
    pub activate_whitelist: bool,
    /// Vast API-Key (env VAST_API_KEY überschreibt).
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "d_poll")]
    pub poll_interval_s: u64,
    #[serde(default = "d_offer_poll")]
    pub offer_poll_s: u64,
    /// Machine-Blacklist: Host nach N Fails (Warmup-Tod/unreachable/
    /// Warmup-Timeout) automatisch blacklisten. 0/1 = aus (Spike: 2).
    #[serde(default = "d_blacklist_fails")]
    pub blacklist_after_fails: i64,
    /// Wie lange ein unreachable-Host läuft, bevor der Router ihn stoppt
    /// (GPU-Geld brennt, Agent >3 min still; Disk bleibt erhalten).
    #[serde(default = "d_unreachable_stop_s")]
    pub unreachable_stop_after_s: i64,
}

impl Default for VastCfg {
    fn default() -> Self {
        Self { activate_blacklist: true, activate_whitelist: false, api_key: String::new(), poll_interval_s: d_poll(), offer_poll_s: d_offer_poll(),
            blacklist_after_fails: d_blacklist_fails(), unreachable_stop_after_s: d_unreachable_stop_s() }
    }
}

fn d_true() -> bool { true }

fn d_poll() -> u64 {
    30
}
fn d_offer_poll() -> u64 {
    60
}
fn d_blacklist_fails() -> i64 {
    2
}
fn d_unreachable_stop_s() -> i64 {
    300
}

#[derive(Debug, Clone, Deserialize)]
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
    /// Retired owned GPU peers only; existing/sleeping contracts are never removed.
    #[serde(default = "d_true")]
    pub cleanup_unused_peers: bool,
}

impl Default for NetbirdCfg {
    fn default() -> Self {
        Self { api_token: String::new(), api_url: d_nb_api(), management_url: d_nb_mgmt(),
            setup_key: String::new(), group: d_group(), groups: Vec::new(),
            router_nb_ip: String::new(), router_nb_port: d_nb_port(), cleanup_unused_peers: true }
    }
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

/// Eine zeitgesteuerte Regel ([[schedule]] in der config.toml):
/// cron-light — tägliche/wochentägliche Uhrzeit-Trigger für Rent/Sleep/
/// Destroy-All. Beispiele:
///
/// ```toml
/// [[schedule]]
/// time = "18:00"
/// days = "daily"
/// action = "destroy_all"   # zerstört ALLE nicht gepinnten Instanzen + auto_rent aus
///
/// [[schedule]]
/// time = "07:00"
/// days = "weekdays"
/// action = "wake"          # auto_rent an + Slots gewünscht → warm, wenn man aufsteht
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct SchedRule {
    /// "HH:MM" in router.tz.
    pub time: String,
    /// "daily" | "weekdays" | "weekends" | "Mon,Wed,Fri" (3- oder Vollnamen).
    #[serde(default = "d_sched_days")]
    pub days: String,
    /// "sleep"/"sleep_all" (auto_rent aus, Boxen stoppen) | "wake"/"rent"
    /// (auto_rent an, Slots mieten) | "destroy_all" (alle Instanzen weg +
    /// aus) | "lock"/"unlock" (Slot+Instanz fixieren/freigeben) |
    /// "pin"/"unpin".
    pub action: String,
    /// Nur diese Slots (Default: alle).
    #[serde(default)]
    pub slots: Option<Vec<i64>>,
    /// Für action="budget": Override fürs Tagesbudget (€). Beide None +
    /// action="budget_reset" = Override löschen (config.toml gilt wieder).
    #[serde(default)]
    pub soft_eur: Option<f64>,
    #[serde(default)]
    pub hard_eur: Option<f64>,
}

fn d_sched_days() -> String {
    "daily".into()
}

impl SchedRule {
    /// Minuten seit Mitternacht (Validierung inklusive).
    pub fn minutes(&self) -> Option<u32> {
        let mut it = self.time.splitn(2, ':');
        let h: u32 = it.next()?.parse().ok()?;
        let m: u32 = it.next()?.parse().ok()?;
        (h < 24 && m < 60).then_some(h * 60 + m)
    }

    /// Passt der Wochentag (1=Mon .. 7=Sun)?
    pub fn matches_weekday(&self, weekday: u8) -> bool {
        let d = self.days.trim().to_ascii_lowercase();
        if d.is_empty() || d == "daily" || d == "*" {
            return true;
        }
        if d == "weekdays" {
            return (1..=5).contains(&weekday);
        }
        if d == "weekends" || d == "weekend" {
            return weekday >= 6;
        }
        // Kommaliste: Mon/Tue/... (3 Buchstaben reichen, Groß/Klein egal).
        d.split(',').any(|tok| {
            let t = tok.trim().to_ascii_lowercase();
            DAY_NAMES
                .iter()
                .position(|n| n.starts_with(&t) || *n == t)
                .map(|idx| idx as u8 + 1 == weekday)
                .unwrap_or(false)
        })
    }
}

const DAY_NAMES: [&str; 7] = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];

#[derive(Debug, Clone, Deserialize)]
pub struct AlertsCfg {
    /// Optional alert endpoint; treat the entire URL as a secret.
    #[serde(default)]
    pub webhook_url: String,
    /// Additional destinations. All receive alerts/tests; duplicates are sent once.
    #[serde(default)]
    pub webhook_urls: Vec<String>,
    /// auto detects Discord, Slack and ntfy.sh per target; otherwise generic JSON.
    #[serde(default)]
    pub webhook_format: crate::webhook::Format,
    /// Actual instance/slot transitions, not repeated heartbeats.
    #[serde(default = "d_true")]
    pub state_changes: bool,
    /// 0 disables the digest. Default: every four hours, persisted across restarts.
    #[serde(default = "d_spend_summary")]
    pub spend_summary_interval_s: u64,
}
fn d_spend_summary() -> u64 { 4*3600 }
impl Default for AlertsCfg {
    fn default()->Self { Self {webhook_url:String::new(),webhook_urls:Vec::new(),webhook_format:Default::default(),state_changes:true,spend_summary_interval_s:d_spend_summary()} }
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
    /// Mietmodus: "interruptible" (Default) oder "on_demand".
    #[serde(default)]
    pub mode: String,
    /// Vast-Suchpredicate (bundles q=) für den Slot.
    #[serde(default)]
    pub search_query: String,
    /// Vast on-demand-Suchpredicate (falls anders, sonst search_query).
    #[serde(default)]
    #[allow(dead_code)]
    pub search_query_on_demand: Option<String>,
    #[serde(default)]
    pub location: crate::selection::Location,
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
    /// Instanz-Pool (s. praxis_policy::PoolConfig): warm gleichzeitig
    /// laufende Boxen, total Obergrenze inkl. kalter Reserve.
    #[serde(default)]
    pub pool: praxis_policy::PoolConfig,
    #[serde(default)]
    pub requirements: praxis_policy::eligibility::Requirements,
    #[serde(default)]
    pub performance: crate::performance::PerformanceConfig,
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
            mode: match self.mode.trim().to_ascii_lowercase().as_str() {
                "on_demand" | "on-demand" | "ondemand" => praxis_policy::SlotMode::OnDemand,
                _ => praxis_policy::SlotMode::Interruptible,
            },
            pool: self.pool,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceCfg {
    pub port: u16,
    /// HTTP-Health-Pfad (relativ zum Service) — wird als
    /// PRAXIS_AGENT_HEALTH_URLS an den Agent geschickt: healthy erst,
    /// wenn der Endpoint NACH dem Boot/Download wirklich antwortet.
    #[serde(default)]
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
    BudgetConfig { daily_soft_eur: 2.0, daily_hard_eur: 2.4, monthly_eur: 50.0, usd_per_eur: 1.08, hard_action: "stop".into() }
}

fn default_limits() -> LimitsConfig {
    LimitsConfig { max_instances: 3, max_per_slot: 2, max_total_rate_usd_h: 0.60 }
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Self::load_str(&raw)
    }

    /// Parse+Validierung ohne Disk (Hot-Reload: Dashboard/API schicken
    /// Rohtext, erst NACH erfolgreicher Validierung wird geschrieben).
    pub fn load_str(raw: &str) -> anyhow::Result<Self> {
        // TOML's Display includes the original line (possibly a webhook/API
        // secret). Never echo that into logs, API errors or redirect URLs.
        let cfg: Config = toml::from_str(raw).map_err(|error: toml::de::Error| {
            let prefix=raw.get(..error.span().map(|span|span.start).unwrap_or(0)).unwrap_or("");
            let line=prefix.bytes().filter(|b|*b==b'\n').count()+1;
            let column=prefix.rsplit('\n').next().unwrap_or("").chars().count()+1;
            anyhow::anyhow!("TOML/schema error at line {line}, column {column}; check field names/types (values hidden to protect secrets)")
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.slots.is_empty() {
            anyhow::bail!("keine Slots konfiguriert");
        }
        crate::webhook::validate(&self.alerts)?;
        anyhow::ensure!(self.router.tz.parse::<chrono_tz::Tz>().is_ok(), "invalid router.tz");
        anyhow::ensure!(self.router.bind_ip == "auto" || self.router.bind_ip.parse::<std::net::IpAddr>().is_ok(), "invalid router.bind_ip");
        let nonnegative = |v: f64| v.is_finite() && v >= 0.0;
        anyhow::ensure!([self.budget.daily_soft_eur, self.budget.daily_hard_eur, self.budget.monthly_eur].into_iter().all(nonnegative), "budgets must be finite and nonnegative");
        anyhow::ensure!(self.budget.usd_per_eur.is_finite() && self.budget.usd_per_eur > 0.0, "usd_per_eur must be positive");
        anyhow::ensure!(self.budget.daily_soft_eur <= self.budget.daily_hard_eur, "soft budget exceeds hard budget");
        anyhow::ensure!(["stop", "destroy"].contains(&self.budget.hard_action.as_str()), "invalid budget.hard_action");
        anyhow::ensure!(self.limits.max_instances > 0 && self.limits.max_per_slot > 0 && self.limits.max_total_rate_usd_h.is_finite() && self.limits.max_total_rate_usd_h > 0.0, "invalid instance/rate limits");
        let mut ids = std::collections::HashSet::new();
        let mut ports = std::collections::HashSet::new();
        anyhow::ensure!(self.router.dashboard_port != 0, "dashboard port must not be zero");
        ports.insert(self.router.dashboard_port);
        anyhow::ensure!(["local", "media_slot"].contains(&self.stt.mode.as_str()), "invalid stt.mode");
        if self.stt.mode == "local" {
            anyhow::ensure!(self.stt.listen_port != 0 && ports.insert(self.stt.listen_port), "duplicate/invalid STT listener port");
        }
        for s in &self.slots {
            anyhow::ensure!(s.id > 0 && ids.insert(s.id), "duplicate/invalid slot id {}", s.id);
            anyhow::ensure!(!s.name.trim().is_empty(), "slot {} ohne name", s.id);
            anyhow::ensure!(s.disk_gb > 0 && nonnegative(s.traffic_gb), "invalid disk/traffic for slot {}", s.id);
            anyhow::ensure!(["", "interruptible", "on_demand", "on-demand", "ondemand"].contains(&s.mode.as_str()), "invalid slot mode");
            anyhow::ensure!(s.pool.warm > 0 && s.pool.warm <= s.pool.total && s.pool.total <= self.limits.max_per_slot as usize, "invalid pool limits for slot {}", s.id);
            anyhow::ensure!([s.bid.margin, s.bid.ceiling_usd_h, s.bid.rent_min_usd_h].into_iter().all(nonnegative) && s.bid.rent_min_usd_h <= s.bid.ceiling_usd_h, "invalid bid window for slot {}", s.id);
            praxis_vast::query::parse_query(&s.search_query)
                .map_err(|e| anyhow::anyhow!("slot {} search_query: {e}", s.id))?;
            if let Some(query) = &s.search_query_on_demand {
                praxis_vast::query::parse_query(query)
                    .map_err(|e| anyhow::anyhow!("slot {} search_query_on_demand: {e}", s.id))?;
            }
            for on_demand in [false,true] {
                crate::selection::query(s,on_demand)
                    .map_err(|e| anyhow::anyhow!("slot {} selection: {e}",s.id))?;
            }
            s.requirements.validate().map_err(anyhow::Error::msg)?;
            s.performance.validate().map_err(anyhow::Error::msg)?;
            if s.performance.benchmark.enabled {
                anyhow::ensure!(s.role == Role::Llm, "built-in benchmark currently supports LLM slots only");
                anyhow::ensure!(s.services.values().any(|svc|svc.port == s.performance.benchmark.spec.port), "benchmark port must be a configured slot service");
            }
            let idle = s.idle_cfg(s.role);
            anyhow::ensure!(idle.stop_after_s >= 0 && idle.destroy_after_stopped_s >= 0 && s.swap.max_warmup_s > 0, "invalid lifecycle timers for slot {}", s.id);
            for (port, service) in &s.passthrough {
                let port: u16 = port.parse()?;
                anyhow::ensure!(port != 0 && ports.insert(port), "duplicate/invalid listener port {port}");
                anyhow::ensure!(s.services.contains_key(service), "unknown passthrough service {service}");
            }
            anyhow::ensure!(s.services.values().all(|svc| svc.port != 0), "service port must not be zero");
        }
        for (i, r) in self.schedule.iter().enumerate() {
            if r.minutes().is_none() {
                anyhow::bail!("schedule[{i}]: time muss \"HH:MM\" sein (ist {:?})", r.time);
            }
            let a = r.action.trim().to_ascii_lowercase();
            if !["sleep","sleep_all","wake","rent","destroy_all","lock","unlock","pin","unpin","budget","budget_reset"].contains(&a.as_str()) {
                anyhow::bail!("schedule[{i}]: unbekannte action {:?} (sleep|wake|rent|destroy_all|lock|unlock|pin|unpin|budget|budget_reset)", r.action);
            }
            if a == "budget" && r.soft_eur.is_none() && r.hard_eur.is_none() {
                anyhow::bail!("schedule[{i}]: action \"budget\" braucht soft_eur und/oder hard_eur");
            }
            anyhow::ensure!(r.soft_eur.into_iter().chain(r.hard_eur).all(nonnegative), "invalid schedule budget");
            if let (Some(soft), Some(hard)) = (r.soft_eur, r.hard_eur) {
                anyhow::ensure!(soft <= hard, "schedule soft budget exceeds hard budget");
            }
            if let Some(slots) = &r.slots {
                anyhow::ensure!(slots.iter().all(|id| ids.contains(id)), "schedule references unknown slot");
            }
        }
        Ok(())
    }

    /// Required at startup/reload, including loopback. Fail closed if an env
    /// override accidentally clears the token. Cookie-safe tokens only.
    pub fn validate_auth(&self) -> anyhow::Result<()> {
        let token = self.router_token();
        anyhow::ensure!(token.len() >= 32 && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "ROUTER_TOKEN must contain at least 32 ASCII letters/digits, '-' or '_' (e.g. openssl rand -hex 32)");
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

    /// NetBird-PAT (mintet ephemere Setup-Keys): env > config.
    pub fn netbird_api_token(&self) -> String {
        std::env::var("NB_API_TOKEN").unwrap_or_else(|_| self.netbird.api_token.clone())
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