//! Reconciler: 30-s-Takt (+ Wake-Notifies). Synchronisiert Vast-Instanzen,
//! errechnet Busy pro Service, wendet die Policy an, meteret Kosten und
//! reconciled stündlich gegen den Vast-Kontostand.

use crate::state::SharedApp;
use praxis_common::{Action, InstanceState, Mode, Role};
use praxis_policy::{InstanceSnapshot, OfferSnapshot, SlotSnapshot, Snapshot};
use praxis_vast::CreateInstanceParams;
use chrono::{Datelike, Timelike};
use rand::RngCore;
use std::collections::HashMap;

pub use crate::operations::{destroy_instance, start_instance, stop_instance};

pub async fn run(app: SharedApp) {
    let poll = std::time::Duration::from_secs(app.cfg().vast.poll_interval_s.max(5));
    let offer_every = std::time::Duration::from_secs(app.cfg().vast.offer_poll_s.max(15));
    let mut offer_due = tokio::time::Instant::now();
    let mut meter_hour = tokio::time::Instant::now() + std::time::Duration::from_secs(3600);
    loop {
        if app.shutting_down.load(std::sync::atomic::Ordering::Relaxed) { return; }
        tokio::select! {
            _ = tokio::time::sleep(poll) => {},
            _ = app.reconcile_now.notified() => {
                // kleines Antibounce, damit Events nicht 100 % CPU fressen
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            // Offer-Refresh entkoppelt: blockierendes interval.tick() hätte
            // jede Wake-Reaktion um bis zu offer_poll_s verzögert.
            _ = tokio::time::sleep_until(offer_due) => {
                for slot in app.cfg().slots.clone() {
                    let _ = search_slot_offers(&app, slot.id, true).await;
                }
                offer_due = tokio::time::Instant::now() + offer_every;
            }
        }
        if app.shutting_down.load(std::sync::atomic::Ordering::Relaxed) { return; }
        match tick(&app).await {
            Ok(()) => app.last_reconcile.store(chrono::Utc::now().timestamp(), std::sync::atomic::Ordering::Relaxed),
            Err(e) => tracing::warn!(error = format!("{e:#}"), "reconcile tick failed"),
        }
        if tokio::time::Instant::now() >= meter_hour {
            meter_hour = tokio::time::Instant::now() + std::time::Duration::from_secs(3600);
            reconcile_credit(&app).await;
        }
    }
}

pub async fn tick(app: &SharedApp) -> anyhow::Result<()> {
    // Account elapsed time even if the provider inventory request fails.
    meter(app)?;
    crate::operations::retry_pending(app).await?;
    // 0. Zeitpläne ([[schedule]]-Regeln) — vor allem anderen, damit eine
    //    18:00-Destroy-All-Regel die Boxen weg räumt, bevor der Tick sie
    //    wieder als "soll laufen" betrachtet.
    run_schedules(app).await;

    // 1. Vast-Sync.
    let vast_client = app.vast.lock().unwrap().clone();
    let vast_instances = match vast_client {
        Some(vast) => Some(vast.instances().await?),
        None => None,
    };
    if let Some(list) = &vast_instances {
        sync_vast(app, list).await?;
    }

    // 2. Agent-Liveness: unreachable-Erkennung + Host-Fail-Buchung.
    //    Ein Fail = eine Transition — Reconnects zählen neu, Blips nicht
    //    (Connector flickt die in ~10 s, bevor 3 min Stille ansteht).
    let now_ts = chrono::Utc::now().timestamp();
    for inst in app.db.instances(false) {
        if matches!(inst.state.as_str(), "healthy" | "agent_connected" | "booting") {
            if let Some(seen) = app.hub.last_seen(inst.vast_id) {
                if now_ts - seen > 180 {
                    let _ = app.db.set_instance_state(inst.vast_id, "unreachable");
                    record_machine_fail(app, inst.slot_id, inst.vast_id, inst.machine_id, "agent >3 min still (unreachable)");
                    // Letzter Agent-Health mit in den Event: Der Degraded-Grund
                    // (z. B. "11434 → 503") sagt, warum die Box nicht healthy
                    // wurde, bevor der Agent verstummte.
                    let health = app
                        .hub
                        .heartbeat(inst.vast_id)
                        .map(|hb| hb.health_json.to_string())
                        .unwrap_or_else(|| "kein Heartbeat".into());
                    app.events.emit(
                        &app.db,
                        "unreachable",
                        Some(inst.slot_id),
                        Some(inst.vast_id),
                        &format!("Agent >3 min still — letzter Health: {health}"),
                        &serde_json::json!({"health": health}),
                    );
                }
            }
        }
        // 2b. Unreachable-Stopper: Vast sagt running (= GPU-Geld brennt),
        //     Agent bleibt tot → stoppen (Disk bleibt warm), damit die Box
        //     nicht unmetered weiterläuft. Fail wurde beim Übergang gezählt.
        if inst.state == "unreachable" && inst.actual_status == "running" && !inst.pinned && inst.mode != Mode::Manual
            && !app.db.slot_pins().iter().any(|(slot, pin)| *slot == inst.slot_id && pin.is_some()) {
            let connected = app.hub.heartbeat(inst.vast_id).is_some();
            let silence = app.hub.last_seen(inst.vast_id).map(|s| now_ts - s).unwrap_or(i64::MAX);
            if !connected && silence > app.cfg().vast.unreachable_stop_after_s.max(180) {
                let _ = crate::operations::automatic_request(
                    app, inst.vast_id, "stop",
                    "unreachable: Vast running, Agent tot — Stop angefordert, Disk bleibt",
                ).await;
            }
        }
    }

    // Explicit opt-in only; attempt/cooldown persisted before any workload.
    crate::performance::auto_benchmarks(app).await;

    // 3. Busy pro Instanz (Agent-Busy, Service-Probes, In-flight).
    for inst in app.db.instances(false) {
        if !matches!(inst.state.as_str(), "healthy" | "agent_connected" | "draining") {
            continue;
        }
        let (busy, reason) = compute_busy(app, &inst).await;
        if busy != inst.busy || reason != inst.busy_reason {
            let _ = app.db.update_instance_busy(inst.vast_id, busy, &reason);
        }
    }

    // Do not race cache publication with a confirmed stop/destroy or flip.
    {
        let _lock = app.management.lock().await;
        for slot in &app.cfg().slots { refresh_slot_routes(app, slot.id)?; }
    }

    // 6. Snapshot bauen + Policy.
    let snap = build_snapshot(app).await?;
    let actions = praxis_policy::decide(&snap, &effective_policy(app));
    for action in actions {
        if let Err(e) = apply_action(app, &action).await {
            tracing::warn!(?action, %e, "action failed");
        }
    }
    Ok(())
}

/// Caller holds management lock. A warm=1 replacement MUST NOT receive
/// traffic before FlipSlot admits it; otherwise pool routing bypasses idle-only.
pub fn refresh_slot_routes(app: &SharedApp, slot_id: i64) -> anyhow::Result<()> {
    let cfg = app.cfg();
    let slot = cfg.slot(slot_id).ok_or_else(|| anyhow::anyhow!("unknown slot"))?;
    let pending = app.db.pending_operations()?;
    let usable = |r: &crate::db::InstanceRow| r.slot_id == slot_id && r.role == slot.role
        && r.destroyed_at.is_none() && r.healthy && r.state == "healthy" && r.actual_status == "running"
        && r.intended_status == "running" && !pending.iter().any(|p| p.0 == r.vast_id);
    let active = app.db.active_instance(slot_id);
    let row = active.and_then(|id| app.db.instance(id));
    if row.as_ref().is_none_or(|r| r.destroyed_at.is_some()) {
        if let Some(id) = active { app.db.clear_active_instance(slot_id, id)?; }
    }
    if let Some(r) = row.as_ref().filter(|r| r.destroyed_at.is_none()) {
        app.targets.set(slot_id, Some(r.vast_id), r.nb_ip.clone().or_else(|| app.hub.nb_ip(r.vast_id)), usable(r));
    } else {
        app.targets.set(slot_id, None, None, false);
    }
    let targets = app.db.instances(false).into_iter().filter(usable)
        .filter(|r| slot.pool.warm > 1 || Some(r.vast_id) == active)
        .filter_map(|r| r.nb_ip.clone().or_else(|| app.hub.nb_ip(r.vast_id)).map(|ip| (r.vast_id, ip))).collect();
    app.pool_routes.set_healthy(slot_id, targets);
    Ok(())
}

async fn sync_vast(app: &SharedApp, list: &[praxis_vast::Instance]) -> anyhow::Result<()> {
    let _lock = app.management.lock().await;
    meter(app)?;
    let by_id: HashMap<i64, &praxis_vast::Instance> = list.iter().map(|i| (i.id, i)).collect();
    for row in app.db.instances(true) {
        let Some(v) = by_id.get(&row.vast_id) else {
            // Vast kennt sie nicht mehr → destroyed markieren.
            if row.state != "destroyed" {
                // Warmup-Box, die Vast komplett verschluckt = Host-Desaster
                // (kein normaler GC eines exited Interruptible) → Fail zählen.
                if matches!(row.state.as_str(), "requested" | "provisioning" | "booting" | "agent_connected") {
                    record_machine_fail(app, row.slot_id, row.vast_id, row.machine_id, "instance_gone während warmup");
                }
                let _ = app.db.mark_destroyed(row.vast_id);
                app.events.emit(&app.db, "instance_gone", Some(row.slot_id), Some(row.vast_id), "nicht mehr in Vast-Liste", &serde_json::json!({}));
            }
            continue;
        };
        // machine_id nicht wischen, wenn Vast ihn diesmal nicht liefert.
        let machine_id = v.machine_id.unwrap_or(row.machine_id);
        app.db.update_instance_vast(row.vast_id, &v.actual_or("loading"), v.min_bid.unwrap_or(row.min_bid), v.dph_total.unwrap_or(row.dph_total), machine_id, v.gpu_or(&row.gpu_name))?;
        // Provider acknowledgement is not always immediate convergence. Retry
        // stopped intent if the next inventory still reports a running GPU.
        if row.state != "destroyed" && row.intended_status == "stopped" {
            if v.actual_or("") == "running" {
                app.db.queue_operation(row.vast_id, "stop", "reconcile: provider still running")?;
            } else if v.actual_or("") == "stopped" {
                app.db.complete_stop(row.vast_id)?;
            }
        }

        // Preemption: wir WOLLEN running, Vast sagt nein.
        let intended_run = v.intended_or("").is_empty() || v.intended_or("") == "running" || row.intended_status == "running";
        let actual = v.actual_or("");
        // False-Preempt-Schutz (Spike-Lektion): frische Instanzen melden
        // mitunter cur_state="stopped", während actual noch "loading"/
        // "created" ist (Container startet gleich) — KEIN Preempt.
        // cur_state zählt nur, wenn actual definitiv tot ist ODER die
        // Box schon mal lief (actual=running + Container weg = echtes Ende).
        let warming = matches!(actual.as_str(), "" | "loading" | "created" | "provisioning");
        let actually_dead = matches!(actual.as_str(), "stopped" | "exited" | "error" | "deleted")
            || (v.cur_state.as_deref() == Some("stopped") && !warming);
        if intended_run && actually_dead && !matches!(row.state.as_str(), "preempted" | "stopped" | "destroyed" | "draining" | "failed") {
            let _ = app.db.set_instance_state(row.vast_id, "preempted");
            // Host-Fail, wenn die Box nie healthy wurde: gestorben auf dem
            // Weg hoch = wackliger Host (Spike: 3× 3090 hintereinander),
            // kein normaler Outbid einer laufenden Instanz.
            // Von unreachable kommend wurde die Episode schon gezählt.
            if !row.ever_healthy && row.state != "unreachable" {
                record_machine_fail(app, row.slot_id, row.vast_id, machine_id, "preempt während warmup");
            }
            app.events.emit(
                &app.db,
                "PREEMPTED",
                Some(row.slot_id),
                Some(row.vast_id),
                &format!(
                    "Instanz {} ({}) von Vast weggebrochen: actual={} cur_state={:?} intended={} min_bid={:.4} bid={:.4}",
                    row.vast_id, row.gpu_name, v.actual_or("?"), v.cur_state, v.intended_or("?"), v.min_bid.unwrap_or(0.0), row.bid_usd_h
                ),
                &serde_json::json!({"min_bid": v.min_bid.unwrap_or(0.0), "bid": row.bid_usd_h}),
            );
        } else if v.actual_or("loading") == "loading" && matches!(row.state.as_str(), "requested" | "booting") {
            let _ = app.db.set_instance_state(row.vast_id, "provisioning");
        }
    }
    Ok(())
}

/// Warum ist die Box nicht healthy? Service-Log-Tails über den Agent
/// ziehen (VOR dem Destroy — die Session fällt mit der Box). Die echten
/// Fehler (CUDA-Arch „no kernel image" wie auf V100/sm_70, OOM, moe-cache-
/// Guard, HF-Abbrüche) landen in /workspace/logs/*.log, NICHT im
/// supervisord-stdout, den Vast request_logs zeigen.
pub async fn agent_failure_logs(app: &SharedApp, vast_id: i64, role: Role) -> String {
    let files: &[&str] = match role {
        Role::Llm => &["llama-chat", "llama-prepare"],
        Role::Media => &["comfyui"],
    };
    let mut parts: Vec<String> = Vec::new();
    for svc in files {
        let res = app
            .hub
            .command(vast_id, |id| praxis_common::node::RouterCommand::Cmd {
                id,
                command: praxis_common::node::Command::Tail {
                    file: format!("/workspace/logs/{svc}.log"),
                    lines: 30,
                },
            })
            .await;
        match res {
            Ok(data) => {
                if let Some(text) = data.get("output").and_then(|o| o.as_str()) {
                    if !text.trim().is_empty() {
                        parts.push(format!("--- {svc}.log ---\n{}", text));
                    }
                }
            }
            Err(e) => parts.push(format!("--- {svc}.log: Agent nicht erreichbar ({e}) ---")),
        }
    }
    parts.join("\n")
}

/// Fehlschlag-Event mit Diagnose: letzter Agent-Health (Degraded-Grund,
/// z. B. „11434 → 503“) + Service-Log-Tails. Grund im reason (Dashboard-
/// Feed), volle Logs im Event-Payload.
async fn emit_failure_diagnosis(app: &SharedApp, vast_id: i64, slot_id: i64, kind: &str, reason: &str) {
    let row = app.db.instance(vast_id);
    let role = row.as_ref().map(|r| r.role).unwrap_or(Role::Llm);
    let health = app
        .hub
        .heartbeat(vast_id)
        .map(|hb| hb.health_json.to_string())
        .unwrap_or_else(|| "kein Heartbeat".into());
    let logs = agent_failure_logs(app, vast_id, role).await;
    // Kompakter Log-Schnipsel (letzte Zeilen) in den reason, damit der
    // Grund IM Event-Feed sichtbar ist (z. B. „CUDA error: no kernel image").
    let snippet: String = logs
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty() && !l.starts_with("---"))
        .map(|l| l.chars().take(220).collect())
        .unwrap_or_default();
    let reason_full = if snippet.is_empty() {
        format!("{reason} — health: {health}")
    } else {
        format!("{reason} — health: {health} — Log: {snippet}")
    };
    app.events.emit(
        &app.db,
        kind,
        Some(slot_id),
        Some(vast_id),
        &reason_full,
        &serde_json::json!({"health": health, "logs": logs}),
    );
}

/// Maschinen-Fail buchen + Auto-Blacklist nach `vast.blacklist_after_fails`.
/// Spike-Lektion: Wake-Replace mietet sonst immer auf dem nächstbilligen
//  (= wackligsten) Host; drei 3090-Hosts starben reihenweise vast-seitig.
fn record_machine_fail(app: &SharedApp, slot_id: i64, instance_id: i64, machine_id: i64, kind: &str) {
    if machine_id == 0 {
        return; // ohne machine_id nicht attribuierbar
    }
    let Ok(fails) = app.db.record_machine_fail(machine_id, kind) else {
        return;
    };
    let threshold = app.cfg().vast.blacklist_after_fails;
    let already = app.db.machine_stat(machine_id).map(|m| m.blacklisted).unwrap_or(false);
    if app.cfg().vast.activate_blacklist && threshold >= 1 && !already && fails >= threshold {
        let _ = app.db.set_machine_blacklist(machine_id, true, kind);
        app.events.emit(
            &app.db,
            "machine_blacklisted",
            Some(slot_id),
            Some(instance_id),
            &format!("Host {machine_id} nach {fails} Fails blacklisted ({kind}) — Wake-Replace mietet ihn nicht mehr"),
            &serde_json::json!({"machine_id": machine_id, "fails": fails}),
        );
    } else {
        app.events.emit(
            &app.db,
            "machine_fail",
            Some(slot_id),
            Some(instance_id),
            &format!("Host {machine_id}: Fail #{fails} ({kind})"),
            &serde_json::json!({"machine_id": machine_id, "fails": fails}),
        );
    }
}

/// Busy: Agent-Meldung, Service-HTTP-Probe (ComfyUI-Queue), Router-In-flight.
async fn compute_busy(app: &SharedApp, inst: &crate::db::InstanceRow) -> (bool, String) {
    let traffic = app.traffic.snapshot(inst.slot_id);
    let in_flight = traffic.in_flight;
    if in_flight > 0 {
        return (true, format!("{in_flight} in-flight"));
    }
    // X-Router-Job-Id-Batch: Praxis hält den Slot für zusammenhängende
    // Sätze busy — kein Idle-Stop zwischen den Requests eines Jobs.
    if let Some(reason) = app.jobs.active(inst.slot_id) {
        return (true, reason);
    }
    if let Some(hb) = app.hub.heartbeat(inst.vast_id) {
        if hb.busy {
            return (true, hb.busy_reason.clone());
        }
    }
    // ComfyUI-Queue (Jobs laufen nach HTTP-Return weiter!).
    if let Some(slot) = app.cfg().slot(inst.slot_id).cloned() {
        if let Some(svc) = slot.services.values().find(|s| s.busy == crate::config::BusyKind::ComfyQueue) {
            if let Some(nb_ip) = inst.nb_ip.clone().or_else(|| app.hub.nb_ip(inst.vast_id)) {
                let url = format!("http://{nb_ip}:{}/prompt", svc.port);
                if let Ok(resp) = reqwest::get(&url).await {
                    if let Ok(j) = resp.json::<serde_json::Value>().await {
                        let remaining = j
                            .pointer("/exec_info/queue_remaining")
                            .and_then(|r| r.as_i64())
                            .unwrap_or(0);
                        if remaining > 0 {
                            return (true, format!("comfy queue {remaining}"));
                        }
                    }
                }
            }
        }
        // STT: lokale WS-Sessions (Router-proxy).
        if slot.services.values().any(|s| s.busy == crate::config::BusyKind::WsSessions)
            && app.cfg().stt.mode == "local"
            && app.stt_sessions.load(std::sync::atomic::Ordering::Relaxed) > 0
        {
            return (true, "stt sessions aktiv".into());
        }
    }
    // busy_grace: 60 s nach letztem Request.
    let now = chrono::Utc::now().timestamp();
    if traffic.last_request > 0 && now - traffic.last_request < 60 {
        return (true, "busy_grace".into());
    }
    (false, String::new())
}

pub(crate) fn meter(app: &SharedApp) -> anyhow::Result<()> {
    let tz = app.cfg().router.tz.parse::<chrono_tz::Tz>()?;
    app.db.meter_until(chrono::Utc::now(), tz)
}

async fn reconcile_credit(app: &SharedApp) {
    let vast_client = app.vast.lock().unwrap().clone();
    let Some(vast) = vast_client else { return };
    let Ok(user) = vast.current_user().await else { return };
    let date = crate::node::local_date(app);
    let metered = app.db.spent_today(&date);
    match app.db.record_credit(&date, user.credit) {
        Ok(spent) if metered > 0.05 && (spent - metered).abs() / metered.max(0.05) > 0.25 => {
            app.events.emit(&app.db, "budget_drift", None, None,
                &format!("Metering {metered:.2} $ vs. Kontostand-Deltas {spent:.2} $ — Differenz > 25 %"),
                &serde_json::json!({"metered":metered,"reconciled":spent}));
        }
        Err(e) => tracing::warn!(%e, "balance reconciliation failed"),
        _ => {}
    }
}

// ---------------------------------------------------------------- Offers

pub async fn search_slot_offers(
    app: &SharedApp,
    slot_id: i64,
    refresh: bool,
) -> anyhow::Result<Vec<OfferSnapshot>> {
    let Some(slot) = app.cfg().slot(slot_id).cloned() else {
        anyhow::bail!("unknown slot {slot_id}");
    };
    if !refresh {
        if let Some((ts, json)) = app.db.cached_offers(slot_id) {
            if let Some(t) = crate::db::parse_iso(&ts) {
                if (chrono::Utc::now() - t).num_seconds() < app.cfg().vast.offer_poll_s as i64 {
                    if let Ok(v) = serde_json::from_str::<Vec<OfferSnapshot>>(&json) {
                        return Ok(v);
                    }
                }
            }
        }
    }
    let vast_client = app.vast.lock().unwrap().clone();
    let Some(vast) = vast_client else {
        anyhow::bail!("vast api not configured");
    };
    // Beide Modi suchen und per offer-id mergen: min_bid (interruptible)
    // und dph_total (on-demand) landen im selben Snapshot.
    let disk = slot.disk_gb as f64;
    let interruptible = vast.search(&slot.search_query, true, disk).await.inspect_err(|e| {
        tracing::error!(%e, "vast bid-Suche fehlgeschlagen (API-Key? Filter?)");
    })?;
    let ondemand = vast.search(&slot.search_query, false, disk).await.inspect_err(|e| {
        tracing::error!(%e, "vast on-demand-Suche fehlgeschlagen (API-Key? Filter?)");
    })?;
    let mut by_id: std::collections::HashMap<i64, OfferSnapshot> = std::collections::HashMap::new();
    let snap_from = |o: &praxis_vast::Offer, min_bid: f64, dph: f64| OfferSnapshot {
        id: o.id,
        machine_id: o.machine_id,
        gpu_name: o.gpu_name.clone(),
        min_bid,
        dph_total: dph,
        storage_cost: o.storage_or(0.0),
        inet_down_cost: o.inet_down_cost.unwrap_or(0.0),
        cpu_ram_gb: o.cpu_ram_gb(),
        gpu_ram_gb: o.gpu_ram_gb(),
        disk_gb: o.disk_space,
        inet_down: o.inet_down,
        reliability2: o.reliability2.unwrap_or(0.0),
        disk_bw: o.disk_bw,
        num_gpus: o.num_gpus,
        cuda_max_good: o.cuda_max_good,
        cpu_cores: o.cpu_cores,
    };
    for o in &interruptible {
        let snap = snap_from(o, o.min_bid_or(0.0).max(0.0), o.dph_or(0.0));
        by_id.insert(snap.id, snap);
    }
    for o in &ondemand {
        let snap = snap_from(o, 0.0, o.dph_or(0.0));
        match by_id.get_mut(&snap.id) {
            Some(existing) => existing.dph_total = snap.dph_total,
            None => {
                by_id.insert(snap.id, snap);
            }
        }
    }
    let offers: Vec<OfferSnapshot> = by_id.into_values().collect();
    app.db.observe_offers(&offers)?;
    app.db.cache_offers(slot_id, &slot.search_query, &serde_json::to_string(&offers)?)?;
    Ok(offers)
}

pub(crate) fn best_candidate(app: &SharedApp, slot_id: i64) -> Option<OfferSnapshot> {
    let (_, json) = app.db.cached_offers(slot_id)?;
    let offers: Vec<OfferSnapshot> = serde_json::from_str(&json).ok()?;
    let slot = app.cfg().slot(slot_id)?.clone();
    let on_demand = slot.policy().mode == praxis_policy::SlotMode::OnDemand;
    let mode = if on_demand {Mode::OnDemand} else {Mode::Interruptible};
    let scores = if slot.performance.enabled && slot.performance.score.enabled {
        crate::performance::host_scores(app, &slot).unwrap_or_default()
    } else {HashMap::new()};
    // Blacklist: Hosts mit ≥ blacklist_after_fails Fails werden nie mehr
    // automatisch gemietet (machine_id 0 = unbekannt/alter Cache → neutral).
    let blacklisted: std::collections::HashSet<i64> = app
        .db
        .machine_stats()
        .into_iter()
        .filter(|m| app.cfg().vast.activate_blacklist && m.blacklisted)
        .map(|m| m.machine_id)
        .collect();
    // Pool-Redundanz: Maschinen, auf denen schon eine LIVE-Instanz dieses
    // Slots läuft, nicht erneut mieten — ein Host-Tod soll nicht gleich
    // zwei Pool-Mitglieder mitnehmen ("immer mindestens eine up").
    let used_machines: std::collections::HashSet<i64> = app
        .db
        .instances(false)
        .into_iter()
        .filter(|r| r.slot_id == slot_id && r.machine_id != 0)
        .map(|r| r.machine_id)
        .collect();
    offers
        .into_iter()
        .filter(|o| crate::catalog::assess(app, &slot, o, mode,
            praxis_policy::eligibility::rental_price(o, mode, &slot.bid), slot.disk_gb).eligible)
        // Preisfenster [rent_min_usd_h, ceiling_usd_h]: hartes Budget —
        // Offers außerhalb tauchen weder im Ranking noch im Dashboard-
        // Picker auf („20-€/h-Boxen" unmöglich, 21.09.).
        .filter(|o| {
            let rate = if on_demand { o.dph_total } else { o.min_bid };
            rate >= slot.bid.rent_min_usd_h && rate <= slot.bid.ceiling_usd_h
        })
        // On-Demand: dph_total ist der Preis (reine OD-Angebote haben min_bid=0);
        // Interruptible: min_bid (OD-only-Angebote raus, dph wäre Fehl-Ranking).
        .filter(|o| if on_demand { o.dph_total > 0.0 } else { o.min_bid > 0.0 })
        .filter(|o| o.machine_id == 0 || (!blacklisted.contains(&o.machine_id) && !used_machines.contains(&o.machine_id)))
        // On-Demand-Ranking über dph_total: min_bid nullen, damit score()
        // auf dph_total zurückfällt (merged Angebote tragen sonst min_bid).
        .map(|o| {
            if on_demand && o.min_bid > 0.0 {
                OfferSnapshot { min_bid: 0.0, ..o }
            } else {
                o
            }
        })
        .min_by(|a, b| {
            let rank = |o: &OfferSnapshot| {
                let score = scores.get(&(o.machine_id, crate::performance::allocation_key(o, slot.disk_gb))).copied();
                slot.performance.score.rank_cost(o.score(slot.disk_gb, slot.traffic_gb, 4.0), score)
            };
            rank(a).total_cmp(&rank(b)).then_with(|| a.id.cmp(&b.id))
        })
}

// ---------------------------------------------------------------- Snapshot

pub async fn build_snapshot(app: &SharedApp) -> anyhow::Result<Snapshot> {
    let tz: chrono_tz::Tz = app.cfg().router.tz.parse().unwrap_or(chrono_tz::Europe::Berlin);
    let now_local = chrono::Utc::now().with_timezone(&tz);
    let seconds_to_day_end = {
        let next_midnight = (now_local + chrono::Duration::days(1))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        (next_midnight - now_local.naive_local()).num_seconds().max(0)
    };
    let local_weekday = now_local.weekday().number_from_monday() as u8;
    let local_minutes = now_local.time().hour() as u32 * 60 + now_local.time().minute() as u32;
    let date = now_local.format("%Y-%m-%d").to_string();

    let rows = app.db.instances(false);
    let pins: HashMap<i64, Option<i64>> = app.db.slot_pins().into_iter().collect();

    let mut slots: Vec<SlotSnapshot> = Vec::new();
    let mut running_rate = 0.0;
    let mut storage_rate = 0.0;
    let instance_count = rows.len();

    for slot in &app.cfg().slots {
        let active_db = app.db.active_instance(slot.id);
        let desired_db = app.db.slot_desired(slot.id);
        // warm_hours: Fenster erzwungen Gewünscht.
        let warm = slot
            .warm_hours
            .as_deref()
            .and_then(praxis_policy::schedule::parse_window)
            .map(|w| w.contains(local_weekday, local_minutes))
            .unwrap_or(false);
        let desired_running = desired_db || warm;

        let mut instances = Vec::new();
        for row in rows.iter().filter(|r| r.slot_id == slot.id) {
            let state = parse_state(&row.state);
            let is_running = row.actual_status == "running" && state.is_active();
            let rate = match row.mode {
                Mode::Interruptible => row.bid_usd_h,
                _ => row.dph_total,
            };
            if is_running {
                running_rate += rate;
            }
            if !is_running && row.destroyed_at.is_none() {
                storage_rate += row.storage_usd_h;
            }
            let idle_since = {
                let last_req = app.traffic.snapshot(slot.id).last_request;
                let last_req = (last_req > 0)
                    .then(|| chrono::DateTime::from_timestamp(last_req, 0).unwrap_or_default());
                let healthy = row.healthy_since.as_deref().and_then(crate::db::parse_iso);
                // Hot-Swap: eine frisch geflippte Box darf nicht die Idle-Uhr der
                // VORHERIGEN Box erben (Stopp 33 s nach dem Flip, weil der letzte
                // Request noch gegen den Vorgänger lief). Idle zählt ab dem
                // Maximum aus letztem Request und eigenem Healthy-Zeitpunkt.
                match (last_req, healthy) {
                    (Some(lr), Some(h)) => Some(lr.max(h)),
                    (lr, h) => lr.or(h),
                }
            };
            let lifecycle: praxis_common::Lifecycle = serde_json::from_str(&row.lifecycle).unwrap_or_default();
            let resume_at = match &lifecycle {
                praxis_common::Lifecycle::Sleep { resume_at: Some(at), .. } => row.stopped_since.as_deref()
                    .and_then(crate::db::parse_iso).and_then(|since| crate::schedule_time::next_resume(since, at, app.cfg().router.tz.parse().unwrap_or(chrono_tz::UTC))),
                _ => None,
            };
            instances.push(InstanceSnapshot {
                vast_id: row.vast_id,
                offer_id: row.offer_id,
                machine_id: row.machine_id,
                gpu_name: row.gpu_name.clone(),
                role: row.role,
                slot_id: row.slot_id,
                mode: row.mode,
                lifecycle,
                state,
                actual_status: row.actual_status.clone(),
                intended_status: row.intended_status.clone(),
                healthy: row.ever_healthy,
                busy: row.busy,
                busy_reason: row.busy_reason.clone(),
                min_bid: row.min_bid,
                bid_usd_h: row.bid_usd_h,
                dph_total: row.dph_total,
                storage_usd_h: row.storage_usd_h,
                created_at: crate::db::parse_iso(&row.created_at).unwrap_or_default(),
                boot_started_at: app.db.boot_started_at(row.vast_id)?.unwrap_or_else(chrono::Utc::now),
                resume_at,
                idle_since,
                stopped_since: row.stopped_since.as_deref().and_then(crate::db::parse_iso),
                last_seen: None,
                pinned: row.pinned || pins.get(&slot.id) == Some(&Some(row.vast_id)),
            });
        }
        let in_flight = app.traffic.snapshot(slot.id).in_flight;
        slots.push(SlotSnapshot {
            id: slot.id,
            role: slot.role,
            name: slot.name.clone(),
            pinned: pins.get(&slot.id).cloned().flatten().is_some(),
            active_instance: active_db,
            instances,
            in_flight,
            last_traffic: app.db.last_traffic(slot.id),
            last_swap: app.db.last_swap(slot.id),
            candidate_offer: best_candidate(app, slot.id),
            desired_running,
            local_weekday,
            local_minutes_of_day: local_minutes,
        });
    }

    Ok(Snapshot {
        now: chrono::Utc::now(),
        seconds_to_day_end,
        spent_today_usd: app.db.spent_today(&date),
        spent_month_usd: app.db.spent_month(&date[..7]),
        slots,
        instance_count,
        running_rate_usd_h: running_rate,
        storage_rate_usd_h: storage_rate,
        auto_rent_enabled: app.db.auto_rent_enabled(),
    })
}

fn parse_state(s: &str) -> InstanceState {
    match s {
        "requested" => InstanceState::Requested,
        "provisioning" => InstanceState::Provisioning,
        "booting" => InstanceState::Booting,
        "agent_connected" => InstanceState::AgentConnected,
        "healthy" => InstanceState::Healthy,
        "draining" => InstanceState::Draining,
        "stopped" => InstanceState::Stopped,
        "destroyed" => InstanceState::Destroyed,
        "preempted" => InstanceState::Preempted,
        "unreachable" => InstanceState::Unreachable,
        _ => InstanceState::Failed,
    }
}

// ---------------------------------------------------------------- Actions

pub async fn apply_action(app: &SharedApp, action: &Action) -> anyhow::Result<()> {
    if !app.db.auto_rent_enabled() && matches!(action, Action::Create { .. } | Action::Start { .. }) {
        return Ok(());
    }
    match action {
        Action::Create { slot_id, offer_id, mode, price_usd_h, disk_gb, reason } => {
            // Kein Doppel-Create über das Pool-Ziel hinaus: Aktive Instanzen
            // (requested..draining) zählen gegen pool.warm — bei warm=1 ist
            // das der klassische „Backer wärmt schon"-Guard (kein Doppel-
            // Miete), bei warm=2 läuft die zweite Box parallel hoch.
            let slot_cfg = app.cfg().slot(*slot_id).cloned();
            let pool_warm = slot_cfg.map(|s| s.pool.warm).unwrap_or(1).max(1);
            let actives = app
                .db
                .instances(false)
                .into_iter()
                .filter(|r| {
                    r.slot_id == *slot_id
                        && matches!(
                            r.state.as_str(),
                            "requested" | "provisioning" | "booting" | "agent_connected" | "healthy" | "draining"
                        )
                })
                .count();
            let replacement = reason.starts_with("bid pressure") || reason.starts_with("optimize cost");
            let target = pool_warm + usize::from(replacement);
            if actives >= target {
                tracing::debug!(slot_id, actives, pool_warm, "create skipped: Pool-Ziel schon unterwegs/erreicht");
                return Ok(());
            }
            // Snapshot-Race: "wake"/"preempted: replace" wurden geplant, als der
            // Slot noch ohne brauchbare Instanz war. Wenn bis zur Ausführung ein
            // Ersatz bereits healthy geflippt ist, ist die Aktion überholt —
            // sonst mieten wir redundant (10:17-07-Box neben der Flip-Box).
            // Kostoptimierungs-Creates (bid pressure/optimiere Kosten) laufen
            // bewusst MIT gesundem Active und sind hier nicht erfasst.
            if reason.starts_with("wake") || reason.starts_with("preempted: replace") {
                let healthy_active = app
                    .db
                    .instances(false)
                    .into_iter()
                    .any(|r| r.slot_id == *slot_id && r.healthy && r.destroyed_at.is_none());
                if healthy_active {
                    tracing::info!(slot_id, %reason, "create skipped: snapshot-Race, bereits healthy aktiv");
                    return Ok(());
                }
            }
            let offers = search_slot_offers(app, *slot_id, true).await?;
            let Some(offer) = offers.into_iter().find(|o| o.id == *offer_id) else {
                anyhow::bail!("offer {offer_id} nicht mehr verfügbar");
            };
            let slot = app.cfg().slot(*slot_id).ok_or_else(|| anyhow::anyhow!("slot {slot_id} fehlt"))?.clone();
            let price = price_usd_h.unwrap_or(offer.min_bid * (1.0 + slot.bid.margin));
            let disk = disk_gb.unwrap_or(slot.disk_gb);
            let vast_id = create_instance(app, *slot_id, &offer, *mode, price, disk, praxis_common::Lifecycle::Auto, true, reason).await?;
            let _ = vast_id;
            Ok(())
        }
        Action::Start { instance_id, reason } => {
            crate::operations::start_automatic_instance(app, *instance_id, reason).await?;
            Ok(())
        }
        Action::Stop { instance_id, reason } => {
            crate::operations::automatic_request(app, *instance_id, "stop", reason).await?;
            Ok(())
        }
        Action::Destroy { instance_id, reason } => {
            // Warmup-Timeout = Host zu langsam/hängend fürs Hochziehen → Fail.
            if reason.contains("warmup timeout") || reason.contains("swap_failed") {
                if let Some(inst) = app.db.instance(*instance_id) {
                    record_machine_fail(app, inst.slot_id, *instance_id, inst.machine_id, "warmup timeout");
                    // WARUM nicht healthy? Diagnose MIT Service-Logs ziehen,
                    // solange die Agent-Session noch steht (CUDA-Arch, OOM,
                    // moe-cache-Guard …) — danach stirbt die Box mit dem Destroy.
                    emit_failure_diagnosis(
                        app,
                        *instance_id,
                        inst.slot_id,
                        "instance_failed",
                        &format!("{reason} (Slot {})", inst.slot_id),
                    )
                    .await;
                }
            }
            crate::operations::automatic_request(app, *instance_id, "destroy", reason).await?;
            Ok(())
        }
        Action::ChangeBid { instance_id, price_usd_h, reason } => {
            crate::operations::change_automatic_bid(app, *instance_id, *price_usd_h, reason).await
        }
        Action::FlipSlot { slot_id, from_instance, to_instance, reason } => {
            crate::operations::flip_slot(app, *slot_id, *from_instance, *to_instance, reason).await
        }
        Action::SwapOut { instance_id, destroy, reason } => {
            let inst = app.db.instance(*instance_id).ok_or_else(|| anyhow::anyhow!("unknown instance"))?;
            // A rejected/deferred FlipSlot must NEVER retire the still-active box.
            anyhow::ensure!(app.db.active_instance(inst.slot_id) != Some(*instance_id), "swap deferred: instance is still active");
            anyhow::ensure!(!inst.pinned && inst.mode != Mode::Manual, "swap blocked by pin/manual mode");
            if app.traffic.snapshot(inst.slot_id).in_flight > 0 || inst.busy {
                app.db.set_instance_state(*instance_id, "draining")?;
                Ok(())
            } else if *destroy {
                crate::operations::automatic_request(app, *instance_id, "destroy", reason).await
            } else {
                crate::operations::automatic_request(app, *instance_id, "stop", reason).await
            }
        }
        Action::Alert { kind, message } => {
            // Alert-Dedupe: gleiche Art höchstens alle 30 min — sonst spammt
            // budget_80 (Projektion über 80 %) jeden 30-s-Tick ins Event-Log.
            let recent = app
                .db
                .last_event_of_kind(kind)
                .map(|t| (chrono::Utc::now() - t).num_seconds() < 1800)
                .unwrap_or(false);
            if recent {
                return Ok(());
            }
            app.events.emit(&app.db, kind, None, None, message, &serde_json::json!({}));
            let url = &app.cfg().alerts.webhook_url;
            if !url.is_empty() {
                let url = url.clone();
                let body = serde_json::json!({"kind": kind, "message": message});
                let _ = tokio::spawn(async move {
                    let _ = reqwest::Client::new().post(&url).json(&body).timeout(std::time::Duration::from_secs(10)).send().await;
                });
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- Instanz-Op

/// Auto-Miete-Schalter (Dashboard/API): `false` = die Policy mietet und
/// startet nichts mehr (Create/Start werden gefiltert). Zusätzlich gilt beim
/// Ausschalten „aus ist aus“: gewünschte Zustände runternehmen und laufende,
/// nicht gepinnte Boxen stoppen (Disk bleibt warm; die Zerstörung übernimmt
/// danach die Idle-Regel). Einschalten mietet NICHT — der Nutzer weckt selbst.
// ------------------------------------------------------- Zeitpläne (Cron light)

/// `[[schedule]]`-Regeln abarbeiten: feuert maximal 1× pro Regel+Tag
/// (Dedup in der DB — Router-Restart refiret NICHT, wenn der Tag schon
/// bedient wurde). Gnadenfenster 90 min: war der Router beim Termin aus,
/// holt er nach. Außerhalb des Fensters = bewusst verpasst.
pub async fn run_schedules(app: &SharedApp) {
    let rules = app.cfg().schedule.clone();
    if rules.is_empty() {
        return;
    }
    let tz: chrono_tz::Tz = app
        .cfg()
        .router
        .tz
        .parse()
        .unwrap_or(chrono_tz::Europe::Berlin);
    let now_local = chrono::Utc::now().with_timezone(&tz);
    let weekday = now_local.weekday().number_from_monday() as u8;
    let minutes = now_local.time().hour() as u32 * 60 + now_local.time().minute() as u32;
    let date = now_local.format("%Y-%m-%d").to_string();

    for rule in &rules {
        let Some(rule_min) = rule.minutes() else { continue };
        // Noch nicht fällig / Tag passt nicht → weiter.
        if minutes < rule_min || minutes > rule_min + 90 {
            continue;
        }
        if !rule.matches_weekday(weekday) {
            continue;
        }
        // Dedup: einmal pro Regel+Tag gefeuert?
        let key = format!(
            "sched_fired:{}:{}:{}:{}",
            rule.time,
            rule.days,
            rule.action,
            rule.slots.clone().unwrap_or_default().iter().map(|i| i.to_string()).collect::<Vec<_>>().join(".")
        );
        if app.db.setting(&key).as_deref() == Some(&date) {
            continue;
        }
        if let Err(e) = app.db.set_setting(&key, &date) {
            tracing::warn!(%e, "schedule dedup persist fehlgeschlagen");
        }
        let slots = rule.slots.clone();
        let action = rule.action.trim().to_ascii_lowercase();
        tracing::info!(%action, time = %rule.time, "schedule-Regel feuert");
        let slot_ids = |all: &Option<Vec<i64>>| -> Vec<i64> {
            match all {
                Some(ids) => ids.clone(),
                None => app.cfg().slots.iter().map(|s| s.id).collect(),
            }
        };
        match action.as_str() {
            // Aus: Auto-Miete stoppt laufende Boxen (Disk bleibt).
            "sleep" | "sleep_all" => {
                if let Err(e) = set_auto_rent(app, false, "schedule").await {
                    tracing::warn!(%e, "scheduled sleep incomplete; provider operations remain pending");
                }
            }
            // An: Slots gewünscht → Reconciler mietet/warmt im selben Tick.
            "wake" | "rent" => {
                if let Err(e) = set_auto_rent(app, true, "schedule").await {
                    tracing::warn!(%e, "scheduled wake failed");
                    continue;
                }
                for slot in &app.cfg().slots {
                    if slots.as_ref().is_none_or(|ids| ids.contains(&slot.id)) {
                        let _ = app.db.set_slot_desired_audited(slot.id, true, "schedule wake");
                    }
                }
                app.reconcile_now.notify_one();
            }
            // Alles weg: zerstört ALLE nicht gepinnten Instanzen und schaltet
            // die Auto-Miete aus (kein Nachmietschritt im selben Atemzug).
            "destroy_all" => {
                if let Err(e) = set_auto_rent(app, false, "schedule destroy_all").await {
                    tracing::warn!(%e, "scheduled auto-rent off incomplete");
                }
                let n = match destroy_all_instances(app, slots.as_deref(), "schedule destroy_all").await {
                    Ok(n) => n,
                    Err(e) => { tracing::warn!(%e, "scheduled destroy incomplete; retries pending"); continue; }
                };
                app.events.emit(
                    &app.db,
                    "schedule_destroy_all",
                    None,
                    None,
                    &format!("Zeitplan {time}: alle Instanzen zerstört", time = rule.time),
                    &serde_json::json!({ "destroyed": n, "rule_time": rule.time }),
                );
            }
            // Slot(s) locken: aktive Instanz pinnen + Slot-Pin — kein
            // Auto-Rent/Replace/Destroy bis unlock. Für „Box soll über
            // Nacht genau SO bleiben"-Szenarien.
            "lock" => {
                let _lock = app.management.lock().await;
                for sid in slot_ids(&slots) {
                    let insts = app.db.instances(false);
                    let cand = app
                        .db
                        .active_instance(sid)
                        .or_else(|| {
                            insts
                                .iter()
                                .find(|r| {
                                    r.slot_id == sid && r.destroyed_at.is_none()
                                        && matches!(r.state.as_str(), "healthy" | "booting" | "agent_connected" | "provisioning" | "requested")
                                })
                                .map(|r| r.vast_id)
                        });
                    match cand {
                        Some(vid) => {
                            let _ = app.db.update_instance_pinned(vid, true);
                            let _ = app.db.set_slot_pin(sid, Some(vid));
                            app.events.emit(&app.db, "locked", Some(sid), Some(vid), "schedule lock", &serde_json::json!({ "rule_time": rule.time }));
                        }
                        None => {
                            app.events.emit(&app.db, "schedule_noop", Some(sid), None, &format!("schedule {}: kein Kandidat zum Locken", rule.time), &serde_json::json!({}));
                        }
                    }
                }
            }
            "unlock" => {
                let _lock = app.management.lock().await;
                for sid in slot_ids(&slots) {
                    let _ = app.db.set_slot_pin(sid, None);
                    for inst in app.db.instances(false) {
                        if inst.slot_id == sid && inst.pinned {
                            let _ = app.db.update_instance_pinned(inst.vast_id, false);
                        }
                    }
                    app.events.emit(&app.db, "unlocked", Some(sid), None, "schedule unlock", &serde_json::json!({ "rule_time": rule.time }));
                }
            }
            // Pin/unpin = Slot-Pin ohne Instanz-Brandsatz (klassisches Pin).
            "pin" => {
                let _lock = app.management.lock().await;
                for sid in slot_ids(&slots) {
                    if let Some(active) = app.db.active_instance(sid) {
                        let _ = app.db.update_instance_pinned(active, true);
                        let _ = app.db.set_slot_pin(sid, Some(active));
                        app.events.emit(&app.db, "pinned", Some(sid), Some(active), "schedule pin", &serde_json::json!({ "rule_time": rule.time }));
                    }
                }
            }
            "unpin" => {
                let _lock = app.management.lock().await;
                for sid in slot_ids(&slots) {
                    let _ = app.db.set_slot_pin(sid, None);
                    for inst in app.db.instances(false) {
                        if inst.slot_id == sid && inst.pinned {
                            let _ = app.db.update_instance_pinned(inst.vast_id, false);
                        }
                    }
                    app.events.emit(&app.db, "unpinned", Some(sid), None, "schedule unpin", &serde_json::json!({ "rule_time": rule.time }));
                }
            }
            // Tagesbudget override (€, persistiert in DB bis gelöscht):
            // [[schedule]] time="00:05" action="budget" hard_eur=2.0
            "budget" => {
                match rule.soft_eur {
                    Some(v) => { let _ = app.db.set_setting("budget_override_soft_eur", &v.to_string()); }
                    None => { let _ = app.db.set_setting("budget_override_soft_eur", ""); }
                }
                match rule.hard_eur {
                    Some(v) => { let _ = app.db.set_setting("budget_override_hard_eur", &v.to_string()); }
                    None => { let _ = app.db.set_setting("budget_override_hard_eur", ""); }
                }
                app.events.emit(
                    &app.db,
                    "budget_override",
                    None,
                    None,
                    &format!("Zeitplan {}: Budget-Override soft={:?} hard={:?}", rule.time, rule.soft_eur, rule.hard_eur),
                    &serde_json::json!({ "soft_eur": rule.soft_eur, "hard_eur": rule.hard_eur }),
                );
                app.reconcile_now.notify_one();
            }
            "budget_reset" => {
                let _ = app.db.set_setting("budget_override_soft_eur", "");
                let _ = app.db.set_setting("budget_override_hard_eur", "");
                app.events.emit(
                    &app.db,
                    "budget_override",
                    None,
                    None,
                    "Zeitplan: Budget-Override gelöscht — config.toml-Werte gelten wieder",
                    &serde_json::json!({ "cleared": true }),
                );
            }
            _ => {}
        }
    }
}

/// Policy-Konfig inkl. DB-Budget-Override (schedule action="budget"):
/// Override liegt in den Settings (persistiert, überlebt Router-Restart),
/// soft/hard leer = Override-Komponente gelöscht (config.toml gilt).
pub fn effective_policy(app: &SharedApp) -> praxis_policy::PolicyConfig {
    let mut pol = app.cfg().policy_config();
    if let Some(v) = app.db.setting("budget_override_soft_eur").and_then(|v| v.trim().parse::<f64>().ok()) {
        pol.budget.daily_soft_eur = v;
    }
    if let Some(v) = app.db.setting("budget_override_hard_eur").and_then(|v| v.trim().parse::<f64>().ok()) {
        pol.budget.daily_hard_eur = v;
    }
    pol
}

/// Alle nicht gepinnten, nicht zerstörten Instanzen (optional gefiltert
/// nach Slot-IDs) zerstören. Gibt die Anzahl zurück. Bewusste API-Aktion
/// (Dashboard-Button/Cron) — Lock/Pin bleibt gewollt bestehen.
pub async fn destroy_all_instances(
    app: &SharedApp,
    slot_ids: Option<&[i64]>,
    reason: &str,
) -> anyhow::Result<usize> {
    let mut n = 0;
    let mut failures = Vec::new();
    for slot in &app.cfg().slots {
        if slot_ids.is_none_or(|ids| ids.contains(&slot.id)) {
            app.db.set_slot_desired_audited(slot.id, false, reason)?;
        }
    }
    for inst in app.db.instances(false) {
        if inst.destroyed_at.is_some() || inst.pinned {
            continue;
        }
        if let Some(ids) = slot_ids {
            if !ids.contains(&inst.slot_id) {
                continue;
            }
        }
        match destroy_instance(app, inst.vast_id, reason).await {
            Ok(()) => n += 1,
            Err(e) => failures.push(format!("{}: {e}", inst.vast_id)),
        }
    }
    anyhow::ensure!(failures.is_empty(), "destroyed {n}; pending failures: {}", failures.join("; "));
    Ok(n)
}

pub async fn set_auto_rent(app: &SharedApp, enabled: bool, source: &str) -> anyhow::Result<()> {
    {
        // Wait for any admitted rental/start to become visible before stopping it.
        let _lock = app.management.lock().await;
        if enabled && app.db.auto_rent_enabled() { return Ok(()); }
        app.db.set_auto_rent(enabled)?;
    }
    if enabled {
        app.events.emit(
            &app.db,
            "auto_rent_enabled",
            None,
            None,
            &format!("Auto-Miete angeschaltet ({source})"),
            &serde_json::json!({}),
        );
        app.reconcile_now.notify_one();
        return Ok(());
    }
    for slot in &app.cfg().slots {
        app.db.set_slot_desired_audited(slot.id, false, "auto_rent off")?;
    }
    let mut failures = Vec::new();
    for inst in app.db.instances(false) {
        if inst.pinned {
            continue;
        }
        let runningish = matches!(
            inst.state.as_str(),
            "requested" | "provisioning" | "booting" | "agent_connected" | "healthy" | "draining"
        );
        if (runningish || inst.actual_status == "running") && inst.destroyed_at.is_none() {
            if let Err(e) = stop_instance(app, inst.vast_id, "auto-rent off: stop requested, disk retained").await {
                failures.push(format!("{}: {e}", inst.vast_id));
            }
        }
    }
    app.events.emit(
        &app.db,
        "auto_rent_disabled",
        None,
        None,
        &format!("Auto-Miete ausgeschaltet ({source}) — Stop angefordert, {} Fehler bleiben zur Wiederholung", failures.len()),
        &serde_json::json!({"pending_errors": failures}),
    );
    anyhow::ensure!(failures.is_empty(), "auto-rent off; stops pending: {}", failures.join("; "));
    Ok(())
}

pub fn mint_node_token() -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

async fn mint_netbird_key(app: &SharedApp, name: &str) -> anyhow::Result<String> {
    let token = app.cfg().netbird_api_token();
    if token.is_empty() {
        // statischer Key aus Config (Fallback, Variante B).
        let k = app.cfg().netbird.setup_key.clone();
        if k.is_empty() {
            anyhow::bail!("kein NetBird setup key konfiguriert");
        }
        return Ok(k);
    }
    let client = reqwest::Client::new();
    let base = app.cfg().netbird.api_url.trim_end_matches('/').to_string();

    // Gruppennamen (Config) → Management-IDs auflösen (auto_groups will IDs).
    // `groups` (Liste) schlägt das Legacy-Feld `group` (String).
    let mut wanted: Vec<String> = app.cfg().netbird.groups.clone();
    if wanted.is_empty() {
        let legacy = app.cfg().netbird.group.clone();
        if !legacy.is_empty() {
            wanted.push(legacy);
        }
    }
    let group_ids: Vec<String> = if wanted.is_empty() {
        vec![]
    } else {
        let resp = client
            .get(format!("{base}/api/groups"))
            .bearer_auth(&token)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let groups: Vec<serde_json::Value> = r.json().await.unwrap_or_default();
                wanted
                    .iter()
                    .map(|w| {
                        groups
                            .iter()
                            .find(|g| g.get("name").and_then(|n| n.as_str()) == Some(w.as_str()))
                            .and_then(|g| g.get("id").and_then(|i| i.as_str()))
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| w.clone()) // evtl. direkt ID konfiguriert
                    })
                    .collect()
            }
            _ => wanted,
        }
    };

    let mut key = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut key);
    let key = format!("sk-{}", hex::encode(key));
    let mut body = serde_json::json!({
        "name": name,
        "type": "one-off",
        "key": key,
        "usage_limit": 1,
        "expires_in": 3600,
    });
    if !group_ids.is_empty() {
        body["auto_groups"] = serde_json::json!(group_ids);
    }
    let resp = client
        .post(format!("{base}/api/setup-keys"))
        .bearer_auth(&token)
        .json(&body)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => {
            let j: serde_json::Value = r.json().await?;
            Ok(j.get("key").and_then(|k| k.as_str()).unwrap_or(&key).to_string())
        }
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            tracing::warn!(status = %status, "netbird mint failed ({text:.200}), fallback static");
            let k = app.cfg().netbird.setup_key.clone();
            if k.is_empty() {
                anyhow::bail!("netbird mint fehlgeschlagen und kein statischer Key");
            }
            Ok(k)
        }
        Err(e) => {
            tracing::warn!(%e, "netbird unreachable, fallback static");
            let k = app.cfg().netbird.setup_key.clone();
            if k.is_empty() {
                anyhow::bail!("netbird unreachable und kein statischer Key");
            }
            Ok(k)
        }
    }
}

pub async fn create_instance(
    app: &SharedApp,
    slot_id: i64,
    offer: &OfferSnapshot,
    mode: Mode,
    price_usd_h: f64,
    disk_gb: i64,
    lifecycle: praxis_common::Lifecycle,
    automatic: bool,
    reason: &str,
) -> anyhow::Result<i64> {
    let _lock = app.management.lock().await;
    anyhow::ensure!(!automatic || app.db.auto_rent_enabled(), "auto-rent disabled");
    meter(app)?;
    anyhow::ensure!(disk_gb > 0 && price_usd_h.is_finite() && price_usd_h > 0.0, "invalid rental price/disk");
    let rate = if mode == Mode::Interruptible { price_usd_h } else { offer.dph_total };
    let storage_usd_h = offer.storage_cost * disk_gb as f64 / 720.0;
    let slot = app.cfg().slot(slot_id).ok_or_else(|| anyhow::anyhow!("slot {slot_id} fehlt"))?.clone();
    let eligibility = crate::catalog::assess(app, &slot, offer, mode, price_usd_h, disk_gb);
    anyhow::ensure!(eligibility.eligible, "offer rejected: {}", eligibility.reasons.join("; "));
    crate::operations::check_admission_with_startup(app, slot_id, rate, storage_usd_h, None, eligibility.startup_traffic_usd)?;
    let vast_client = app.vast.lock().unwrap().clone();
    let Some(vast) = vast_client else {
        anyhow::bail!("vast api not configured");
    };
    let node_token = mint_node_token();
    let tok8 = &node_token[..8];
    let hostname = format!("gpu-{}-{}", slot.role, tok8);
    let nb_key = mint_netbird_key(app, &format!("praxis-{tok8}")).await?;

    let mut env = serde_json::Map::new();
    env.insert("NB_SETUP_KEY".into(), serde_json::json!(nb_key));
    if !app.cfg().netbird.management_url.is_empty() {
        env.insert("NB_MANAGEMENT_URL".into(), serde_json::json!(app.cfg().netbird.management_url));
    }
    env.insert("NB_HOSTNAME".into(), serde_json::json!(hostname));
    env.insert("NB_SOCKS5_LISTENER_PORT".into(), serde_json::json!("1080"));
    env.insert("ROUTER_URL".into(), serde_json::json!(crate::config::router_call_url(&app.cfg())));
    env.insert("PRAXIS_NODE_TOKEN".into(), serde_json::json!(node_token));
    // Endpoint-Check ("check_if_works"): Der Agent probed lokal (Loopback)
    // die Health-Pfade der Slot-Services — healthy heißt dann wirklich
    // „llama /health 200 NACH dem Model-Download / ComfyUI-Nodes geladen",
    // nicht nur „Prozess läuft". Ohne URLs war der Agent optimistisch
    // Healthy (Spike-Flip war Glück, kein Gating).
    let health_urls: Vec<String> = slot
        .services
        .iter()
        .filter_map(|(_name, svc)| {
            svc.health
                .as_ref()
                .map(|h| format!("http://127.0.0.1:{}{}", svc.port, h))
        })
        .collect();
    if !health_urls.is_empty() {
        env.insert("PRAXIS_AGENT_HEALTH_URLS".into(), serde_json::json!(health_urls.join(",")));
    }
    for (k, v) in &slot.env {
        env.insert(k.clone(), serde_json::json!(v));
    }
    if slot.performance.enabled && slot.performance.benchmark.enabled {
        env.insert("PRAXIS_AGENT_ALLOW_BENCHMARK".into(), serde_json::json!("1"));
    }

    // Unique per rental: label-based recovery must not select an old contract.
    let label = format!("praxis-{}-s{}-{tok8}", slot.role, slot_id);
    let params = CreateInstanceParams {
        client: "me",
        image: &slot.image,
        // On-Demand-Modus: KEIN price-Feld senden = echter On-Demand-Vertrag
        // zum Listenpreis (nicht preemptbar). Ein Gebot AUF dph_total wäre
        // weiterhin interruptible (is_bid=True, s. vastai-SDK-Doku).
        price: (mode == praxis_common::Mode::Interruptible).then_some(price_usd_h),
        disk: Some(disk_gb),
        label: Some(&label),
        template_hash_id: None,
        onstart_cmd: None,
        runtype: Some("args"), // Plain-Docker-Run des Image-Entrypoints
        env: serde_json::Value::Object(env),
        extra: None,
        image_login: None,
        python_utf8: false,
        lang_utf8: false,
        use_jupyter_lab: false,
        jupyter_dir: None,
        force: false,
        cancel_unavail: false,
        user: None,
    };

    let vast_id = vast.create(offer.id, &params).await?;
    let row = crate::db::InstanceRow {
        vast_id,
        slot_id,
        role: slot.role,
        node_token,
        offer_id: offer.id,
        machine_id: offer.machine_id,
        gpu_name: offer.gpu_name.clone(),
        nb_ip: None,
        image: slot.image.clone(),
        mode,
        lifecycle: serde_json::to_string(&lifecycle)?,
        pinned: false,
        actual_status: "loading".into(),
        intended_status: "running".into(),
        state: "requested".into(),
        healthy: false,
        ever_healthy: false,
        busy: false,
        busy_reason: String::new(),
        min_bid: offer.min_bid,
        bid_usd_h: price_usd_h,
        dph_total: offer.dph_total,
        storage_usd_h,
        created_at: crate::db::now_iso(),
        healthy_since: None,
        stopped_since: None,
        destroyed_at: None,
        label,
    };
    app.db.insert_instance(&row)?;
    app.db.save_rental_facts(vast_id, &OfferSnapshot {disk_gb: disk_gb as f64, ..offer.clone()}, &crate::performance::runtime_key(&slot, &slot.image))?;
    let _ = app.db.set_slot_desired_audited(slot_id, true, "create_instance");
    app.events.emit(
        &app.db,
        "instance_created",
        Some(slot_id),
        Some(vast_id),
        &format!(
            "{} gemietet: offer {} ({}) {} $/h, disk {disk_gb} GB — {reason}",
            slot.name, offer.id, offer.gpu_name, price_usd_h
        ),
        &serde_json::json!({"price": price_usd_h, "mode": mode, "offer": offer.id}),
    );
    app.reconcile_now.notify_one();
    Ok(vast_id)
}

/// NetBird-Peer nach Hostname (gpu-<role>-<tok8>) suchen und löschen.
/// Match wie im Connector: name ODER hostname-Feld ODER dns_label-Präfix.
pub(crate) async fn netbird_delete_peer(app: &SharedApp, hostname: &str) -> anyhow::Result<()> {
    let token = app.cfg().netbird_api_token();
    if token.is_empty() {
        return Ok(());
    }
    let base = app.cfg().netbird.api_url.trim_end_matches('/').to_string();
    let resp = reqwest::Client::new()
        .get(format!("{base}/api/peers"))
        .bearer_auth(&token)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;
    let peers: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
    let exp = hostname.to_ascii_lowercase();
    let peer_id = peers.iter().find_map(|p| {
        let name = p.get("name").and_then(|n| n.as_str())?;
        let ip = p.get("ip").and_then(|i| i.as_str())?;
        let hostname_f = p.get("hostname").and_then(|h| h.as_str()).unwrap_or("");
        let dns = p.get("dns_label").and_then(|d| d.as_str()).unwrap_or("");
        let hit = name.eq_ignore_ascii_case(hostname)
            || hostname_f.eq_ignore_ascii_case(hostname)
            || dns.starts_with(&format!("{exp}."))
            || dns.starts_with(&format!("{exp}-"));
        if hit {
            p.get("id").and_then(|i| i.as_str()).map(|s| (s.to_string(), ip.to_string()))
        } else {
            None
        }
    });
    let Some((peer_id, ip)) = peer_id else {
        tracing::debug!(%hostname, "kein NetBird-Peer zum Löschen gefunden");
        return Ok(());
    };
    let resp = reqwest::Client::new()
        .delete(format!("{base}/api/peers/{peer_id}"))
        .bearer_auth(&token)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;
    if resp.status().is_success() {
        tracing::info!(%hostname, %ip, "NetBird-Peer gelöscht (destroy-Hygiene)");
    } else {
        anyhow::bail!("netbird peer delete {}: {}", peer_id, resp.status());
    }
    Ok(())
}