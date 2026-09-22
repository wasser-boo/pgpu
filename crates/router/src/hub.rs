//! Agent-Hub: WS-Call-home `/api/v1/node`. Verwaltet verbundene Agents,
//! Heartbeats, Kommandos mit Ergebnis (oneshot) und Terminal-Relays.

use praxis_common::node::RouterCommand;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

#[cfg(test)]
mod tests;

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
    session_id: u64,
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
    dialing: HashSet<i64>,
    next_session_id: u64,
    /// Letzte Herzschlag-Zeit pro Instanz — überlebt `unregister`, damit
    /// der Reconciler auch NACH einem WS-Abriß „Agent seit X s still"
    /// erkennen kann (Sitzung weg = last_seen(None) sonst nicht unterscheidbar).
    last_seen_all: HashMap<i64, i64>,
    pending: HashMap<(i64, u64), oneshot::Sender<CommandResult>>,
    terms: HashMap<(i64, u64), mpsc::Sender<serde_json::Value>>,
    next_id: u64,
}

impl Default for HubInner {
    fn default() -> Self {
        Self {
            agents: HashMap::new(),
            dialing: HashSet::new(),
            next_session_id: 0,
            last_seen_all: HashMap::new(),
            pending: HashMap::new(),
            terms: HashMap::new(),
            next_id: 0,
        }
    }
}

impl HubInner {
    fn remove_agent(&mut self, vast_id: i64) {
        self.agents.remove(&vast_id);
        self.pending.retain(|(agent, _), _| *agent != vast_id);
        self.terms.retain(|(agent, _), _| *agent != vast_id);
        // Preserve last_seen_all for the reconciler's disconnect grace period.
    }
}

/// Both inbound and outbound WebSocket tasks own a registration guard.
/// Drop runs on errors AND cancellation; an old task cannot evict its replacement.
pub struct AgentSession {
    hub: Hub,
    vast_id: i64,
    session_id: u64,
}

impl AgentSession {
    pub fn is_current(&self) -> bool {
        self.hub.inner.lock().unwrap().agents.get(&self.vast_id)
            .is_some_and(|h| h.lock().unwrap().session_id == self.session_id)
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        let mut inner = self.hub.inner.lock().unwrap();
        if inner.agents.get(&self.vast_id)
            .is_some_and(|h| h.lock().unwrap().session_id == self.session_id) {
            inner.remove_agent(self.vast_id);
        }
    }
}

/// One dial task per instance, including TCP connect and the Hello handshake.
pub struct DialAttempt {
    hub: Hub,
    vast_id: i64,
}

impl Drop for DialAttempt {
    fn drop(&mut self) { self.hub.inner.lock().unwrap().dialing.remove(&self.vast_id); }
}

#[derive(Clone, Default)]
pub struct Hub {
    inner: Arc<Mutex<HubInner>>,
}

impl Hub {
    pub fn try_dial(&self, vast_id: i64) -> Option<DialAttempt> {
        let mut inner = self.inner.lock().unwrap();
        if inner.agents.contains_key(&vast_id) || !inner.dialing.insert(vast_id) { return None; }
        Some(DialAttempt { hub: self.clone(), vast_id })
    }

    pub fn register_session(
        &self, vast_id: i64, slot_id: i64, nb_ip: Option<String>, services: HashMap<String, bool>,
    ) -> (AgentSession, mpsc::UnboundedReceiver<RouterCommand>) {
        let (session_id, rx) = self.register_inner(vast_id, slot_id, nb_ip, services);
        (AgentSession { hub: self.clone(), vast_id, session_id }, rx)
    }

    /// Fixtures can register a simulated agent without a WebSocket task.
    #[cfg(test)]
    pub fn register(
        &self, vast_id: i64, slot_id: i64, nb_ip: Option<String>, services: HashMap<String, bool>,
    ) -> mpsc::UnboundedReceiver<RouterCommand> {
        self.register_inner(vast_id, slot_id, nb_ip, services).1
    }

    fn register_inner(
        &self, vast_id: i64, slot_id: i64, nb_ip: Option<String>, services: HashMap<String, bool>,
    ) -> (u64, mpsc::UnboundedReceiver<RouterCommand>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut inner = self.inner.lock().unwrap();
        let session_id = inner.next_session_id;
        inner.next_session_id += 1;
        let handle = AgentHandle {
            session_id,
            tx,
            slot_id,
            nb_ip,
            connected_at: chrono::Utc::now().timestamp(),
            last_seen: chrono::Utc::now().timestamp(),
            heartbeat: HeartbeatData::default(),
            draining: false,
            services,
        };
        inner.remove_agent(vast_id);
        inner.agents.insert(vast_id, Arc::new(Mutex::new(handle)));
        inner.last_seen_all.insert(vast_id, chrono::Utc::now().timestamp());
        (session_id, rx)
    }

    #[cfg(test)]
    pub fn unregister(&self, vast_id: i64) {
        self.inner.lock().unwrap().remove_agent(vast_id);
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

    /// A new allocation needs a fresh session, not the pre-stop socket/heartbeat.
    /// Removing the sender wakes the old task; its guard cannot remove a new one.
    /// Caller serializes with BOTH WebSocket paths via the management lock.
    pub fn clear_boot_health(&self, vast_id: i64) {
        let mut inner = self.inner.lock().unwrap();
        inner.remove_agent(vast_id);
        inner.last_seen_all.remove(&vast_id);
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
        self.command_timeout(vast_id, make_cmd, std::time::Duration::from_secs(90)).await
    }

    pub async fn command_timeout(&self, vast_id: i64, make_cmd: impl FnOnce(u64) -> RouterCommand, timeout: std::time::Duration) -> CommandResult {
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
            inner.pending.insert((vast_id, id), res_tx);
        }
        if tx_agent.send(cmd).is_err() {
            let mut inner = self.inner.lock().unwrap();
            inner.pending.remove(&(vast_id, id));
            return Err("Agent-Verbindung geschlossen".into());
        }
        match tokio::time::timeout(timeout, res_rx).await {
            Ok(Ok(r)) => r,
            _ => {
                let mut inner = self.inner.lock().unwrap();
                inner.pending.remove(&(vast_id, id));
                Err("Timeout beim Warten auf Agent-Antwort".into())
            }
        }
    }

    /// cmd_result vom WS-Task auflösen.
    pub fn resolve(&self, vast_id: i64, id: u64, result: CommandResult) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(tx) = inner.pending.remove(&(vast_id, id)) {
            let _ = tx.send(result);
        }
    }

    // -------------------------------------------------- Terminal-Relay

    /// Dashboard registriert einen Kanal für term-Frames.
    pub fn register_term(&self, vast_id: i64, term_id: u64, tx: mpsc::Sender<serde_json::Value>) {
        let mut inner = self.inner.lock().unwrap();
        inner.terms.insert((vast_id, term_id), tx);
    }

    pub fn unregister_term(&self, vast_id: i64, term_id: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.terms.remove(&(vast_id, term_id));
    }

    /// term-Frames vom Agent → Dashboard weiterleiten.
    pub fn relay_term(&self, vast_id: i64, frame: serde_json::Value) {
        let term_id = frame.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let key = (vast_id, term_id);
        let mut inner = self.inner.lock().unwrap();
        if let Some(tx) = inner.terms.get(&key) {
            if tx.try_send(frame).is_err() {
                // Slow/disconnected browser: bound memory and end its terminal.
                inner.terms.remove(&key);
            }
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