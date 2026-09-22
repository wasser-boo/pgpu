//! Durable, serialized lifecycle operations. Provider errors never become local success.
//! Stop/delete are idempotent and retried after restart; start is not retried by
//! destroying a user's disk. Only the reconciler decides whether a new rental is needed.

use crate::state::SharedApp;

pub async fn stop_instance(app: &SharedApp, id: i64, reason: &str) -> anyhow::Result<()> {
    request(app, id, "stop", reason, false).await
}

pub async fn destroy_instance(app: &SharedApp, id: i64, reason: &str) -> anyhow::Result<()> {
    request(app, id, "destroy", reason, false).await
}

pub async fn automatic_request(app: &SharedApp, id: i64, operation: &str, reason: &str) -> anyhow::Result<()> {
    anyhow::ensure!(matches!(operation, "stop" | "destroy"), "invalid automatic operation");
    request(app, id, operation, reason, true).await
}

async fn request(app: &SharedApp, id: i64, operation: &str, reason: &str, automatic: bool) -> anyhow::Result<()> {
    let _lock = app.management.lock().await;
    let inst = app.db.instance(id).ok_or_else(|| anyhow::anyhow!("unknown instance {id}"))?;
    if inst.destroyed_at.is_some() { return Ok(()); }
    if automatic {
        anyhow::ensure!(!inst.pinned && !app.db.slot_pins().iter().any(|(s, pin)| *s == inst.slot_id && pin.is_some()), "automatic operation blocked by pin/lock");
        if inst.mode == praxis_common::Mode::Manual {
            crate::reconciler::meter(app)?;
            let cfg = crate::reconciler::effective_policy(app);
            let (today, month) = app.db.budget_totals(&crate::node::local_date(app))?;
            anyhow::ensure!(today >= cfg.budget.daily_hard_eur * cfg.budget.usd_per_eur
                || month >= cfg.budget.monthly_eur * cfg.budget.usd_per_eur, "manual mode permits automatic stop/destroy only at hard cap");
        }
    }
    // Persist before I/O. Never mark completed or stop metering on a failed request.
    app.db.queue_operation(id, operation, reason)?;
    app.reconcile_now.notify_one();
    for (pending_id, op, why) in app.db.pending_operations()? {
        if pending_id == id { return execute(app, id, &op, &why).await; }
    }
    anyhow::bail!("operation intent disappeared for {id}")
}

pub async fn retry_pending(app: &SharedApp) -> anyhow::Result<()> {
    let _lock = app.management.lock().await;
    for (id, op, reason) in app.db.pending_operations()? {
        if let Err(e) = execute(app, id, &op, &reason).await {
            tracing::warn!(id, operation = %op, %e, "provider operation remains pending");
        }
    }
    Ok(())
}

async fn execute(app: &SharedApp, id: i64, op: &str, reason: &str) -> anyhow::Result<()> {
    let inst = app.db.instance(id).ok_or_else(|| anyhow::anyhow!("unknown instance {id}"))?;
    let vast = app.vast.lock().unwrap().clone().ok_or_else(|| anyhow::anyhow!("vast api not configured"))?;
    match op {
        "destroy" => vast.destroy(id).await?,
        "stop" => vast.set_status(id, false).await?,
        _ => anyhow::bail!("invalid pending operation {op}"),
    }
    crate::reconciler::meter(app)?;
    if op == "destroy" {
        app.db.mark_destroyed(id)?; // also removes intent, atomically
    } else {
        app.db.complete_stop(id)?;
        // Retiring a replaced backer must not turn off the new active pool.
        if app.db.active_instance(inst.slot_id).is_none_or(|active| active == id) {
            app.db.set_slot_desired_audited(inst.slot_id, false, "stop confirmed")?;
        }
    }
    // Invalidate BOTH routing caches immediately, not at the next polling tick.
    if app.targets.get(inst.slot_id).map(|t| t.0) == Some(id) {
        app.targets.set(inst.slot_id, None, None, false);
    }
    let targets = app.pool_routes.healthy(inst.slot_id).into_iter().filter(|t| t.0 != id).collect();
    app.pool_routes.set_healthy(inst.slot_id, targets);
    app.events.emit(&app.db, if op == "destroy" { "instance_destroyed" } else { "instance_stopped" },
        Some(inst.slot_id), Some(id), reason, &serde_json::json!({"provider_confirmed": true}));
    // Peer cleanup is reconciled against complete Vast inventory, with retries.
    // An accepted delete request alone is not proof that the contract is gone.
    Ok(())
}

/// Revalidate the policy snapshot and publish BOTH routing caches while new
/// proxy requests are excluded. No network I/O is allowed inside this gate.
pub async fn flip_slot(app: &SharedApp, slot_id: i64, from: i64, to: i64, reason: &str) -> anyhow::Result<()> {
    let _lock = app.management.lock().await;
    app.traffic.when_idle(slot_id, |traffic| -> anyhow::Result<()> {
        anyhow::ensure!(app.db.active_instance(slot_id).unwrap_or(0) == from, "stale flip: active instance changed");
        anyhow::ensure!(!app.db.slot_pins().iter().any(|(s, pin)| *s == slot_id && pin.is_some()), "slot is locked");
        let cfg = app.cfg();
        let slot = cfg.slot(slot_id).ok_or_else(|| anyhow::anyhow!("unknown slot"))?;
        let target = app.db.instance(to).ok_or_else(|| anyhow::anyhow!("unknown replacement"))?;
        anyhow::ensure!(target.slot_id == slot_id && target.role == slot.role && target.destroyed_at.is_none()
            && target.healthy && target.state == "healthy" && target.actual_status == "running"
            && target.intended_status == "running" && !target.busy, "replacement is not ready/idle in this slot");
        anyhow::ensure!(target.nb_ip.is_some() || app.hub.nb_ip(to).is_some(), "replacement has no address");
        let pending = app.db.pending_operations()?;
        anyhow::ensure!(!pending.iter().any(|p| p.0 == to || p.0 == from), "flip has pending provider operations");
        if let Some(old) = app.db.instance(from) {
            anyhow::ensure!(!old.busy && !old.pinned && old.mode != praxis_common::Mode::Manual, "active backer is busy/pinned/manual");
            if old.actual_status == "running" {
                anyhow::ensure!(app.jobs.active(slot_id).is_none(), "active job batch prevents flip");
                if old.state == "healthy" && traffic.last_request > 0 {
                    anyhow::ensure!(chrono::Utc::now().timestamp() - traffic.last_request >= 60, "busy grace prevents flip");
                }
            }
        }
        app.db.set_active_instance(slot_id, Some(to))?;
        if let Err(error) = crate::reconciler::refresh_slot_routes(app, slot_id) {
            // A DB/cache publication failure must not keep serving the retired
            // backer. Reconciliation can republish the persisted active later.
            app.targets.set(slot_id, None, None, false);
            app.pool_routes.set_healthy(slot_id, Vec::new());
            return Err(error);
        }
        app.events.emit(&app.db, "slot_flipped", Some(slot_id), Some(to), reason, &serde_json::json!({"from": from}));
        Ok(())
    }).ok_or_else(|| anyhow::anyhow!("slot has active traffic/benchmark; flip deferred"))?
}

/// Recheck budgets/capacity UNDER management lock at execution time, not only
/// against the policy's earlier snapshot. Shared by manual and automatic flows.
/// `existing` starts an existing contract; None admits a new rental.
pub fn check_admission(app: &SharedApp, slot: i64, rate: f64, storage: f64, existing: Option<i64>) -> anyhow::Result<()> {
    check_admission_with_startup(app, slot, rate, storage, existing, 0.0)
}

pub fn check_admission_with_startup(app: &SharedApp, slot: i64, rate: f64, storage: f64, existing: Option<i64>, startup_cost: f64) -> anyhow::Result<()> {
    anyhow::ensure!(startup_cost.is_finite() && startup_cost >= 0.0, "invalid startup cost");
    anyhow::ensure!(!app.shutting_down.load(std::sync::atomic::Ordering::Relaxed), "router is shutting down");
    anyhow::ensure!(rate.is_finite() && rate > 0.0 && storage.is_finite() && storage >= 0.0, "invalid hourly rate");
    let cfg = crate::reconciler::effective_policy(app);
    let rows = app.db.instances(false);
    let pending = app.db.pending_operations()?;
    anyhow::ensure!(!pending.iter().any(|p| rows.iter().any(|r| r.vast_id == p.0 && r.slot_id == slot)), "slot has pending provider operations");
    if existing.is_none() {
        anyhow::ensure!((rows.len() as i64) < cfg.limits.max_instances, "instance limit reached");
        anyhow::ensure!((rows.iter().filter(|r| r.slot_id == slot).count() as i64) < cfg.limits.max_per_slot, "slot instance limit reached");
    }
    let running_rate: f64 = rows.iter().filter(|r| Some(r.vast_id) != existing &&
        (r.actual_status == "running" || matches!(r.state.as_str(), "requested" | "provisioning" | "booting" | "agent_connected")))
        .map(|r| if r.mode == praxis_common::Mode::Interruptible { r.bid_usd_h } else { r.dph_total }).sum();
    let storage_rate: f64 = rows.iter().map(|r| r.storage_usd_h).sum::<f64>() + storage;
    anyhow::ensure!(running_rate + rate + storage_rate <= cfg.limits.max_total_rate_usd_h, "hourly rate limit reached");
    let date = crate::node::local_date(app);
    let (today, month) = app.db.budget_totals(&date)?;
    let exchange = cfg.budget.usd_per_eur;
    anyhow::ensure!(today < cfg.budget.daily_hard_eur * exchange, "daily hard budget exhausted");
    let projected = (running_rate + rate + storage_rate) * 0.5 + startup_cost;
    anyhow::ensure!(today + projected <= cfg.budget.daily_soft_eur * exchange, "daily soft budget exhausted");
    anyhow::ensure!(month + projected <= cfg.budget.monthly_eur * exchange, "monthly budget exhausted");
    Ok(())
}

pub async fn change_bid(app: &SharedApp, id: i64, price: f64, reason: &str) -> anyhow::Result<()> {
    change_bid_impl(app, id, price, reason, false).await
}

pub async fn change_automatic_bid(app: &SharedApp, id: i64, price: f64, reason: &str) -> anyhow::Result<()> {
    change_bid_impl(app, id, price, reason, true).await
}

async fn change_bid_impl(app: &SharedApp, id: i64, price: f64, reason: &str, automatic: bool) -> anyhow::Result<()> {
    let _lock = app.management.lock().await;
    let inst = app.db.instance(id).ok_or_else(|| anyhow::anyhow!("unknown instance {id}"))?;
    let cfg = app.cfg();
    let slot = cfg.slot(inst.slot_id).ok_or_else(|| anyhow::anyhow!("unknown slot"))?;
    anyhow::ensure!(inst.destroyed_at.is_none() && inst.mode == praxis_common::Mode::Interruptible, "bid requires a live interruptible contract");
    anyhow::ensure!(!automatic || (!inst.pinned && !app.db.slot_pins().iter().any(|(s, pin)| *s == inst.slot_id && pin.is_some())), "automatic bid blocked by pin/lock");
    anyhow::ensure!(price.is_finite() && price > 0.0 && price <= slot.bid.ceiling_usd_h, "bid outside slot price ceiling");
    crate::catalog::check_resume(app, &inst, price)?;
    crate::reconciler::meter(app)?;
    if price > inst.bid_usd_h {
        check_admission(app, inst.slot_id, price, 0.0, Some(id))?;
    }
    let vast = app.vast.lock().unwrap().clone().ok_or_else(|| anyhow::anyhow!("vast api not configured"))?;
    vast.set_bid(id, price).await?;
    crate::reconciler::meter(app)?;
    app.db.set_instance_bid(id, price)?;
    app.events.emit(&app.db, "bid_changed", Some(inst.slot_id), Some(id), reason, &serde_json::json!({"price":price}));
    Ok(())
}

pub async fn start_instance(app: &SharedApp, id: i64, reason: &str) -> anyhow::Result<()> {
    start(app, id, reason, false).await
}

pub async fn start_automatic_instance(app: &SharedApp, id: i64, reason: &str) -> anyhow::Result<()> {
    start(app, id, reason, true).await
}

async fn start(app: &SharedApp, id: i64, reason: &str, automatic: bool) -> anyhow::Result<()> {
    let _lock = app.management.lock().await;
    anyhow::ensure!(!automatic || app.db.auto_rent_enabled(), "auto-rent disabled");
    let inst = app.db.instance(id).ok_or_else(|| anyhow::anyhow!("unknown instance {id}"))?;
    anyhow::ensure!(inst.destroyed_at.is_none(), "instance {id} is destroyed");
    anyhow::ensure!(!automatic || (!inst.pinned && inst.mode != praxis_common::Mode::Manual
        && !app.db.slot_pins().iter().any(|(s, pin)| *s == inst.slot_id && pin.is_some())), "automatic start blocked by pin/manual mode");
    anyhow::ensure!(!app.db.pending_operations()?.iter().any(|p| p.0 == id), "instance {id} has a pending stop/destroy");
    crate::reconciler::meter(app)?;
    let rate = if inst.mode == praxis_common::Mode::Interruptible { inst.bid_usd_h } else { inst.dph_total };
    crate::catalog::check_resume(app, &inst, rate)?;
    check_admission(app, inst.slot_id, rate, 0.0, Some(id))?;
    let vast = app.vast.lock().unwrap().clone().ok_or_else(|| anyhow::anyhow!("vast api not configured"))?;
    // Ambiguous/network failures must not destroy a disk as a 'recovery' action.
    vast.set_status(id, true).await?;
    app.db.set_instance_intended(id, "running")?;
    app.db.set_instance_state(id, "booting")?;
    app.db.set_slot_desired_audited(inst.slot_id, true, "start confirmed")?;
    app.events.emit(&app.db, "instance_started", Some(inst.slot_id), Some(id), reason, &serde_json::json!({}));
    app.reconcile_now.notify_one();
    Ok(())
}
