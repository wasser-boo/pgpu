//! Shared admission/host-trust checks for manual rental, automation and dashboard.
use crate::{config::SlotCfg, state::SharedApp};
use praxis_common::Mode;
use praxis_policy::{eligibility::{self, Assessment}, OfferSnapshot};

pub fn assess(app: &SharedApp, slot: &SlotCfg, offer: &OfferSnapshot, mode: Mode, price: f64, disk: i64) -> Assessment {
    let stat=app.db.machine_stat(offer.machine_id);
    let mut result=eligibility::assess(&slot.requirements,&slot.bid,offer,mode,price,disk,slot.traffic_gb,
        stat.as_ref().is_some_and(|s|s.whitelisted),app.cfg().vast.activate_blacklist && stat.is_some_and(|s|s.blacklisted),app.cfg().vast.activate_whitelist);
    match crate::selection::rejection(slot,offer.geolocation.as_deref(),&offer.gpu_name,mode != Mode::Interruptible) {
        Ok(Some(reason))=>result.reasons.push(reason),
        Err(error)=>result.reasons.push(format!("Ungültige Standort-/GPU-Auswahl: {error}")),
        Ok(None)=>{},
    }
    result.eligible=result.reasons.is_empty();
    result
}

pub fn check_resume(app: &SharedApp, inst: &crate::db::InstanceRow, price: f64) -> anyhow::Result<()> {
    let cfg=app.cfg();
    let slot=cfg.slot(inst.slot_id).ok_or_else(||anyhow::anyhow!("unknown slot"))?;
    let offer=app.db.rental_facts(inst.vast_id)?.unwrap_or_else(||OfferSnapshot {
        machine_id:inst.machine_id,gpu_name:inst.gpu_name.clone(),..Default::default()
    });
    let stat=app.db.machine_stat(inst.machine_id);
    let reasons=slot.requirements.hardware_rejections(&offer,stat.as_ref().is_some_and(|s|s.whitelisted),cfg.vast.activate_blacklist && stat.is_some_and(|s|s.blacklisted),cfg.vast.activate_whitelist);
    anyhow::ensure!(reasons.is_empty(),"resume rejected: {} (legacy instances may lack hardware facts)",reasons.join("; "));
    anyhow::ensure!(price.is_finite() && price>0.0 && price<=slot.bid.ceiling_usd_h,"slot price ceiling exceeded");
    if let Some(max)=slot.requirements.gpu_ceiling(&inst.gpu_name) {anyhow::ensure!(price<=max,"GPU model price ceiling exceeded");}
    if let Some(max)=slot.requirements.max_storage_usd_h {anyhow::ensure!(inst.storage_usd_h<=max,"storage ceiling exceeded");}
    if let Some(max)=slot.requirements.max_effective_usd_h {anyhow::ensure!(price+inst.storage_usd_h<=max,"effective hourly ceiling exceeded");}
    Ok(())
}

pub async fn machine_action(app: &SharedApp, id: i64, action: &str) -> anyhow::Result<()> {
    anyhow::ensure!(id>0,"invalid machine ID");
    let _lock=app.management.lock().await;
    match action {
        "whitelist"=>app.db.set_machine_whitelist(id,true)?,
        "unwhitelist"=>app.db.set_machine_whitelist(id,false)?,
        "blacklist"=>app.db.set_machine_blacklist(id,true,"manuell blacklisted")?,
        "unblacklist"=>app.db.set_machine_blacklist(id,false,"")?,
        _=>anyhow::bail!("unknown action; use whitelist|unwhitelist|blacklist|unblacklist"),
    }
    app.events.emit(&app.db,"machine_trust_changed",None,None,&format!("Host {id}: {action}"),&serde_json::json!({"machine_id":id,"action":action}));
    app.reconcile_now.notify_one();
    Ok(())
}
