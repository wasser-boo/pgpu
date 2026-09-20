//! Agent-Hub: WS-Call-home `/api/v1/node`. Verwaltet verbundene Agents,
//! Heartbeats, Kommandos mit Ergebnis (oneshot) und Terminal-Relays.

use praxis_common::node::RouterCommand;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

pub type CommandResult = Result<serde_json::Value, String>;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct HeartbeatData {
    pub health_json: serde_json::Value,
    pub busy: bool,
    pub busy_reason: String,
    pub gpu_json: serde_json::Value,
    pub progress_json: serde_json::Value,
    pub disk_free_gb: f64,
}

impl Default for HeartbeatData {
    fn default() -> Self {
        Self {
            health_json: serde_json::json!("starting"),
            busy: false,
            busy_reason: String::new(),
            gpu_json: serde_json::json!({}),
            progress_json: serde_json::json!({}),
            disk_free_gb: 0.0,
        }
    }
}

#[allow(dead_code)]
pub struct AgentHandle {
    pub tx: mpsc::UnboundedSender<RouterCommand>,
    pub slot_id: i64,
    pub nb_ip: Option<String>,
    pub connected_at: i64,
    pub last_seen: i64,
    pub heartbeat: HeartbeatData,
    pub draining: bool,
    pub services: HashMap<String, bool>,
}

struct HubInner {
    agents: HashMap<i64, Arc<Mutex<AgentHandle>>>,
    /// Letzte Herzschlag-Zeit pro Instanz — überlebt `unregister`, damit
    /// der Reconciler auch NACH einem WS-Abriß „Agent seit X s still"
    /// erkennen kann (Sitzung weg = last_seen(None) sonst nicht unterscheidbar).
    last_seen_all: HashMap<i64, i64>,
    pending: HashMap<u64, oneshot::Sender<CommandResult>>,
    terms: HashMap<u64, mpsc::UnboundedSender<serde_json::Value>>,
    next_id: u64,
}

impl Default for HubInner {
    fn default() -> Self {
        Self {
            agents: HashMap::new(),
            last_seen_all: HashMap::new(),
            pending: HashMap::new(),
            terms: HashMap::new(),
            next_id: 0,
        }
    }
}

#[derive(Clone, Default)]
pub struct Hub {
    inner: Arc<Mutex<HubInner>>,
    next_term: Arc<AtomicU64>,
}

const TERM_ID_BASE: u64 = 1_000_000;

impl Hub {
    /// Registriert einen Agent. Der WS-Task hält die Receiver-Seite.
    pub fn register(
        &self,
        vast_id: i64,
        slot_id: i64,
        nb_ip: Option<String>,
        services: HashMap<String, bool>,
    ) -> mpsc::UnboundedReceiver<RouterCommand> {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = AgentHandle {
            tx,
            slot_id,
            nb_ip,
            connected_at: chrono::Utc::now().timestamp(),
            last_seen: chrono::Utc::now().timestamp(),
            heartbeat: HeartbeatData::default(),
            draining: false,
            services,
        };
        self.inner
            .lock()
            .unwrap()
            .agents
            .insert(vast_id, Arc::new(Mutex::new(handle)));
        self.inner.lock().unwrap().last_seen_all.insert(vast_id, chrono::Utc::now().timestamp());
        rx
    }

    pub fn unregister(&self, vast_id: i64) {
        let mut inner = self.inner.lock().unwrap();
        inner.agents.remove(&vast_id);
        // last_seen_all bleibt bewusst stehen (s. oben).
    }

    #[allow(dead_code)]
    pub fn connected_ids(&self) -> Vec<i64> {
        let inner = self.inner.lock().unwrap();
        inner.agents.keys().copied().collect()
    }

    pub fn heartbeat(&self, vast_id: i64) -> Option<HeartbeatData> {
        let inner = self.inner.lock().unwrap();
        inner.agents.get(&vast_id).map(|h| h.lock().unwrap().heartbeat.clone())
    }

    pub fn last_seen(&self, vast_id: i64) -> Option<i64> {
        let inner = self.inner.lock().unwrap();
        match inner.agents.get(&vast_id) {
            Some(h) => Some(h.lock().unwrap().last_seen),
            None => inner.last_seen_all.get(&vast_id).copied(),
        }
    }

    pub fn nb_ip(&self, vast_id: i64) -> Option<String> {
        let inner = self.inner.lock().unwrap();
        inner.agents.get(&vast_id).and_then(|h| h.lock().unwrap().nb_ip.clone())
    }

    #[allow(dead_code)]
    pub fn draining(&self, vast_id: i64) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.agents.get(&vast_id).map(|h| h.lock().unwrap().draining).unwrap_or(false)
    }

    #[allow(dead_code)]
    pub fn set_draining(&self, vast_id: i64, draining: bool) {
        let inner = self.inner.lock().unwrap();
        if let Some(h) = inner.agents.get(&vast_id) {
            h.lock().unwrap().draining = draining;
        }
    }

    /// Heartbeat vom WS-Task einsortieren.
    pub fn record_heartbeat(&self, vast_id: i64, hb: HeartbeatData, nb_ip: Option<String>) {
        let mut inner = self.inner.lock().unwrap();
        let ts = chrono::Utc::now().timestamp();
        if let Some(h) = inner.agents.get(&vast_id) {
            let mut g = h.lock().unwrap();
            g.heartbeat = hb;
            g.last_seen = ts;
            if nb_ip.is_some() {
                g.nb_ip = nb_ip;
            }
        }
        inner.last_seen_all.insert(vast_id, ts);
    }

    /// Kommando senden und auf Ergebnis warten.
    pub async fn command(
        &self,
        vast_id: i64,
        make_cmd: impl FnOnce(u64) -> RouterCommand,
    ) -> CommandResult {
        let (id, cmd, tx_agent) = {
            let mut inner = self.inner.lock().unwrap();
            let handle = inner.agents.get(&vast_id).map(|h| h.lock().unwrap().tx.clone());
            let Some(tx_agent) = handle else {
                return Err(format!("Agent der Instanz {vast_id} nicht verbunden"));
            };
            let id = inner.next_id;
            inner.next_id += 1;
            (id, make_cmd(id), tx_agent)
        };
        let (res_tx, res_rx) = oneshot::channel::<CommandResult>();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.pending.insert(id, res_tx);
        }
        if tx_agent.send(cmd).is_err() {
            let mut inner = self.inner.lock().unwrap();
            inner.pending.remove(&id);
            return Err("Agent-Verbindung geschlossen".into());
        }
        match tokio::time::timeout(std::time::Duration::from_secs(90), res_rx).await {
            Ok(Ok(r)) => r,
            _ => {
                let mut inner = self.inner.lock().unwrap();
                inner.pending.remove(&id);
                Err("Timeout beim Warten auf Agent-Antwort".into())
            }
        }
    }

    /// cmd_result vom WS-Task auflösen.
    pub fn resolve(&self, id: u64, result: CommandResult) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(tx) = inner.pending.remove(&id) {
            let _ = tx.send(result);
        }
    }

    // -------------------------------------------------- Terminal-Relay

    pub fn new_term_id(&self) -> u64 {
        self.next_term.fetch_add(1, Ordering::Relaxed) + TERM_ID_BASE
    }

    /// Dashboard registriert einen Kanal für term-Frames.
    pub fn register_term(&self, term_id: u64, tx: mpsc::UnboundedSender<serde_json::Value>) {
        let mut inner = self.inner.lock().unwrap();
        inner.terms.insert(term_id, tx);
    }

    pub fn unregister_term(&self, term_id: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.terms.remove(&term_id);
    }

    /// term-Frames vom Agent → Dashboard weiterleiten.
    pub fn relay_term(&self, frame: serde_json::Value) {
        let term_id = frame.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let inner = self.inner.lock().unwrap();
        if let Some(tx) = inner.terms.get(&term_id) {
            let _ = tx.send(frame);
        }
    }

    /// stdin/close vom Dashboard → Agent.
    pub fn term_send(&self, vast_id: i64, msg: RouterCommand) -> Result<(), String> {
        let inner = self.inner.lock().unwrap();
        let handle = inner.agents.get(&vast_id).map(|h| h.lock().unwrap().tx.clone());
        match handle {
            Some(tx) => tx.send(msg).map_err(|_| "Agent-Verbindung geschlossen".to_string()),
            None => Err(format!("Agent der Instanz {vast_id} nicht verbunden")),
        }
    }
}