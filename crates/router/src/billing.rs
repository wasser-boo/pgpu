//! Compare actual Vast usage with the configured SLOT LABELS, not account-wide
//! movements and not only running contracts. Stopped/deleted tagged contracts count.
use crate::{config::Config, db::InstanceRow, slot_labels, state::SharedApp};
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Datelike, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Row {
    pub instance_id: i64,
    pub slot_id: i64,
    pub label: String,
    pub state: String,
    pub provider_usd: Option<f64>,
    pub estimated_usd: Option<f64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Window {
    pub period: String,
    pub from_unix: i64,
    pub through_unix: i64,
    pub provider_usd: f64,
    pub estimated_usd: f64,
    pub ignored_contracts: usize,
    pub rows: Vec<Row>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub scope: Vec<String>,
    pub timezone: String,
    pub synced_at: String,
    pub day: Window,
    pub month: Window,
}

fn midnight(date: chrono::NaiveDate, tz: chrono_tz::Tz) -> Result<DateTime<Utc>> {
    let mut local = date.and_hms_opt(0, 0, 0).context("invalid local date")?;
    for _ in 0..=1440 {
        if let Some(at) = tz.from_local_datetime(&local).earliest() {
            return Ok(at.with_timezone(&Utc));
        }
        local += chrono::Duration::minutes(1);
    }
    anyhow::bail!("no valid billing day boundary")
}

/// Calendar midnight in the configured timezone, including 23/25-hour DST days.
pub(crate) fn seconds_to_day_end(now:DateTime<Utc>,tz:chrono_tz::Tz)->Result<i64> {
    let tomorrow=now.with_timezone(&tz).date_naive().succ_opt().context("invalid next day")?;
    Ok((midnight(tomorrow,tz)?-now).num_seconds().max(0))
}

fn label<'a>(
    id: i64,
    metadata: Option<&'a str>,
    current: &'a HashMap<i64, praxis_vast::Instance>,
    known: &'a HashMap<i64, InstanceRow>,
) -> Option<&'a str> {
    // Explicit current labels win over historical metadata. An empty label is
    // an explicit removal, not permission to guess from an old DB value.
    if let Some(instance) = current.get(&id) {
        instance.label.as_deref().or(metadata)
    } else {
        metadata.or_else(|| known.get(&id).map(|r| r.label.as_str()))
    }
}

fn compare(
    cfg: &Config,
    period: String,
    from_unix: i64,
    through_unix: i64,
    charges: &[praxis_vast::Charge],
    current: &HashMap<i64, praxis_vast::Instance>,
    known: &HashMap<i64, InstanceRow>,
    estimates: &HashMap<i64, f64>,
) -> Result<Window> {
    let mut rows = BTreeMap::new();
    for instance in known.values() {
        let Some(label) = label(instance.vast_id, None, current, known) else {
            continue;
        };
        let Some(slot_id) = slot_labels::matching_slot(label, cfg) else {
            continue;
        };
        rows.insert(
            instance.vast_id,
            Row {
                instance_id: instance.vast_id,
                slot_id,
                label: label.into(),
                state: current
                    .get(&instance.vast_id)
                    .map(|v| v.actual_or("unknown"))
                    .unwrap_or_else(|| instance.state.clone()),
                provider_usd: None,
                estimated_usd: Some(estimates.get(&instance.vast_id).copied().unwrap_or(0.0)),
            },
        );
    }
    let mut ignored_contracts = 0;
    for charge in charges {
        let Some(id) = charge.instance_id() else {
            ignored_contracts += 1;
            continue;
        };
        let meta = charge.metadata.as_ref().and_then(|m| m.label.as_deref());
        let Some(label) = label(id, meta, current, known) else {
            ignored_contracts += 1;
            continue;
        };
        let Some(slot_id) = slot_labels::matching_slot(label, cfg) else {
            ignored_contracts += 1;
            continue;
        };
        ensure!(
            charge.amount.is_finite() && charge.amount >= 0.0,
            "invalid tagged usage amount"
        );
        let state = current
            .get(&id)
            .map(|v| v.actual_or("unknown"))
            .or_else(|| known.get(&id).map(|v| v.state.clone()))
            .unwrap_or_else(|| "nicht lokal erfasst".into());
        rows.insert(
            id,
            Row {
                instance_id: id,
                slot_id,
                label: label.into(),
                state,
                provider_usd: Some(charge.amount),
                estimated_usd: known
                    .contains_key(&id)
                    .then(|| estimates.get(&id).copied().unwrap_or(0.0)),
            },
        );
    }
    let rows: Vec<_> = rows.into_values().collect();
    let provider_usd = rows.iter().filter_map(|r| r.provider_usd).sum::<f64>();
    let estimated_usd = rows.iter().filter_map(|r| r.estimated_usd).sum::<f64>();
    ensure!(
        provider_usd.is_finite() && estimated_usd.is_finite(),
        "billing total overflow"
    );
    Ok(Window {
        period,
        from_unix,
        through_unix,
        provider_usd,
        estimated_usd,
        ignored_contracts,
        rows,
    })
}

pub async fn reconcile(app: &SharedApp) -> Result<Snapshot> {
    let cfg = app.cfg();
    let vast = app
        .vast
        .lock()
        .unwrap()
        .clone()
        .context("Vast API nicht konfiguriert")?;
    let now = Utc::now();
    let tz = cfg.router.tz.parse::<chrono_tz::Tz>()?;
    let date = now.with_timezone(&tz).date_naive();
    let day_start = midnight(date, tz)?.timestamp();
    let month_start = midnight(date.with_day(1).unwrap(), tz)?.timestamp();
    crate::reconciler::meter(app)?;
    let day_estimates = app.db.instance_costs(&date.to_string())?;
    let month_estimates = app.db.instance_costs(&date.format("%Y-%m").to_string())?;
    let known: HashMap<_, _> = app
        .db
        .try_instances(true)?
        .into_iter()
        .map(|r| (r.vast_id, r))
        .collect();
    let current: HashMap<_, _> = vast
        .instances()
        .await?
        .into_iter()
        .map(|r| (r.id, r))
        .collect();
    let daily = vast.charges(day_start, now.timestamp()).await?;
    let monthly = if day_start == month_start {
        daily.clone()
    } else {
        vast.charges(month_start, now.timestamp()).await?
    };
    let day = compare(
        &cfg,
        date.to_string(),
        day_start,
        now.timestamp(),
        &daily,
        &current,
        &known,
        &day_estimates,
    )?;
    let month = compare(
        &cfg,
        date.format("%Y-%m").to_string(),
        month_start,
        now.timestamp(),
        &monthly,
        &current,
        &known,
        &month_estimates,
    )?;
    ensure!(
        month.provider_usd + 0.005 >= day.provider_usd,
        "inconsistent provider billing windows; retry later"
    );
    let _management = app.management.lock().await;
    ensure!(
        !app.shutting_down.load(std::sync::atomic::Ordering::Relaxed),
        "router shutting down"
    );
    ensure!(
        slot_labels::scope(&cfg) == slot_labels::scope(&app.cfg())
            && cfg.router.tz == app.cfg().router.tz,
        "billing scope changed during comparison"
    );
    let snapshot = Snapshot {
        scope: slot_labels::scope(&cfg),
        timezone: cfg.router.tz.clone(),
        synced_at: crate::db::now_iso(),
        day,
        month,
    };
    let charges: Vec<_> = [
        (&snapshot.day, &day_estimates),
        (&snapshot.month, &month_estimates),
    ]
    .into_iter()
    .flat_map(|(window, estimates)| {
        window.rows.iter().filter_map(move |r| {
            r.provider_usd.map(|amount| {
                (
                    window.period.clone(),
                    r.instance_id,
                    amount,
                    estimates.get(&r.instance_id).copied().unwrap_or(0.0),
                )
            })
        })
    })
    .collect();
    app.db
        .record_provider_usage(&serde_json::to_string(&snapshot)?, &charges)?;
    Ok(snapshot)
}

/// Called in a separate bounded background task: billing API latency must not
/// stop metering, budget enforcement or lifecycle reconciliation.
pub async fn refresh(app: SharedApp) {
    let outcome = match reconcile(&app).await {
        Ok(snapshot) => {
            app.events.emit(&app.db,"billing_reconciled",None,None,
                &format!("Vast-Nutzung nach Slot-Labels: heute {:.3} $, Monat {:.3} $ (laufend + gestoppt/historisch)",snapshot.day.provider_usd,snapshot.month.provider_usd),
                &json!({"scope":snapshot.scope,"day":snapshot.day.provider_usd,"month":snapshot.month.provider_usd}));
            app.reconcile_now.notify_one();
            json!({"state":"ok","at":crate::db::now_iso()})
        }
        Err(error) => {
            tracing::warn!(%error,"Vast usage comparison unavailable; existing budget ledger preserved");
            json!({"state":"error","at":crate::db::now_iso(),"message":error.to_string()})
        }
    };
    let _ = app
        .db
        .set_setting("vast_billing_status", &outcome.to_string());
}

pub fn snapshot(app: &SharedApp) -> Result<Option<Snapshot>> {
    let snapshot = app
        .db
        .try_setting("vast_billing_snapshot")?
        .map(|s| serde_json::from_str::<Snapshot>(&s))
        .transpose()?;
    Ok(snapshot
        .filter(|s| s.scope == slot_labels::scope(&app.cfg()) && s.timezone == app.cfg().router.tz))
}
pub fn status(app: &SharedApp) -> serde_json::Value {
    match snapshot(app) {
        Ok(snapshot) => json!({"scope":slot_labels::scope(&app.cfg()),"snapshot":snapshot,
            "last_attempt":app.db.setting("vast_billing_status").and_then(|s|serde_json::from_str::<serde_json::Value>(&s).ok()),
            "budget_rule":"confirmed usage floor plus subsequent metering; never lower limits or erase historical/storage costs"}),
        Err(error) => json!({"scope":slot_labels::scope(&app.cfg()),"error":error.to_string()}),
    }
}

mod today;
pub use today::{today,TodayCosts};

#[cfg(test)]
mod tests;
