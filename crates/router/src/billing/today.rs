//! Presentation of today's reported charges versus local estimates, never a
//! second accounting ledger. Unknown/stale provider windows cannot become $0 today.
use super::{snapshot, SharedApp};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Serialize)]
pub struct TodayCosts {
    pub date:String,
    pub tz:String,
    pub provider:Option<f64>,
    pub provider_slots:HashMap<i64,f64>,
    pub local:Option<f64>,
    pub local_slots:HashMap<i64,f64>,
    pub unassigned:Option<f64>,
    pub synced_at:String,
    pub stale:bool,
}

pub fn today(app:&SharedApp,now:DateTime<Utc>)->TodayCosts {
    let cfg=app.cfg();
    let tz=cfg.router.tz.parse::<chrono_tz::Tz>().unwrap_or(chrono_tz::UTC);
    let date=now.with_timezone(&tz).format("%Y-%m-%d").to_string();
    let mut view=TodayCosts {date:date.clone(),tz:cfg.router.tz.clone(),provider:None,provider_slots:HashMap::new(),
        local:None,local_slots:HashMap::new(),unassigned:None,synced_at:String::new(),stale:false};
    if let Ok((local,mut slots))=app.db.slot_costs(&date) {
        slots.retain(|id,_|cfg.slot(*id).is_some());
        let allocated=slots.values().sum::<f64>();
        if allocated.is_finite() {view.unassigned=(local-allocated>0.000001).then_some(local-allocated);view.local=Some(local);view.local_slots=slots;}
    }
    if let Ok(Some(s))=snapshot(app) {
        // Scope/timezone have already been checked. A prior-day snapshot stays
        // useful in Settings, but is NOT today's charge on a dashboard card.
        if s.timezone==view.tz && s.day.period==date && s.day.through_unix<=now.timestamp()
            && s.day.provider_usd.is_finite() && s.day.provider_usd>=0.0
            && s.day.rows.iter().filter_map(|r|r.provider_usd).all(|v|v.is_finite() && v>=0.0) {
            let mut slots=HashMap::new();
            for row in &s.day.rows {
                if let Some(amount)=row.provider_usd {*slots.entry(row.slot_id).or_insert(0.0)+=amount;}
            }
            let sum=slots.values().sum::<f64>();
            if sum.is_finite() && (sum-s.day.provider_usd).abs()<0.000001 {
                view.provider=Some(s.day.provider_usd);view.provider_slots=slots;
                view.synced_at=DateTime::from_timestamp(s.day.through_unix,0).unwrap_or(now).with_timezone(&tz).format("%Y-%m-%d %H:%M:%S %Z").to_string();
                view.stale=now.timestamp()-s.day.through_unix>7200;
            }
        }
    }
    if app.db.try_setting("vast_billing_status").ok().flatten().and_then(|s|serde_json::from_str::<serde_json::Value>(&s).ok())
        .is_some_and(|s|s["state"]=="error") {view.stale=true;}
    view
}
