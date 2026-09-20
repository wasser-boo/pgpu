//! In-Memory-Event-Bus (tokio broadcast) für SSE + Persistenz in SQLite.

use crate::db::Db;
use serde::Serialize;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize)]
pub struct SseEvent {
    pub id: i64,
    pub ts: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<i64>,
    pub reason: String,
}

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<SseEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SseEvent> {
        self.tx.subscribe()
    }

    /// Sender für Db::add_event (Live-Feed auch für direkte DB-Event-Rufer).
    pub fn sender(&self) -> broadcast::Sender<SseEvent> {
        self.tx.clone()
    }

    /// Persistiert + verteilt. Payload landet nur in der DB.
    /// Broadcast macht db.add_event selbst (injizierter Sender) — hier nicht
    /// erneut senden (sonst Duplikate im Live-Feed).
    pub fn emit(
        &self,
        db: &Db,
        kind: &str,
        slot_id: Option<i64>,
        instance_id: Option<i64>,
        reason: &str,
        payload: &serde_json::Value,
    ) {
        let _ = db
            .add_event(kind, slot_id, instance_id, reason, payload)
            .unwrap_or(0);
        tracing::info!(kind, %reason, instance_id, slot_id, "event");
    }
}