//! Geteilter App-State: Config, DB, Bus, Agent-Hub, Traffic-Zähler, Vast.

use crate::config::Config;
use crate::db::Db;
use crate::events::EventBus;
use crate::hub::Hub;
use praxis_vast::Vast;
use std::collections::HashMap;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::Notify;

/// In-flight-Zähler + letzte Request-Zeit pro Slot.
#[derive(Clone, Default)]
pub struct Traffic {
    per_slot: Arc<Mutex<HashMap<i64, SlotTraffic>>>,
}

#[derive(Default, Clone, Copy)]
pub struct SlotTraffic {
    pub in_flight: u32,
    /// Unix-Sekunden des letzten Requests.
    pub last_request: i64,
}

impl Traffic {
    pub fn begin(&self, slot_id: i64) -> InFlightGuard {
        let mut map = self.per_slot.lock().unwrap();
        let e = map.entry(slot_id).or_default();
        e.in_flight = e.in_flight.saturating_add(1);
        e.last_request = chrono::Utc::now().timestamp();
        InFlightGuard { traffic: Arc::clone(&self.per_slot), slot_id }
    }

    pub fn mark(&self, slot_id: i64) {
        let mut map = self.per_slot.lock().unwrap();
        map.entry(slot_id).or_default().last_request = chrono::Utc::now().timestamp();
    }

    pub fn snapshot(&self, slot_id: i64) -> SlotTraffic {
        self.per_slot.lock().unwrap().get(&slot_id).copied().unwrap_or_default()
    }

    #[allow(dead_code)]
    pub fn all(&self) -> HashMap<i64, SlotTraffic> {
        self.per_slot.lock().unwrap().clone()
    }
}

/// Decrement beim Drop — deckt auch abgerissene Streams (Preemption) ab.
pub struct InFlightGuard {
    traffic: Arc<Mutex<HashMap<i64, SlotTraffic>>>,
    slot_id: i64,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut map = self.traffic.lock().unwrap();
        if let Some(e) = map.get_mut(&self.slot_id) {
            e.in_flight = e.in_flight.saturating_sub(1);
            e.last_request = chrono::Utc::now().timestamp();
        }
    }
}

/// `X-Router-Job-Id`-Batches (Bauplan §3.3): Praxis markiert zusammen-
/// hängende Requests (TTS-Sätze eines Antwortblocks) mit derselben Job-ID.
/// Der Slot gilt bis WINDOW_S nach dem LETZTEN Request des Jobs als busy —
/// Lücken zwischen den Sätzen lösen keinen Idle-Stop aus.
#[derive(Default)]
pub struct JobBatches(pub Mutex<HashMap<String, (i64, i64)>>);

impl JobBatches {
    pub const WINDOW_S: i64 = 300;

    pub fn touch(&self, slot_id: i64, job: &str) {
        if job.is_empty() || job.len() > 128 {
            return;
        }
        let expires = chrono::Utc::now().timestamp() + Self::WINDOW_S;
        self.0
            .lock()
            .unwrap()
            .insert(job.to_string(), (slot_id, expires));
    }

    /// Aktiver Batch für den Slot ( Bereinigung abgelaufener Einträge
    /// inklusive). Reason für compute_busy.
    pub fn active(&self, slot_id: i64) -> Option<String> {
        let now = chrono::Utc::now().timestamp();
        let mut map = self.0.lock().unwrap();
        map.retain(|_, (_, expires)| *expires > now);
        map.iter()
            .find(|(_, (slot, _))| *slot == slot_id)
            .map(|(job, _)| {
                let short: String = job.chars().take(8).collect();
                format!("batch {short}")
            })
    }
}

/// Schnellcache: aktives Ziel pro Slot (hot für jeden Proxy-Request).
/// slot_id -> (vast_id, nb_ip, healthy)
#[derive(Default)]
pub struct ActiveTargets(pub RwLock<HashMap<i64, Option<(i64, Option<String>, bool)>>>);

impl ActiveTargets {
    pub fn set(&self, slot_id: i64, vast_id: Option<i64>, nb_ip: Option<String>, healthy: bool) {
        self.0
            .write()
            .unwrap()
            .insert(slot_id, vast_id.map(|v| (v, nb_ip, healthy)));
    }
    pub fn get(&self, slot_id: i64) -> Option<(i64, Option<String>, bool)> {
        self.0.read().unwrap().get(&slot_id).cloned().flatten()
    }
}

pub struct App {
    pub cfg: Config,
    pub db: Db,
    pub events: EventBus,
    pub traffic: Traffic,
    pub jobs: JobBatches,
    pub targets: ActiveTargets,
    pub hub: Hub,
    pub vast: Arc<Mutex<Option<Vast>>>,
    /// Reconciler sofort aufwecken (Wake-Request etc.).
    pub reconcile_now: Notify,
    /// Offene STT-WS-Sessions (Router-lokales Proxying).
    pub stt_sessions: AtomicU32,
}

pub type SharedApp = Arc<App>;

/// State-Extractor-Newtype: Handler nehmen `app: AppCtx` und arbeiten per
/// Deref direkt auf `App` (`app.db`, `app.cfg`, ...). `app.0` = SharedApp.
#[derive(Clone)]
pub struct AppCtx(pub SharedApp);

impl std::ops::Deref for AppCtx {
    type Target = App;
    fn deref(&self) -> &App {
        &self.0
    }
}

#[axum::async_trait]
impl axum::extract::FromRequestParts<SharedApp> for AppCtx {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(_parts: &mut axum::http::request::Parts, state: &SharedApp) -> Result<Self, Self::Rejection> {
        Ok(AppCtx(state.clone()))
    }
}