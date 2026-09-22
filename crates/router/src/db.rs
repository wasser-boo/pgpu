//! SQLite (rusqlite, bundled). Sync-Wrapper mit Mutex — Operationen sind
//! kurz genug für v1 (Reconciler-Takt 30 s, Dashboard-Einzelabrufe).

mod performance_store;
mod billing_store;
pub use performance_store::PerformanceRow;

use crate::config::Config;
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use praxis_common::{Mode, Role};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Db(
    Arc<Mutex<Connection>>,
    /// Optional: SSE-Broadcast für JEDEN add_event (auch direkte DB-Rufer
    /// wie set_slot_desired_audited — sonst fehlen solche Events im
    /// Dashboard-Live-Feed). Wird in main nach dem EventBus-Bau injiziert.
    Arc<Mutex<Option<tokio::sync::broadcast::Sender<crate::events::SseEvent>>>>,
);

pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn parse_iso(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

#[derive(Debug, Clone)]
pub struct InstanceRow {
    pub vast_id: i64,
    pub slot_id: i64,
    pub role: Role,
    pub node_token: String,
    pub offer_id: i64,
    pub machine_id: i64,
    pub gpu_name: String,
    pub nb_ip: Option<String>,
    pub image: String,
    pub mode: Mode,
    pub lifecycle: String, // JSON
    pub pinned: bool,
    pub actual_status: String,
    pub intended_status: String,
    pub state: String,
    pub healthy: bool,
    /// Historical readiness; must survive preemption/stop unlike current health.
    pub ever_healthy: bool,
    pub busy: bool,
    pub busy_reason: String,
    pub min_bid: f64,
    pub bid_usd_h: f64,
    pub dph_total: f64,
    pub storage_usd_h: f64,
    pub created_at: String,
    /// Wann die Box zuletzt Healthy wurde (Idle-Uhr-Basis nach Warmup).
    pub healthy_since: Option<String>,
    pub stopped_since: Option<String>,
    pub destroyed_at: Option<String>,
    pub label: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EventRow {
    pub id: i64,
    pub ts: String,
    pub kind: String,
    pub slot_id: Option<i64>,
    pub instance_id: Option<i64>,
    pub reason: String,
    pub payload_json: String,
}

/// Host-Bilanz: Fails (Warmup-Tod/unreachable/Timeout) + Blacklist-Zustand.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MachineStatRow {
    pub machine_id: i64,
    pub fails: i64,
    pub blacklisted: bool,
    pub whitelisted: bool,
    pub last_fail_at: Option<String>,
    pub note: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MeterRow {
    pub date: String, // lokale Datumsangabe YYYY-MM-DD
    pub instance_id: i64,
    pub metered_usd: f64,
    pub storage_usd: f64,
    pub traffic_usd: f64,
    pub updated_at: String,
}

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        Self::migrate(&conn)?;
        Ok(Self(Arc::new(Mutex::new(conn)), Arc::new(Mutex::new(None))))
    }

    fn migrate(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            CREATE TABLE IF NOT EXISTS slots(
                id INTEGER PRIMARY KEY,
                role TEXT NOT NULL,
                name TEXT NOT NULL,
                pinned_instance INTEGER,
                active_instance INTEGER,
                desired_running INTEGER NOT NULL DEFAULT 0,
                last_swap_at TEXT,
                last_traffic_at TEXT
            );
            CREATE TABLE IF NOT EXISTS instances(
                vast_id INTEGER PRIMARY KEY,
                slot_id INTEGER NOT NULL,
                role TEXT NOT NULL,
                node_token TEXT NOT NULL UNIQUE,
                offer_id INTEGER NOT NULL DEFAULT 0,
                machine_id INTEGER NOT NULL DEFAULT 0,
                gpu_name TEXT NOT NULL DEFAULT '',
                nb_ip TEXT,
                image TEXT NOT NULL DEFAULT '',
                mode TEXT NOT NULL DEFAULT 'interruptible',
                lifecycle TEXT NOT NULL DEFAULT '{"kind":"auto"}',
                pinned INTEGER NOT NULL DEFAULT 0,
                actual_status TEXT NOT NULL DEFAULT 'loading',
                intended_status TEXT NOT NULL DEFAULT 'running',
                state TEXT NOT NULL DEFAULT 'requested',
                healthy INTEGER NOT NULL DEFAULT 0,
                busy INTEGER NOT NULL DEFAULT 0,
                busy_reason TEXT NOT NULL DEFAULT '',
                min_bid REAL NOT NULL DEFAULT 0,
                bid_usd_h REAL NOT NULL DEFAULT 0,
                dph_total REAL NOT NULL DEFAULT 0,
                storage_usd_h REAL NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                stopped_since TEXT,
                destroyed_at TEXT,
                label TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS budget_days(
                date TEXT PRIMARY KEY,
                metered_usd REAL NOT NULL DEFAULT 0,
                reconciled_usd REAL NOT NULL DEFAULT 0,
                traffic_usd REAL NOT NULL DEFAULT 0,
                last_credit_usd REAL
            );
            CREATE TABLE IF NOT EXISTS instance_meter(
                date TEXT NOT NULL,
                instance_id INTEGER NOT NULL,
                metered_usd REAL NOT NULL DEFAULT 0,
                storage_usd REAL NOT NULL DEFAULT 0,
                traffic_usd REAL NOT NULL DEFAULT 0,
                last_metered_at TEXT NOT NULL,
                PRIMARY KEY(date, instance_id)
            );
            CREATE TABLE IF NOT EXISTS provider_spend(
                period TEXT NOT NULL,
                instance_id INTEGER NOT NULL,
                provider_usd REAL NOT NULL,
                floor_usd REAL NOT NULL,
                meter_at_sync REAL NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY(period,instance_id)
            );
            CREATE TABLE IF NOT EXISTS events(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts TEXT NOT NULL,
                kind TEXT NOT NULL,
                slot_id INTEGER,
                instance_id INTEGER,
                reason TEXT NOT NULL DEFAULT '',
                payload_json TEXT NOT NULL DEFAULT '{}'
            );
            CREATE TABLE IF NOT EXISTS offers_cache(
                slot_id INTEGER PRIMARY KEY,
                ts TEXT NOT NULL,
                query TEXT NOT NULL DEFAULT '',
                offers_json TEXT NOT NULL DEFAULT '[]'
            );
            CREATE TABLE IF NOT EXISTS machine_stats(
                machine_id INTEGER PRIMARY KEY,
                fails INTEGER NOT NULL DEFAULT 0,
                blacklisted INTEGER NOT NULL DEFAULT 0,
                last_fail_at TEXT,
                note TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS assets_hash_cache(
                path TEXT PRIMARY KEY,
                sha256 TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS settings(k TEXT PRIMARY KEY, v TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS pending_operations(
                instance_id INTEGER PRIMARY KEY,
                operation TEXT NOT NULL CHECK(operation IN ('stop', 'destroy')),
                reason TEXT NOT NULL
            );
            "#,
        )?;
        // Do not hide real migration failures (disk full, permissions, corruption).
        let columns: Vec<String> = conn.prepare("PRAGMA table_info(instances)")?
            .query_map([], |r| r.get(1))?.collect::<rusqlite::Result<_>>()?;
        let tx = conn.unchecked_transaction()?;
        if !columns.iter().any(|c| c == "boot_started_at") {
            tx.execute_batch("ALTER TABLE instances ADD COLUMN boot_started_at TEXT;
                UPDATE instances SET boot_started_at=created_at;")?;
        }
        if !columns.iter().any(|c| c == "healthy_since") {
            tx.execute_batch("ALTER TABLE instances ADD COLUMN healthy_since TEXT;")?;
        }
        if !columns.iter().any(|c| c == "ever_healthy") {
            tx.execute_batch("ALTER TABLE instances ADD COLUMN ever_healthy INTEGER NOT NULL DEFAULT 0;
                UPDATE instances SET ever_healthy=1 WHERE healthy=1 OR healthy_since IS NOT NULL OR state='healthy';")?;
        }
        if !columns.iter().any(|c| c == "last_metered_at") {
            tx.execute_batch("ALTER TABLE instances ADD COLUMN last_metered_at TEXT;")?;
            // Historic totals from the old meter cannot be reconstructed safely.
            // Start existing contracts at migration time, never replay their lifetime.
            tx.execute("UPDATE instances SET last_metered_at=?1", params![now_iso()])?;
        }
        performance_store::migrate(&tx)?;
        tx.commit()?;
        Ok(())
    }

    // ------------------------------------------------------------ Slots

    pub fn init_slots(&self, cfg: &Config) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        for s in &cfg.slots {
            conn.execute(
                "INSERT INTO slots(id, role, name) VALUES(?1,?2,?3)
                 ON CONFLICT(id) DO UPDATE SET role=?2, name=?3",
                params![s.id, serde_json::to_string(&s.role)?.trim_matches('"'), s.name],
            )?;
        }
        Ok(())
    }

    pub fn set_slot_desired(&self, slot_id: i64, desired: bool) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute("UPDATE slots SET desired_running=?2 WHERE id=?1", params![slot_id, desired])?;
        Ok(())
    }

    /// Wie `set_slot_desired`, aber mit Audit-Event (wer hat's gesetzt?).
    /// Mystery vom 20.09.: desired flippte ohne sichtbaren Verursacher zurück
    /// auf true — ohne Trail ist das nicht debuggbar.
    pub fn set_slot_desired_audited(&self, slot_id: i64, desired: bool, source: &str) -> anyhow::Result<()> {
        let old = self.slot_desired(slot_id);
        self.set_slot_desired(slot_id, desired)?;
        if old != desired {
            self.add_event("slot_desired", Some(slot_id), None, &format!("desired {old} → {desired} ({source})"), &serde_json::json!({"desired": desired, "source": source}))?;
        }
        Ok(())
    }

    pub fn slot_desired(&self, slot_id: i64) -> bool {
        let conn = self.0.lock().unwrap();
        conn.query_row("SELECT desired_running FROM slots WHERE id=?1", params![slot_id], |r| {
            r.get::<_, i64>(0)
        })
        .map(|v| v != 0)
        .unwrap_or(false)
    }

    pub fn set_slot_pin(&self, slot_id: i64, instance: Option<i64>) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute("UPDATE slots SET pinned_instance=?2 WHERE id=?1", params![slot_id, instance])?;
        Ok(())
    }

    pub fn slot_pins(&self) -> Vec<(i64, Option<i64>)> {
        let conn = self.0.lock().unwrap();
        let mut stmt = match conn.prepare("SELECT id, pinned_instance FROM slots") {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    pub fn set_active_instance(&self, slot_id: i64, instance: Option<i64>) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "UPDATE slots SET active_instance=?2, last_swap_at=?3 WHERE id=?1",
            params![slot_id, instance, now_iso()],
        )?;
        Ok(())
    }

    /// Active-Austrag OHNE last_swap_at — Invariante-Heilung (zerstörte
    /// Instanz steht noch als Active), kein echter Hot-Swap.
    pub fn clear_active_instance(&self, slot_id: i64, vast_id: i64) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "UPDATE slots SET active_instance=NULL WHERE id=?1 AND active_instance=?2",
            params![slot_id, vast_id],
        )?;
        Ok(())
    }

    pub fn active_instance(&self, slot_id: i64) -> Option<i64> {
        let conn = self.0.lock().unwrap();
        conn.query_row("SELECT active_instance FROM slots WHERE id=?1", params![slot_id], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .unwrap_or(None)
    }

    pub fn mark_traffic(&self, slot_id: i64) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute("UPDATE slots SET last_traffic_at=?2 WHERE id=?1", params![slot_id, now_iso()]);
    }

    pub fn last_traffic(&self, slot_id: i64) -> Option<DateTime<Utc>> {
        let conn = self.0.lock().unwrap();
        conn.query_row("SELECT last_traffic_at FROM slots WHERE id=?1", params![slot_id], |r| {
            r.get::<_, Option<String>>(0)
        })
        .unwrap_or(None)
        .and_then(|s| parse_iso(&s))
    }

    pub fn last_swap(&self, slot_id: i64) -> Option<DateTime<Utc>> {
        let conn = self.0.lock().unwrap();
        conn.query_row("SELECT last_swap_at FROM slots WHERE id=?1", params![slot_id], |r| {
            r.get::<_, Option<String>>(0)
        })
        .unwrap_or(None)
        .and_then(|s| parse_iso(&s))
    }

    // ------------------------------------------------------------ Instances

    pub fn insert_instance(&self, row: &InstanceRow) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO instances(vast_id, slot_id, role, node_token, offer_id, machine_id, gpu_name,
                                   image, mode, lifecycle, pinned, actual_status, intended_status,
                                   state, min_bid, bid_usd_h, dph_total, storage_usd_h, created_at, label, last_metered_at, ever_healthy)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?19,?21)",
            params![
                row.vast_id,
                row.slot_id,
                serde_json::to_string(&row.role)?.trim_matches('"'),
                row.node_token,
                row.offer_id,
                row.machine_id,
                row.gpu_name,
                row.image,
                serde_json::to_string(&row.mode)?.trim_matches('"'),
                row.lifecycle,
                row.pinned as i64,
                row.actual_status,
                row.intended_status,
                row.state,
                row.min_bid,
                row.bid_usd_h,
                row.dph_total,
                row.storage_usd_h,
                row.created_at,
                row.label,
                row.ever_healthy || row.healthy,
            ],
        )?;
        Ok(())
    }

    pub fn update_instance_vast(&self, vast_id: i64, actual_status: &str, min_bid: f64, dph_total: f64, machine_id: i64, gpu_name: &str) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "UPDATE instances SET actual_status=?2, min_bid=?3, dph_total=?4, machine_id=?5, gpu_name=?6 WHERE vast_id=?1",
            params![vast_id, actual_status, min_bid, dph_total, machine_id, gpu_name],
        )?;
        Ok(())
    }

    pub fn update_instance_agent(&self, vast_id: i64, nb_ip: Option<&str>, healthy: bool, state: &str) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        // Heartbeats dürfen nur aktive Zustände verschieben: Ein PREEMPTED/STOPPED/
        // DESTROYED bleibt terminal — der Agent meldet nach dem Vast-Tod oft noch
        // Sekunden weiter und würde sonst "preempted → booting" zurücksetzen
        // (beobachtet 2026-09-20: 2 PREEMPTED-Events + Doppel-Fail für dieselbe
        // Instanz, Blacklist feuerte dadurch nach EINEM echten Vorfall).
        // healthy_since: Idle-Uhr-Basis — nur beim echten Übergang setzen.
        conn.execute(
            "UPDATE instances SET nb_ip=COALESCE(?2, nb_ip), healthy=?3, state=?4,
                ever_healthy=CASE WHEN ?3=1 THEN 1 ELSE ever_healthy END,
                healthy_since=CASE
                    WHEN ?4='healthy' AND healthy_since IS NULL THEN ?5
                    WHEN ?4!='healthy' THEN NULL
                    ELSE healthy_since END
             WHERE vast_id=?1 AND state IN ('requested','provisioning','booting','agent_connected','healthy','unreachable')",
            params![vast_id, nb_ip, healthy as i64, state, now_iso()],
        )?;
        Ok(())
    }

    pub fn update_instance_busy(&self, vast_id: i64, busy: bool, reason: &str) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "UPDATE instances SET busy=?2, busy_reason=?3 WHERE vast_id=?1",
            params![vast_id, busy as i64, reason],
        )?;
        Ok(())
    }

    pub fn set_instance_bid(&self, vast_id: i64, price: f64) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute("UPDATE instances SET bid_usd_h=?2 WHERE vast_id=?1", params![vast_id, price])?;
        Ok(())
    }

    pub fn update_instance_lifecycle(&self, vast_id: i64, lifecycle: &str) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute("UPDATE instances SET lifecycle=?2 WHERE vast_id=?1", params![vast_id, lifecycle])?;
        Ok(())
    }

    pub fn update_instance_pinned(&self, vast_id: i64, pinned: bool) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute("UPDATE instances SET pinned=?2 WHERE vast_id=?1", params![vast_id, pinned as i64])?;
        Ok(())
    }

    pub fn boot_started_at(&self, vast_id: i64) -> anyhow::Result<Option<DateTime<Utc>>> {
        let value: Option<String> = self.0.lock().unwrap().query_row(
            "SELECT COALESCE(boot_started_at, created_at) FROM instances WHERE vast_id=?1",
            params![vast_id], |row| row.get(0)).optional()?;
        Ok(value.as_deref().and_then(parse_iso))
    }

    pub fn set_instance_state(&self, vast_id: i64, state: &str) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "UPDATE instances SET state=?2,
                boot_started_at=CASE WHEN ?2='booting' AND state NOT IN ('requested','provisioning','booting','agent_connected')
                    THEN ?3 ELSE COALESCE(boot_started_at, created_at) END,
                stopped_since=CASE WHEN ?2='stopped' THEN COALESCE(stopped_since, ?3) ELSE NULL END,
                healthy_since=CASE WHEN ?2='healthy' THEN COALESCE(healthy_since, ?3) ELSE NULL END,
                healthy=CASE WHEN ?2='healthy' THEN 1 ELSE 0 END,
                ever_healthy=CASE WHEN ?2='healthy' THEN 1 ELSE ever_healthy END,
                busy=CASE WHEN ?2 IN ('stopped','preempted','unreachable','destroyed') THEN 0 ELSE busy END
             WHERE vast_id=?1",
            params![vast_id, state, now_iso()],
        )?;
        Ok(())
    }

    pub fn set_instance_intended(&self, vast_id: i64, intended: &str) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "UPDATE instances SET intended_status=?2 WHERE vast_id=?1",
            params![vast_id, intended],
        )?;
        Ok(())
    }

    pub fn mark_destroyed(&self, vast_id: i64) -> anyhow::Result<()> {
        let mut conn = self.0.lock().unwrap();
        let conn = conn.transaction()?;
        // Active-Instanz sofort freimachen: sonst bleibt der Proxy bis zum
        // nächsten Flip auf der toten Box hängen und 502t statt 503+Wake zu
        // antworten (20.09.: instance_gone ließ active=51735121 stehen).
        conn.execute(
            "UPDATE slots SET active_instance=NULL WHERE active_instance=?1",
            params![vast_id],
        )?;
        conn.execute(
            "UPDATE instances SET state='destroyed', destroyed_at=?2, intended_status='deleted',
                healthy=0, busy=0, busy_reason='' WHERE vast_id=?1",
            params![vast_id, now_iso()],
        )?;
        conn.execute("UPDATE slots SET pinned_instance=NULL WHERE pinned_instance=?1", params![vast_id])?;
        conn.execute("DELETE FROM pending_operations WHERE instance_id=?1", params![vast_id])?;
        conn.commit()?;
        Ok(())
    }

    /// Persist intent before provider I/O. A pending destroy cannot be weakened to stop.
    pub fn queue_operation(&self, id: i64, operation: &str, reason: &str) -> anyhow::Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO pending_operations(instance_id,operation,reason) VALUES(?1,?2,?3)
             ON CONFLICT(instance_id) DO UPDATE SET operation=?2, reason=?3
             WHERE pending_operations.operation!='destroy'",
            params![id, operation, reason],
        )?;
        tx.execute("UPDATE instances SET intended_status=CASE
            WHEN (SELECT operation FROM pending_operations WHERE instance_id=?1)='destroy'
            THEN 'deleted' ELSE 'stopped' END WHERE vast_id=?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn pending_operations(&self) -> anyhow::Result<Vec<(i64, String, String)>> {
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare("SELECT instance_id,operation,reason FROM pending_operations ORDER BY instance_id")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn complete_stop(&self, id: i64) -> anyhow::Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("UPDATE instances SET state='stopped', intended_status='stopped',
            healthy=0, busy=0, healthy_since=NULL, stopped_since=COALESCE(stopped_since,?2)
            WHERE vast_id=?1", params![id, now_iso()])?;
        tx.execute("DELETE FROM pending_operations WHERE instance_id=?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn instances(&self, include_destroyed: bool) -> Vec<InstanceRow> {
        self.try_instances(include_destroyed).unwrap_or_default()
    }

    /// Ownership-sensitive callers must not act on a partial local inventory.
    pub fn try_instances(&self, include_destroyed: bool) -> anyhow::Result<Vec<InstanceRow>> {
        let conn = self.0.lock().unwrap();
        let sql = if include_destroyed {
            "SELECT * FROM instances ORDER BY created_at DESC"
        } else {
            "SELECT * FROM instances WHERE state != 'destroyed' ORDER BY created_at DESC"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], Self::row_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn instance(&self, vast_id: i64) -> Option<InstanceRow> {
        let conn = self.0.lock().unwrap();
        conn.query_row("SELECT * FROM instances WHERE vast_id=?1", params![vast_id], Self::row_from)
            .ok()
    }

    pub fn instance_by_token(&self, token: &str) -> Option<InstanceRow> {
        let conn = self.0.lock().unwrap();
        conn.query_row("SELECT * FROM instances WHERE node_token=?1 AND state!='destroyed'", params![token], Self::row_from)
            .ok()
    }

    fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<InstanceRow> {
        Ok(InstanceRow {
            vast_id: r.get("vast_id")?,
            slot_id: r.get("slot_id")?,
            role: match r.get::<_, String>("role")? {
                s if s == "media" => Role::Media,
                _ => Role::Llm,
            },
            node_token: r.get("node_token")?,
            offer_id: r.get("offer_id")?,
            machine_id: r.get("machine_id")?,
            gpu_name: r.get("gpu_name")?,
            nb_ip: r.get("nb_ip")?,
            image: r.get("image")?,
            mode: match r.get::<_, String>("mode")? {
                s if s == "on_demand" => Mode::OnDemand,
                s if s == "manual" => Mode::Manual,
                _ => Mode::Interruptible,
            },
            lifecycle: r.get("lifecycle")?,
            pinned: r.get::<_, i64>("pinned")? != 0,
            actual_status: r.get("actual_status")?,
            intended_status: r.get("intended_status")?,
            state: r.get("state")?,
            healthy: r.get::<_, i64>("healthy")? != 0,
            ever_healthy: r.get::<_, i64>("ever_healthy")? != 0,
            busy: r.get::<_, i64>("busy")? != 0,
            busy_reason: r.get("busy_reason")?,
            min_bid: r.get("min_bid")?,
            bid_usd_h: r.get("bid_usd_h")?,
            dph_total: r.get("dph_total")?,
            storage_usd_h: r.get("storage_usd_h")?,
            created_at: r.get("created_at")?,
            healthy_since: r.get("healthy_since")?,
            stopped_since: r.get("stopped_since")?,
            destroyed_at: r.get("destroyed_at")?,
            label: r.get("label")?,
        })
    }

    // ------------------------------------------------------------ Events

    pub fn add_event(
        &self,
        kind: &str,
        slot_id: Option<i64>,
        instance_id: Option<i64>,
        reason: &str,
        payload: &serde_json::Value,
    ) -> anyhow::Result<i64> {
        let id = {
            let conn = self.0.lock().unwrap();
            conn.execute(
                "INSERT INTO events(ts, kind, slot_id, instance_id, reason, payload_json) VALUES(?1,?2,?3,?4,?5,?6)",
                params![now_iso(), kind, slot_id, instance_id, reason, payload.to_string()],
            )?;
            conn.last_insert_rowid()
        };
        // Live-Feed: broadcast ist non-blocking, ohne Receiver ist send() ein No-Op.
        if let Some(tx) = self.1.lock().unwrap().as_ref() {
            let _ = tx.send(crate::events::SseEvent {
                id,
                ts: now_iso(),
                kind: kind.to_string(),
                slot_id,
                instance_id,
                reason: reason.to_string(),
            });
        }
        Ok(id)
    }

    /// SSE-Sender injizieren (main, nach EventBus-Bau).
    pub fn set_sse_sender(&self, tx: tokio::sync::broadcast::Sender<crate::events::SseEvent>) {
        *self.1.lock().unwrap() = Some(tx);
    }

    pub fn events(&self, limit: i64, instance_id: Option<i64>) -> Vec<EventRow> {
        let conn = self.0.lock().unwrap();
        let (sql, p): (&str, Vec<Box<dyn rusqlite::ToSql>>) = match instance_id {
            Some(id) => (
                "SELECT * FROM events WHERE instance_id=?1 ORDER BY id DESC LIMIT ?2",
                vec![Box::new(id), Box::new(limit)],
            ),
            None => ("SELECT * FROM events ORDER BY id DESC LIMIT ?1", vec![Box::new(limit)]),
        };
        let Ok(mut stmt) = conn.prepare(sql) else { return vec![] };
        let params_ref: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
        stmt.query_map(params_ref.as_slice(), |r| {
            Ok(EventRow {
                id: r.get("id")?,
                ts: r.get("ts")?,
                kind: r.get("kind")?,
                slot_id: r.get("slot_id")?,
                instance_id: r.get("instance_id")?,
                reason: r.get("reason")?,
                payload_json: r.get("payload_json")?,
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    /// Zeitstempel des jüngsten Events dieser Art (Alert-Dedupe).
    pub fn last_event_of_kind(&self, kind: &str) -> Option<DateTime<Utc>> {
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT ts FROM events WHERE kind=?1 ORDER BY id DESC LIMIT 1",
            params![kind],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|s| parse_iso(&s))
    }

    // ------------------------------------------------------------ Metering

    /// Integrate last OBSERVED provider rates, not heartbeat age or desired state.
    /// Cursor and both ledgers commit together, including across restarts and midnight.
    /// Provider state during an outage is unknown: this is an estimate, not an invoice.
    pub fn meter_until(&self, now: DateTime<Utc>, tz: chrono_tz::Tz) -> anyhow::Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        let rows: Vec<(i64, String, f64, f64)> = {
            let mut stmt = tx.prepare("SELECT vast_id, COALESCE(last_metered_at,created_at),
                CASE WHEN actual_status='running' THEN
                    CASE WHEN mode='interruptible' THEN bid_usd_h ELSE dph_total END
                    ELSE 0 END, storage_usd_h FROM instances WHERE state!='destroyed'")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for (id, cursor, gpu_rate, storage_rate) in rows {
            anyhow::ensure!(gpu_rate.is_finite() && gpu_rate >= 0.0 && storage_rate.is_finite() && storage_rate >= 0.0, "invalid metering rate for {id}");
            let mut from = parse_iso(&cursor).ok_or_else(|| anyhow::anyhow!("invalid meter cursor for {id}"))?;
            if now <= from { continue; } // backward clock: never move cursor backwards
            while from < now {
                let date = from.with_timezone(&tz).date_naive();
                let mut midnight = date.succ_opt().ok_or_else(|| anyhow::anyhow!("date overflow"))?.and_hms_opt(0,0,0).unwrap();
                // Some zones skip local midnight (or even a day). First valid minute
                // of the next local date is the boundary; earliest handles DST folds.
                let boundary = loop {
                    if let Some(t) = tz.from_local_datetime(&midnight).earliest() { break t.with_timezone(&Utc); }
                    midnight += chrono::Duration::minutes(1);
                };
                let until = now.min(boundary);
                let hours = (until - from).num_milliseconds() as f64 / 3_600_000.0;
                let gpu = gpu_rate * hours;
                let storage = storage_rate * hours;
                let date = date.to_string();
                tx.execute("INSERT INTO instance_meter(date,instance_id,metered_usd,storage_usd,last_metered_at)
                    VALUES(?1,?2,?3,?4,?5) ON CONFLICT(date,instance_id) DO UPDATE SET
                    metered_usd=metered_usd+excluded.metered_usd,
                    storage_usd=storage_usd+excluded.storage_usd, last_metered_at=excluded.last_metered_at",
                    params![date,id,gpu,storage,until.to_rfc3339()])?;
                tx.execute("INSERT INTO budget_days(date,metered_usd) VALUES(?1,?2)
                    ON CONFLICT(date) DO UPDATE SET metered_usd=metered_usd+excluded.metered_usd",
                    params![date,gpu+storage])?;
                from = until;
            }
            tx.execute("UPDATE instances SET last_metered_at=?2 WHERE vast_id=?1", params![id,now.to_rfc3339()])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Safety-critical callers must propagate read errors instead of assuming $0.
    pub fn budget_totals(&self, date: &str) -> anyhow::Result<(f64, f64)> {
        let conn = self.0.lock().unwrap();
        Ok((billing_store::total(&conn,date)?,billing_store::total(&conn,&date[..7])?))
    }

    pub fn spent_today(&self, date: &str) -> f64 {
        billing_store::total(&self.0.lock().unwrap(),date).unwrap_or(0.0)
    }

    pub fn spent_month(&self, month_prefix: &str) -> f64 {
        billing_store::total(&self.0.lock().unwrap(),month_prefix).unwrap_or(0.0)
    }

    /// Accumulate balance deltas atomically. This is diagnostic only: top-ups,
    /// refunds and non-pgpu contracts prevent balance changes being an invoice.
    #[allow(dead_code)] // Legacy diagnostic, deliberately not used for budget enforcement.
    pub fn record_credit(&self, date: &str, credit: f64) -> anyhow::Result<f64> {
        anyhow::ensure!(credit.is_finite(), "invalid provider balance");
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("INSERT INTO budget_days(date,last_credit_usd) VALUES(?1,?2)
            ON CONFLICT(date) DO UPDATE SET
            reconciled_usd=reconciled_usd+MAX(COALESCE(last_credit_usd,?2)-?2,0), last_credit_usd=?2",
            params![date,credit])?;
        let total = tx.query_row("SELECT reconciled_usd FROM budget_days WHERE date=?1", params![date], |r| r.get(0))?;
        tx.commit()?;
        Ok(total)
    }

    // ------------------------------------------------------------ Offers cache

    pub fn cache_offers(&self, slot_id: i64, query: &str, offers: &str) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO offers_cache(slot_id, ts, query, offers_json) VALUES(?1,?2,?3,?4)
             ON CONFLICT(slot_id) DO UPDATE SET ts=?2, query=?3, offers_json=?4",
            params![slot_id, now_iso(), query, offers],
        )?;
        Ok(())
    }

    pub fn invalidate_offers(&self) -> anyhow::Result<()> {
        self.0.lock().unwrap().execute("DELETE FROM offers_cache", [])?;
        Ok(())
    }

    pub fn cached_offers(&self, slot_id: i64) -> Option<(String, String)> {
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT ts, offers_json FROM offers_cache WHERE slot_id=?1",
            params![slot_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()
        .ok()
        .flatten()
    }

    // ------------------------------------------------------------ Machines

    /// Maschinen-Fail zählen (warmup-Tod, unreachable, Warmup-Timeout).
    /// Liefert den neuen Zählerstand.
    pub fn record_machine_fail(&self, machine_id: i64, kind: &str) -> anyhow::Result<i64> {
        if machine_id == 0 {
            return Ok(0); // ohne machine_id nicht attribuierbar
        }
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO machine_stats(machine_id, fails, last_fail_at, note) VALUES(?1, 1, ?2, ?3)
             ON CONFLICT(machine_id) DO UPDATE SET
               fails = fails + 1, last_fail_at = ?2,
               note = CASE WHEN ?3 = '' THEN note ELSE ?3 END",
            params![machine_id, now_iso(), kind],
        )?;
        Ok(conn.query_row(
            "SELECT fails FROM machine_stats WHERE machine_id=?1",
            params![machine_id],
            |r| r.get::<_, i64>(0),
        )?)
    }

    pub fn set_machine_blacklist(&self, machine_id: i64, blacklisted: bool, note: &str) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO machine_stats(machine_id, fails, blacklisted, note) VALUES(?1, 0, ?2, ?3)
             ON CONFLICT(machine_id) DO UPDATE SET blacklisted=?2,
               note = CASE WHEN ?3 = '' THEN note ELSE ?3 END",
            params![machine_id, blacklisted as i64, note],
        )?;
        Ok(())
    }

    pub fn machine_stat(&self, machine_id: i64) -> Option<MachineStatRow> {
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT * FROM machine_stats WHERE machine_id=?1",
            params![machine_id],
            Self::machine_row_from,
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn machine_stats(&self) -> Vec<MachineStatRow> {
        let conn = self.0.lock().unwrap();
        let Ok(mut stmt) =
            conn.prepare("SELECT * FROM machine_stats ORDER BY blacklisted DESC, fails DESC, last_fail_at DESC")
        else {
            return vec![];
        };
        stmt.query_map([], Self::machine_row_from)
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    fn machine_row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<MachineStatRow> {
        Ok(MachineStatRow {
            machine_id: r.get("machine_id")?,
            fails: r.get("fails")?,
            blacklisted: r.get::<_, i64>("blacklisted")? != 0,
            whitelisted: r.get::<_, i64>("whitelisted")? != 0,
            last_fail_at: r.get("last_fail_at")?,
            note: r.get("note")?,
        })
    }

    // ------------------------------------------------------------ Settings/Hashes

    #[allow(dead_code)]
    pub fn setting(&self, k: &str) -> Option<String> {
        self.try_setting(k).ok().flatten()
    }

    pub fn try_setting(&self, k: &str) -> anyhow::Result<Option<String>> {
        let conn = self.0.lock().unwrap();
        Ok(conn.query_row("SELECT v FROM settings WHERE k=?1", params![k], |r| r.get(0)).optional()?)
    }

    #[allow(dead_code)]
    pub fn set_setting(&self, k: &str, v: &str) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO settings(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=?2",
            params![k, v],
        )?;
        Ok(())
    }

    /// Atomically reserve a persisted interval (manual notification tests/cleanup).
    pub fn claim_interval(&self, key: &str, now: i64, interval_s: i64) -> anyhow::Result<bool> {
        anyhow::ensure!(interval_s > 0, "invalid interval");
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let last: Option<String> = tx.query_row("SELECT v FROM settings WHERE k=?1", params![key], |r| r.get(0)).optional()?;
        if let Some(last) = last {
            let last: i64 = last.parse()?;
            if now >= last && now.saturating_sub(last) < interval_s { return Ok(false); }
        }
        tx.execute("INSERT INTO settings(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=?2", params![key,now.to_string()])?;
        tx.commit()?;
        Ok(true)
    }

    /// Auto-Miete-Schalter (Default an): aus = Policy erzeugt keine
    /// Create/Start-Aktionen mehr; Proxy-Wake antwortet sofort 503.
    pub fn auto_rent_enabled(&self) -> bool {
        self.setting("auto_rent").map(|v| v != "0").unwrap_or(true)
    }

    pub fn set_auto_rent(&self, enabled: bool) -> anyhow::Result<()> {
        self.set_setting("auto_rent", if enabled { "1" } else { "0" })
    }

    pub fn asset_hash(&self, path: &str) -> Option<String> {
        let conn = self.0.lock().unwrap();
        conn.query_row("SELECT sha256 FROM assets_hash_cache WHERE path=?1", params![path], |r| {
            r.get(0)
        })
        .ok()
    }

    pub fn set_asset_hash(&self, path: &str, sha: &str) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO assets_hash_cache(path, sha256) VALUES(?1,?2)
             ON CONFLICT(path) DO UPDATE SET sha256=?2",
            params![path, sha],
        )?;
        Ok(())
    }
}