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

    /// Persistiert + verteilt. Payload landet nur in der DB.
    pub fn emit(
        &self,
        db: &Db,
        kind: &str,
        slot_id: Option<i64>,
        instance_id: Option<i64>,
        reason: &str,
        payload: &serde_json::Value,
    ) {
        let id = db
            .add_event(kind, slot_id, instance_id, reason, payload)
            .unwrap_or(0);
        let _ = self.tx.send(SseEvent {
            id,
            ts: crate::db::now_iso(),
            kind: kind.to_string(),
            slot_id,
            instance_id,
            reason: reason.to_string(),
        });
        tracing::info!(kind, %reason, instance_id, slot_id, "event");
    }
}