//! SQLite (rusqlite, bundled). Sync-Wrapper mit Mutex — Operationen sind
//! kurz genug für v1 (Reconciler-Takt 30 s, Dashboard-Einzelabrufe).

use crate::config::Config;
use chrono::{DateTime, SecondsFormat, Utc};
use praxis_common::{Mode, Role};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

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
    pub busy: bool,
    pub busy_reason: String,
    pub min_bid: f64,
    pub bid_usd_h: f64,
    pub dph_total: f64,
    pub storage_usd_h: f64,
    pub created_at: String,
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
        Ok(Self(Arc::new(Mutex::new(conn))))
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
            "#,
        )?;
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
                                   state, min_bid, bid_usd_h, dph_total, storage_usd_h, created_at, label)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
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
        conn.execute(
            "UPDATE instances SET nb_ip=COALESCE(?2, nb_ip), healthy=?3, state=?4 WHERE vast_id=?1",
            params![vast_id, nb_ip, healthy as i64, state],
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

    pub fn update_instance_mode(&self, vast_id: i64, mode: Mode) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "UPDATE instances SET mode=?2 WHERE vast_id=?1",
            params![vast_id, serde_json::to_string(&mode)?.trim_matches('"')],
        )?;
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

    pub fn set_instance_state(&self, vast_id: i64, state: &str) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "UPDATE instances SET state=?2,
                stopped_since=CASE WHEN ?2='stopped' AND stopped_since IS NULL THEN ?3 ELSE stopped_since END,
                stopped_since=CASE WHEN ?2!='stopped' THEN NULL ELSE stopped_since END
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
        let conn = self.0.lock().unwrap();
        conn.execute(
            "UPDATE instances SET state='destroyed', destroyed_at=?2, intended_status='deleted' WHERE vast_id=?1",
            params![vast_id, now_iso()],
        )?;
        Ok(())
    }

    pub fn instances(&self, include_destroyed: bool) -> Vec<InstanceRow> {
        let conn = self.0.lock().unwrap();
        let sql = if include_destroyed {
            "SELECT * FROM instances ORDER BY created_at DESC"
        } else {
            "SELECT * FROM instances WHERE state != 'destroyed' ORDER BY created_at DESC"
        };
        let Ok(mut stmt) = conn.prepare(sql) else { return vec![] };
        stmt.query_map([], Self::row_from)
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
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
            busy: r.get::<_, i64>("busy")? != 0,
            busy_reason: r.get("busy_reason")?,
            min_bid: r.get("min_bid")?,
            bid_usd_h: r.get("bid_usd_h")?,
            dph_total: r.get("dph_total")?,
            storage_usd_h: r.get("storage_usd_h")?,
            created_at: r.get("created_at")?,
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
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO events(ts, kind, slot_id, instance_id, reason, payload_json) VALUES(?1,?2,?3,?4,?5,?6)",
            params![now_iso(), kind, slot_id, instance_id, reason, payload.to_string()],
        )?;
        Ok(conn.last_insert_rowid())
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

    // ------------------------------------------------------------ Metering

    pub fn meter(&self, date: &str, instance_id: i64, metered_usd: f64, storage_usd: f64) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO instance_meter(date, instance_id, metered_usd, storage_usd, last_metered_at)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(date, instance_id) DO UPDATE SET
               metered_usd = metered_usd + excluded.metered_usd,
               storage_usd = storage_usd + excluded.storage_usd,
               last_metered_at = excluded.last_metered_at",
            params![date, instance_id, metered_usd, storage_usd, now_iso()],
        )?;
        conn.execute(
            "INSERT INTO budget_days(date, metered_usd) VALUES(?1, ?2)
             ON CONFLICT(date) DO UPDATE SET metered_usd = metered_usd + excluded.metered_usd",
            params![date, metered_usd + storage_usd],
        )?;
        Ok(())
    }

    pub fn meter_traffic(&self, date: &str, traffic_usd: f64) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO budget_days(date, traffic_usd) VALUES(?1,?2)
             ON CONFLICT(date) DO UPDATE SET traffic_usd = traffic_usd + excluded.traffic_usd",
            params![date, traffic_usd],
        )?;
        Ok(())
    }

    pub fn spent_today(&self, date: &str) -> f64 {
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(metered_usd,0) + COALESCE(traffic_usd,0) FROM budget_days WHERE date=?1",
            params![date],
            |r| r.get::<_, f64>(0),
        )
        .unwrap_or(0.0)
    }

    pub fn spent_month(&self, month_prefix: &str) -> f64 {
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(SUM(COALESCE(metered_usd,0) + COALESCE(traffic_usd,0)),0)
               FROM budget_days WHERE date LIKE ?1",
            params![format!("{month_prefix}%")],
            |r| r.get::<_, f64>(0),
        )
        .unwrap_or(0.0)
    }

    pub fn set_reconciled(&self, date: &str, reconciled_usd: f64, credit_usd: f64) -> anyhow::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO budget_days(date, reconciled_usd, last_credit_usd) VALUES(?1,?2,?3)
             ON CONFLICT(date) DO UPDATE SET reconciled_usd=?2, last_credit_usd=?3",
            params![date, reconciled_usd, credit_usd],
        )?;
        Ok(())
    }

    pub fn last_credit(&self, date: &str) -> Option<f64> {
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT last_credit_usd FROM budget_days WHERE date=?1",
            params![date],
            |r| r.get::<_, Option<f64>>(0),
        )
        .unwrap_or(None)
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
            last_fail_at: r.get("last_fail_at")?,
            note: r.get("note")?,
        })
    }

    // ------------------------------------------------------------ Settings/Hashes

    #[allow(dead_code)]
    pub fn setting(&self, k: &str) -> Option<String> {
        let conn = self.0.lock().unwrap();
        conn.query_row("SELECT v FROM settings WHERE k=?1", params![k], |r| r.get(0)).ok()
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