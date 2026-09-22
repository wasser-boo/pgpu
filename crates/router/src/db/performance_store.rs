//! Durable measurements + lifetime aggregates. Raw retention never deletes the GPU catalogue.
use super::{Db, now_iso};
use praxis_common::performance::Metrics;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(super) fn migrate(tx: &rusqlite::Transaction<'_>) -> anyhow::Result<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS performance_samples(
        id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, slot_id INTEGER NOT NULL,
        instance_id INTEGER NOT NULL, machine_id INTEGER NOT NULL, gpu_name TEXT NOT NULL,
        profile TEXT NOT NULL, workload_key TEXT NOT NULL, allocation TEXT NOT NULL DEFAULT '', image TEXT NOT NULL, source TEXT NOT NULL,
        success INTEGER NOT NULL, price_usd_h REAL NOT NULL, metrics_json TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS performance_lookup ON performance_samples(workload_key, machine_id, ts);
        CREATE INDEX IF NOT EXISTS performance_ts ON performance_samples(ts);
        CREATE INDEX IF NOT EXISTS performance_slot ON performance_samples(slot_id,id);
        CREATE INDEX IF NOT EXISTS performance_slot_age ON performance_samples(slot_id,ts);
        CREATE TABLE IF NOT EXISTS performance_catalogue(
            catalogue_key TEXT PRIMARY KEY, data TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS benchmark_runs(
            instance_id INTEGER NOT NULL, workload_key TEXT NOT NULL, attempted_at INTEGER NOT NULL,
            PRIMARY KEY(instance_id,workload_key));
        CREATE TABLE IF NOT EXISTS machine_offers(
            machine_id INTEGER PRIMARY KEY, seen_at TEXT NOT NULL, offer_json TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS rental_facts(instance_id INTEGER PRIMARY KEY, offer_json TEXT NOT NULL);")?;
    let sample_columns: Vec<String> = tx.prepare("PRAGMA table_info(performance_samples)")?.query_map([], |r|r.get(1))?.collect::<rusqlite::Result<_>>()?;
    if !sample_columns.iter().any(|c|c=="allocation") {
        tx.execute_batch("ALTER TABLE performance_samples ADD COLUMN allocation TEXT NOT NULL DEFAULT '';")?;
    }
    let columns: Vec<String> = tx.prepare("PRAGMA table_info(machine_stats)")?.query_map([], |r| r.get(1))?.collect::<rusqlite::Result<_>>()?;
    if !columns.iter().any(|c| c == "whitelisted") {
        tx.execute_batch("ALTER TABLE machine_stats ADD COLUMN whitelisted INTEGER NOT NULL DEFAULT 0;")?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceRow {
    pub id: i64,
    pub ts: i64,
    pub slot_id: i64,
    pub instance_id: i64,
    pub machine_id: i64,
    pub gpu_name: String,
    pub profile: String,
    pub workload_key: String,
    pub allocation: String,
    pub image: String,
    pub source: String,
    pub success: bool,
    pub price_usd_h: f64,
    pub metrics: Metrics,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogueRow {
    pub slot_id: i64,
    pub machine_id: i64,
    pub gpu_name: String,
    pub profile: String,
    pub workload_key: String,
    #[serde(default)]
    pub allocation: String,
    pub model: String,
    pub image: String,
    pub source: String,
    pub samples: u64,
    pub successful: u64,
    pub last_seen: i64,
    pub sums: BTreeMap<String, f64>,
    pub counts: BTreeMap<String, u64>,
}
impl CatalogueRow {
    pub fn means(&self) -> BTreeMap<String, f64> {
        self.sums.iter().filter_map(|(key, sum)| self.counts.get(key).filter(|n| **n > 0).map(|n| (key.clone(), sum / *n as f64))).collect()
    }
}

impl Db {
    pub fn record_performance(&self, row: &PerformanceRow, retention_days: u32, max_samples: usize) -> anyhow::Result<()> {
        row.metrics.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(["usage", "benchmark"].contains(&row.source.as_str()) && row.price_usd_h.is_finite() && row.price_usd_h >= 0.0, "invalid measurement context");
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("INSERT INTO performance_samples(ts,slot_id,instance_id,machine_id,gpu_name,profile,workload_key,allocation,image,source,success,price_usd_h,metrics_json)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![row.ts,row.slot_id,row.instance_id,row.machine_id,row.gpu_name,row.profile,row.workload_key,row.allocation,row.image,row.source,row.success,row.price_usd_h,serde_json::to_string(&row.metrics)?])?;
        // Include model and source: never merge different workloads or live usage with synthetic benchmarks.
        let key = serde_json::to_string(&(row.slot_id,row.machine_id,&row.gpu_name,&row.workload_key,&row.allocation,&row.source,&row.metrics.model))?;
        let previous: Option<String> = tx.query_row("SELECT data FROM performance_catalogue WHERE catalogue_key=?1", [&key], |r| r.get(0)).optional()?;
        let mut c: CatalogueRow = match previous { Some(raw) => serde_json::from_str(&raw)?, None => CatalogueRow {
            slot_id:row.slot_id,machine_id:row.machine_id,gpu_name:row.gpu_name.clone(),profile:row.profile.clone(),workload_key:row.workload_key.clone(),allocation:row.allocation.clone(),
            model:row.metrics.model.clone(),image:row.image.clone(),source:row.source.clone(),..Default::default() } };
        c.samples += 1;
        c.last_seen = c.last_seen.max(row.ts);
        if row.success {
            c.successful += 1;
            let mut metrics = serde_json::to_value(&row.metrics)?;
            metrics["price_usd_h"] = serde_json::json!(row.price_usd_h);
            for (name,value) in metrics.as_object().unwrap() {
                if let Some(value) = value.as_f64() {
                    *c.sums.entry(name.clone()).or_default() += value;
                    *c.counts.entry(name.clone()).or_default() += 1;
                }
            }
        }
        tx.execute("INSERT INTO performance_catalogue(catalogue_key,data) VALUES(?1,?2) ON CONFLICT(catalogue_key) DO UPDATE SET data=excluded.data", params![key,serde_json::to_string(&c)?])?;
        // Apply each slot's retention only to that slot; do not destroy another slot's history.
        let cutoff = chrono::Utc::now().timestamp() - i64::from(retention_days) * 86400;
        tx.execute("DELETE FROM performance_samples WHERE slot_id=?1 AND ts < ?2", params![row.slot_id,cutoff])?;
        tx.execute("DELETE FROM performance_samples WHERE slot_id=?1 AND id IN
            (SELECT id FROM performance_samples WHERE slot_id=?1 ORDER BY id DESC LIMIT -1 OFFSET ?2)", params![row.slot_id,max_samples as i64])?;
        tx.commit()?;
        Ok(())
    }
    pub fn performance_samples(&self, slot: Option<i64>, machine: Option<i64>, before: Option<i64>, limit: usize) -> anyhow::Result<Vec<PerformanceRow>> {
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM performance_samples WHERE (?1 IS NULL OR slot_id=?1)
            AND (?2 IS NULL OR machine_id=?2) AND (?3 IS NULL OR id<?3) ORDER BY id DESC LIMIT ?4")?;
        let records = stmt.query_map(params![slot,machine,before,limit.min(10000) as i64], |r| {
            let json: String = r.get("metrics_json")?;
            Ok((PerformanceRow { id:r.get("id")?,ts:r.get("ts")?,slot_id:r.get("slot_id")?,instance_id:r.get("instance_id")?,
                machine_id:r.get("machine_id")?,gpu_name:r.get("gpu_name")?,profile:r.get("profile")?,workload_key:r.get("workload_key")?,allocation:r.get("allocation")?,
                image:r.get("image")?,source:r.get("source")?,success:r.get("success")?,price_usd_h:r.get("price_usd_h")?,metrics:Metrics::default() }, json))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        records.into_iter().map(|(mut row,raw)| { row.metrics=serde_json::from_str(&raw)?; Ok(row) }).collect()
    }
    pub fn benchmark_score_samples(&self, slot: i64, key: &str, since: i64) -> anyhow::Result<Vec<(i64,String,Metrics)>> {
        let conn=self.0.lock().unwrap();
        let rows=conn.prepare("SELECT machine_id,allocation,metrics_json FROM performance_samples WHERE slot_id=?1
            AND workload_key=?2 AND ts>=?3 AND source='benchmark' AND success=1 ORDER BY id DESC LIMIT 10000")?
            .query_map(params![slot,key,since],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(|(id,gpu,json)|Ok((id,gpu,serde_json::from_str(&json)?))).collect()
    }
    pub fn performance_catalogue(&self) -> anyhow::Result<Vec<CatalogueRow>> {
        let conn = self.0.lock().unwrap();
        let strings = conn.prepare("SELECT data FROM performance_catalogue ORDER BY catalogue_key")?
            .query_map([], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        strings.into_iter().map(|s| Ok(serde_json::from_str(&s)?)).collect()
    }
    pub fn benchmark_due(&self, instance: i64, key: &str, interval: u64) -> anyhow::Result<bool> {
        let conn = self.0.lock().unwrap();
        let last: Option<i64> = conn.query_row("SELECT attempted_at FROM benchmark_runs WHERE instance_id=?1 AND workload_key=?2", params![instance,key], |r| r.get(0)).optional()?;
        Ok(last.is_none_or(|ts| chrono::Utc::now().timestamp() - ts >= interval as i64))
    }
    pub fn mark_benchmark_attempt(&self, instance: i64, key: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().execute("INSERT INTO benchmark_runs(instance_id,workload_key,attempted_at) VALUES(?1,?2,?3)
            ON CONFLICT(instance_id,workload_key) DO UPDATE SET attempted_at=excluded.attempted_at", params![instance,key,chrono::Utc::now().timestamp()])?;
        Ok(())
    }
    pub fn observe_offers(&self, offers: &[praxis_policy::OfferSnapshot]) -> anyhow::Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        for o in offers.iter().filter(|o| o.machine_id > 0) {
            tx.execute("INSERT OR IGNORE INTO machine_stats(machine_id) VALUES(?1)", [o.machine_id])?;
            tx.execute("INSERT INTO machine_offers(machine_id,seen_at,offer_json) VALUES(?1,?2,?3)
                ON CONFLICT(machine_id) DO UPDATE SET seen_at=excluded.seen_at, offer_json=excluded.offer_json",params![o.machine_id,now_iso(),serde_json::to_string(o)?])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn set_machine_whitelist(&self, machine: i64, allow: bool) -> anyhow::Result<()> {
        anyhow::ensure!(machine > 0, "invalid machine ID");
        self.0.lock().unwrap().execute("INSERT INTO machine_stats(machine_id,whitelisted) VALUES(?1,?2)
            ON CONFLICT(machine_id) DO UPDATE SET whitelisted=excluded.whitelisted", params![machine,allow])?;
        Ok(())
    }
    pub fn save_rental_facts(&self, id: i64, offer: &praxis_policy::OfferSnapshot, runtime_key: &str) -> anyhow::Result<()> {
        let mut value=serde_json::to_value(offer)?;
        value["_runtime_key"]=serde_json::json!(runtime_key);
        self.0.lock().unwrap().execute("INSERT OR REPLACE INTO rental_facts(instance_id,offer_json) VALUES(?1,?2)", params![id,serde_json::to_string(&value)?])?;
        Ok(())
    }
    pub fn rental_runtime_key(&self, id: i64) -> anyhow::Result<Option<String>> {
        let raw: Option<String> = self.0.lock().unwrap().query_row("SELECT offer_json FROM rental_facts WHERE instance_id=?1", [id], |r|r.get(0)).optional()?;
        let value: Option<serde_json::Value> = raw.map(|r|serde_json::from_str(&r)).transpose()?;
        Ok(value.and_then(|v|v["_runtime_key"].as_str().map(str::to_string)))
    }
    pub fn rental_facts(&self, id: i64) -> anyhow::Result<Option<praxis_policy::OfferSnapshot>> {
        let raw: Option<String> = self.0.lock().unwrap().query_row("SELECT offer_json FROM rental_facts WHERE instance_id=?1", [id], |r| r.get(0)).optional()?;
        raw.map(|s| Ok(serde_json::from_str(&s)?)).transpose()
    }
}
