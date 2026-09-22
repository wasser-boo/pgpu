//! Durable, idempotent usage-charge floors. Never add an invoice twice or erase
//! historical spend when an instance stops, disappears or leaves today's label scope.
use super::{now_iso, Db};
use anyhow::{ensure, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;

pub(super) fn estimates(conn: &Connection, period: &str) -> Result<HashMap<i64, f64>> {
    let mut statement=conn.prepare("SELECT instance_id,SUM(metered_usd+storage_usd+traffic_usd) FROM instance_meter WHERE date LIKE ?1 GROUP BY instance_id")?;
    let rows = statement.query_map(params![format!("{period}%")], |r| {
        Ok((r.get(0)?, r.get::<_, f64>(1)?))
    })?;
    let result: HashMap<i64, f64> = rows.collect::<rusqlite::Result<_>>()?;
    ensure!(
        result.values().all(|v| v.is_finite() && *v >= 0.0),
        "invalid per-instance estimate"
    );
    Ok(result)
}
pub(super) fn metered(conn: &Connection, period: &str) -> Result<f64> {
    let value: f64 = conn.query_row(
        "SELECT COALESCE(SUM(metered_usd+traffic_usd),0) FROM budget_days WHERE date LIKE ?1",
        params![format!("{period}%")],
        |r| r.get(0),
    )?;
    ensure!(
        value.is_finite() && value >= 0.0,
        "invalid budget ledger total"
    );
    Ok(value)
}
pub(super) fn total(conn: &Connection, period: &str) -> Result<f64> {
    let base = metered(conn, period)?;
    let mut total = base;
    let estimates = estimates(conn, period)?;
    let legacy_unattributed = (base - estimates.values().sum::<f64>()).max(0.0);
    let mut provider_floor = 0.0;
    let mut statement = conn.prepare(
        "SELECT instance_id,floor_usd,meter_at_sync FROM provider_spend WHERE period=?1",
    )?;
    let rows = statement.query_map(params![period], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, f64>(2)?,
        ))
    })?;
    for row in rows {
        let (id, floor, anchor) = row?;
        ensure!(
            [floor, anchor].iter().all(|v| v.is_finite() && *v >= 0.0),
            "invalid provider budget floor"
        );
        let local = estimates.get(&id).copied().unwrap_or(0.0);
        let carried = floor + (local - anchor).max(0.0);
        provider_floor += carried;
        total += (carried - local).max(0.0);
    }
    // Pre-upgrade/legacy traffic cannot be attributed reliably. Do not add
    // invoices on top of already counted but unallocated historical spending.
    if legacy_unattributed > 0.000001 {
        total = base.max(provider_floor);
    }
    ensure!(total.is_finite(), "budget total overflow");
    Ok(total)
}
impl Db {
    /// All locally recorded compute/storage/traffic for the day, including stopped
    /// and destroyed contracts. Never limit this to the active backend.
    pub fn slot_costs(&self,date:&str)->Result<(f64,HashMap<i64,f64>)> {
        let conn=self.0.lock().unwrap();
        let mut stmt=conn.prepare("SELECT i.slot_id,SUM(m.metered_usd+m.storage_usd+m.traffic_usd)
            FROM instance_meter m JOIN instances i ON i.vast_id=m.instance_id
            WHERE m.date=?1 GROUP BY i.slot_id")?;
        let rows:HashMap<i64,f64>=stmt.query_map([date],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        ensure!(rows.values().all(|v|v.is_finite() && *v>=0.0),"invalid slot costs");
        Ok((metered(&conn,date)?,rows))
    }
    pub fn instance_costs(&self, period: &str) -> Result<HashMap<i64, f64>> {
        estimates(&self.0.lock().unwrap(), period)
    }
    pub fn metered_totals(&self, date: &str) -> Result<(f64, f64)> {
        let conn = self.0.lock().unwrap();
        Ok((metered(&conn, date)?, metered(&conn, &date[..7])?))
    }
    /// Commit both complete provider windows and their comparison together.
    /// Positive corrections only: provider reporting may lag and offers/rates are
    /// estimates. Carry subsequent local usage forward; never double count it.
    pub fn record_provider_usage(
        &self,
        snapshot_json: &str,
        charges: &[(String, i64, f64, f64)],
    ) -> Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        for (period, id, amount, current) in charges {
            ensure!(
                *id > 0
                    && amount.is_finite()
                    && *amount >= 0.0
                    && current.is_finite()
                    && *current >= 0.0,
                "invalid confirmed usage"
            );
            let current = *current;
            let prior:Option<(f64,f64,f64)>=tx.query_row("SELECT provider_usd,floor_usd,meter_at_sync FROM provider_spend WHERE period=?1 AND instance_id=?2",params![period,id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            let (confirmed, floor) = if let Some((previous, floor, anchor)) = prior {
                ensure!(
                    [previous, floor, anchor]
                        .iter()
                        .all(|v| v.is_finite() && *v >= 0.0),
                    "invalid persisted provider usage"
                );
                (
                    amount.max(previous),
                    amount.max(floor + (current - anchor).max(0.0)),
                )
            } else {
                (*amount, *amount)
            };
            ensure!(
                confirmed.is_finite() && floor.is_finite(),
                "provider budget floor overflow"
            );
            tx.execute("INSERT INTO provider_spend(period,instance_id,provider_usd,floor_usd,meter_at_sync,updated_at) VALUES(?1,?2,?3,?4,?5,?6)
                ON CONFLICT(period,instance_id) DO UPDATE SET provider_usd=?3,floor_usd=?4,meter_at_sync=?5,updated_at=?6",
                params![period,id,confirmed,floor,current,now_iso()])?;
        }
        tx.execute("INSERT INTO settings(k,v) VALUES('vast_billing_snapshot',?1) ON CONFLICT(k) DO UPDATE SET v=?1",params![snapshot_json])?;
        tx.commit()?;
        Ok(())
    }
    /// New traffic reports are attributed, preventing their inclusion twice when
    /// actual provider charges also contain bandwidth. Legacy estimates remain.
    pub fn meter_instance_traffic(&self, date: &str, id: i64, traffic_usd: f64) -> Result<()> {
        ensure!(
            id > 0 && traffic_usd.is_finite() && traffic_usd >= 0.0,
            "invalid traffic charge"
        );
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("INSERT INTO budget_days(date,traffic_usd) VALUES(?1,?2) ON CONFLICT(date) DO UPDATE SET traffic_usd=traffic_usd+excluded.traffic_usd",params![date,traffic_usd])?;
        tx.execute("INSERT INTO instance_meter(date,instance_id,traffic_usd,last_metered_at) VALUES(?1,?2,?3,?4) ON CONFLICT(date,instance_id) DO UPDATE SET traffic_usd=traffic_usd+excluded.traffic_usd",params![date,id,traffic_usd,now_iso()])?;
        tx.commit()?;
        Ok(())
    }
}
