//! Durable state audit and notification cursors. Triggers capture every real
//! persisted transition, including brief ones between reconciler polls.
use super::{Db, EventRow};
use rusqlite::{params, Connection};

pub(super) fn migrate(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(r#"
        CREATE TRIGGER IF NOT EXISTS notification_instance_state
        AFTER UPDATE OF state ON instances WHEN OLD.state IS NOT NEW.state
        BEGIN
            INSERT INTO events(ts,kind,slot_id,instance_id,reason,payload_json)
            VALUES(strftime('%Y-%m-%dT%H:%M:%SZ','now'),'instance_state_changed',NEW.slot_id,NEW.vast_id,
                'Status ' || OLD.state || ' -> ' || NEW.state,
                json_object('from',OLD.state,'to',NEW.state,'gpu',NEW.gpu_name,'mode',NEW.mode,
                    'actual_status',NEW.actual_status,'intended_status',NEW.intended_status,
                    'compute_usd_h',CASE WHEN NEW.mode='interruptible' THEN NEW.bid_usd_h ELSE NEW.dph_total END,
                    'storage_usd_h',NEW.storage_usd_h));
        END;
        CREATE TRIGGER IF NOT EXISTS notification_slot_backend
        AFTER UPDATE OF active_instance ON slots WHEN OLD.active_instance IS NOT NEW.active_instance
        BEGIN
            INSERT INTO events(ts,kind,slot_id,instance_id,reason,payload_json)
            VALUES(strftime('%Y-%m-%dT%H:%M:%SZ','now'),'slot_backend_changed',NEW.id,NEW.active_instance,
                'Backend-Zuordnung geaendert',json_object('previous',OLD.active_instance,'next',NEW.active_instance));
        END;
    "#)?;
    Ok(())
}

impl Db {
    /// First install starts at the current tail, not years of historical events.
    pub fn initialize_notifications(&self, now: i64) -> anyhow::Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("INSERT OR IGNORE INTO settings(k,v) SELECT 'notification_event_cursor',CAST(COALESCE(MAX(id),0) AS TEXT) FROM events",[])?;
        tx.execute(
            "INSERT OR IGNORE INTO settings(k,v) VALUES('spend_summary_last_attempt',?1)",
            params![now.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// At-most-once dispatch reservation; transport owns bounded transient retries.
    /// Only controlled lifecycle events are selected, never webhook result events.
    pub fn claim_notification_events(&self) -> anyhow::Result<Vec<EventRow>> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let cursor: String = tx.query_row(
            "SELECT v FROM settings WHERE k='notification_event_cursor'",
            [],
            |r| r.get(0),
        )?;
        let cursor: i64 = cursor.parse()?;
        let events = {
            let mut stmt=tx.prepare("SELECT * FROM events WHERE id>?1 AND kind IN ('instance_state_changed','slot_backend_changed') ORDER BY id LIMIT 32")?;
            let rows = stmt.query_map(params![cursor], |r| {
                Ok(EventRow {
                    id: r.get("id")?,
                    ts: r.get("ts")?,
                    kind: r.get("kind")?,
                    slot_id: r.get("slot_id")?,
                    instance_id: r.get("instance_id")?,
                    reason: r.get("reason")?,
                    payload_json: r.get("payload_json")?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let last = if let Some(event) = events.last() {
            event.id
        } else {
            tx.query_row("SELECT COALESCE(MAX(id),0) FROM events", [], |r| {
                r.get::<_, i64>(0)
            })?
            .max(cursor)
        };
        if last != cursor {
            tx.execute(
                "UPDATE settings SET v=?1 WHERE k='notification_event_cursor'",
                params![last.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(events)
    }

    /// Returns the previous value only when an already-known derived state changes.
    pub fn swap_notification_state(
        &self,
        key: &str,
        value: &str,
    ) -> anyhow::Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let old: Option<String> = tx
            .query_row("SELECT v FROM settings WHERE k=?1", params![key], |r| {
                r.get(0)
            })
            .optional()?;
        if old.as_deref() != Some(value) {
            tx.execute(
                "INSERT INTO settings(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=?2",
                params![key, value],
            )?;
        }
        tx.commit()?;
        Ok(old.filter(|old| old != value))
    }
}
