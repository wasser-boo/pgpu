//! REST-API (Bearer `ROUTER_TOKEN`): State, Budget, Offers, Slots,
//! Instances, Events-SSE, Terminal-WS.

use crate::state::AppCtx;
use crate::state::SharedApp;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{Path, Query, Request};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseItem, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use praxis_common::{Mode, Role};
use serde::Deserialize;
use std::collections::HashMap;

use crate::node::bearer;
use futures::{SinkExt, StreamExt};

pub fn check_token(app: &SharedApp, req: &Request) -> bool {
    let token = app.cfg().router_token();
    if token.is_empty() {
        return false; // Missing configuration must never grant administrator access.
    }
    match bearer(req.headers()) {
        Some(t) if t == token => true,
        _ => false,
    }
}

macro_rules! guarded {
    ($app:expr, $req:expr) => {
        if !check_token(&$app.0, &$req) {
            return (StatusCode::UNAUTHORIZED, "bad token").into_response();
        }
    };
}

// ------------------------------------------------------------- Config (Hot-Reload)

/// `POST /api/v1/sleep_all` — Auto-Miete aus + alle laufenden Boxen stoppen
/// (Disk bleibt). Gegenstück zu destroy_all fürs sanfte Runterfahren.
pub async fn sleep_all(app: AppCtx, req: Request) -> Response {
    guarded!(app, req);
    if let Err(e) = crate::reconciler::set_auto_rent(&app.0, false, "api: sleep_all").await {
        return (StatusCode::BAD_GATEWAY, e.to_string()).into_response();
    }
    (StatusCode::OK, "sleeping: auto_rent off, instances stopped").into_response()
}

/// `POST /api/v1/destroy_all` — ALLE nicht gepinnten Instanzen zerstören
/// (optional body {"slots":[1,2]} oder {"auto_rent":false}). Fürs manuelle
/// Aufräumen + als Ziel für externe Cronjobs.
pub async fn destroy_all(app: AppCtx, req: Request) -> Response {
    guarded!(app, req);
    #[derive(Deserialize, Default)]
    #[serde(deny_unknown_fields)]
    struct DestroyRequest { slots: Option<Vec<i64>>, auto_rent: Option<bool> }
    let Ok(body) = axum::body::to_bytes(req.into_body(), 64 * 1024).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response();
    };
    let payload = if body.is_empty() { DestroyRequest::default() } else {
        let Ok(payload) = serde_json::from_slice::<DestroyRequest>(&body) else {
            return (StatusCode::BAD_REQUEST, "invalid destroy request").into_response();
        };
        payload
    };
    if payload.slots.as_ref().is_some_and(|ids| ids.iter().any(|id| app.cfg().slot(*id).is_none())) {
        return (StatusCode::BAD_REQUEST, "unknown slot").into_response();
    }
    if let Some(enabled) = payload.auto_rent {
        // Persist the switch without a redundant stop-before-destroy round trip.
        let _lock = app.management.lock().await;
        if let Err(e) = app.db.set_auto_rent(enabled) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }
    match crate::reconciler::destroy_all_instances(&app.0, payload.slots.as_deref(), "api: destroy_all").await {
        Ok(n) => (StatusCode::OK, format!("destroyed {n} instances")).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

/// `GET /api/v1/config` — Rohtext der config.toml (Bearer-geschützt).
pub async fn config_get(app: AppCtx, req: Request) -> Response {
    guarded!(app, req);
    Json(serde_json::json!({
        "path": app.config_path,
        "raw": app.cfg_raw(),
    }))
    .into_response()
}

/// `PUT /api/v1/config` {"raw": "<toml>"} — validiert, schreibt atomar auf
/// Disk, tauscht live (slots/budget/limits/policy sofort wirksam) und
/// weckt den Reconciler. `[router]/[vast]/[netbird]/[stt]`-Basisdaten
/// (Bind, Ports, Keys, URLs) gelten erst nach Router-Neustart — sie
/// werden gespeichert, aber im laufenden Prozess nicht umgebogen.
pub async fn config_put(app: AppCtx, req: Request) -> Response {
    guarded!(app, req);
    let body = axum::body::to_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let Some(raw) = payload.get("raw").and_then(|r| r.as_str()).map(|s| s.to_string()) else {
        return (StatusCode::BAD_REQUEST, "missing raw").into_response();
    };
    match apply_config(&app.0, &raw).await {
        Ok(msg) => Json(serde_json::json!({ "ok": true, "message": msg })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e}")).into_response(),
    }
}

/// Geteilter Apply-Pfad (API + Dashboard-Form): parse → validate →
/// atomar schreiben → live-swap → Slot-DB sync → Event + Reconciler-Wake.
/// `new_cfg` behält bereits aufgelöste Werte (bind_ip/router_nb_ip != "auto")
/// der laufenden Konfig, damit ein Edit am File den laufenden Router nicht
/// dekonfiguriert.
pub async fn apply_config(app: &SharedApp, raw: &str) -> anyhow::Result<String> {
    let mut new_cfg = crate::config::Config::load_str(raw)?; // wirft bei TOML-/Validierungs-Fehler
    let _management = app.management.lock().await;
    let cur = app.cfg();
    // "auto"-Auflösungen der laufenden Config übernehmen (detect läuft nur
    // beim Boot; die laufenden Werte sind bereits real).
    if new_cfg.router.bind_ip == "auto" {
        new_cfg.router.bind_ip = cur.router.bind_ip.clone();
    }
    if new_cfg.netbird.router_nb_ip == "auto" {
        new_cfg.netbird.router_nb_ip = cur.netbird.router_nb_ip.clone();
    }
    // Secrets: leere Felder → Werte der laufenden Config (Env-Override
    // greift eh in router_token()/vast_api_key()/netbird_api_token()).
    if new_cfg.router.token.is_empty() {
        new_cfg.router.token = cur.router.token.clone();
    }
    if new_cfg.vast.api_key.is_empty() {
        new_cfg.vast.api_key = cur.vast.api_key.clone();
    }
    if new_cfg.netbird.api_token.is_empty() {
        new_cfg.netbird.api_token = cur.netbird.api_token.clone();
    }

    new_cfg.validate_auth()?;
    // Slot-DB synchronisieren (neue Slots, geänderte Rollen/Namen).
    app.db.init_slots(&new_cfg)?;
    // Clear stale offers before writing: a ledger failure must leave the saved
    // file and active config unchanged, not fail after a successful rename.
    app.db.invalidate_offers()?;
    // Atomar schreiben: tmp + rename — der Router startet nach einem Crash
    // nie mit halber TOML.
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = format!("{}.tmp-{:016x}", app.config_path, rand::random::<u64>());
    let save = (|| -> std::io::Result<()> {
        // Never turn private router/webhook credentials into a mode-0644 file.
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        file.write_all(raw.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, &app.config_path)
    })();
    if save.is_err() { let _ = std::fs::remove_file(&tmp); }
    save?;

    let slots: Vec<String> = new_cfg.slots.iter().map(|s| format!("#{} {}", s.id, s.name)).collect();
    app.cfg_swap(new_cfg);
    app.events.emit(
        &app.db,
        "config_reloaded",
        None,
        None,
        "config.toml live neu geladen (Dashboard/API)",
        &serde_json::json!({ "slots": slots, "path": app.config_path }),
    );
    // Reconciler sofort ticken lassen (neue warmups/ceilings greifen in <30s).
    app.reconcile_now.notify_one();
    Ok(format!("ok — {} Slots aktiv", slots.len()))
}

/// Authenticated test of the saved webhook. No URL/message override and no GPU operations.
pub async fn webhook_test(app: AppCtx, req: Request) -> Response {
    guarded!(app, req);
    match crate::notifications::test(&app.0).await {
        Ok(report) => (if report.ok { StatusCode::OK } else { StatusCode::BAD_GATEWAY }, Json(report)).into_response(),
        Err(error) => (error.status(), Json(serde_json::json!({"ok":false,"error":error.to_string()}))).into_response(),
    }
}

// ------------------------------------------------------------- State

/// Readiness of the control plane, not a promise that a cold GPU is available.
/// No secrets are exposed; liveness is separately served at /healthz.
pub async fn ready(app: AppCtx) -> Response {
    use std::sync::atomic::Ordering;
    let last = app.last_reconcile.load(Ordering::Relaxed);
    let max_age = app.cfg().vast.poll_interval_s.max(5).saturating_mul(3).saturating_add(60);
    let age = chrono::Utc::now().timestamp().saturating_sub(last);
    let ready = !app.shutting_down.load(Ordering::Relaxed) && last > 0 && age >= 0
        && age as u64 <= max_age && app.db.budget_totals(&crate::node::local_date(&app.0)).is_ok();
    (if ready { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE },
        if ready { "ready" } else { "not ready" }).into_response()
}

pub async fn state(app: AppCtx, req: Request) -> Response {
    guarded!(app, req);
    axum::Json(crate::dashboard::state_json(&app.0)).into_response()
}

pub async fn budget(app: AppCtx, req: Request) -> Response {
    guarded!(app, req);
    let data=budget_json(&app.0);
    (if data.get("error").is_some() { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK },axum::Json(data)).into_response()
}

pub fn budget_json(app: &SharedApp) -> serde_json::Value {
    let date = crate::node::local_date(app);
    let Ok((spent,month))=app.db.budget_totals(&date) else { return serde_json::json!({"error":"budget ledger unavailable"}); };
    let metered=app.db.metered_totals(&date).ok();
    serde_json::json!({
        "date": date,
        "spent_today_usd": spent,
        "spent_month_usd": month,
        "metered_today_usd": metered.map(|m|m.0),
        "metered_month_usd": metered.map(|m|m.1),
        "vast_usage": crate::billing::status(app),
        "soft_eur": crate::reconciler::effective_policy(&app).budget.daily_soft_eur,
        "hard_eur": crate::reconciler::effective_policy(&app).budget.daily_hard_eur,
        "monthly_eur": app.cfg().budget.monthly_eur,
        "usd_per_eur": app.cfg().budget.usd_per_eur,
    })
}

pub async fn events_json(app: AppCtx, Query(params): Query<HashMap<String, String>>, req: Request) -> Response {
    guarded!(app, req);
    let limit = params.get("limit").and_then(|l| l.parse::<i64>().ok()).unwrap_or(50);
    let instance = params.get("instance_id").and_then(|i| i.parse::<i64>().ok());
    axum::Json(app.db.events(limit, instance)).into_response()
}

/// SSE `/api/v1/events/stream`
pub async fn events_sse(app: AppCtx, req: Request) -> Response {
    // EventSource kann KEINE Authorization-Header setzen — Session-Cookie
    // (== Router-Token) zusaetzlich zum Bearer akzeptieren, sonst ist der
    // Live-Event-Feed im Browser stumm (401).
    if !crate::dashboard::session_ok(&app.0, req.headers()) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let rx = app.events.subscribe();
    Sse::new(sse_stream(rx)).keep_alive(KeepAlive::default()).into_response()
}

fn sse_stream(
    rx: tokio::sync::broadcast::Receiver<crate::events::SseEvent>,
) -> impl futures::Stream<Item = Result<SseItem, std::convert::Infallible>> {
    futures::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(e) => {
                let data = serde_json::to_string(&e).unwrap_or_default();
                Some((Ok(SseItem::default().event(e.kind).data(data)), rx))
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                Some((Ok(SseItem::default().event("lagged").data(format!("{{\"lagged\":{n}}}"))), rx))
            }
            Err(_) => None,
        }
    })
}

// ------------------------------------------------------------- Offers

pub async fn offers(app: AppCtx, Query(params): Query<HashMap<String, String>>, req: Request) -> Response {
    guarded!(app, req);
    let Some(slot_id) = params.get("slot").and_then(|s| s.parse::<i64>().ok()) else {
        return (StatusCode::BAD_REQUEST, "missing slot param").into_response();
    };
    let refresh = params.contains_key("refresh");
    match crate::reconciler::search_slot_offers(&app.0, slot_id, refresh).await {
        Ok(offers) => {
            let cfg = app.cfg();
            let slot = cfg.slot(slot_id).unwrap();
            let mode = match params.get("mode").map(String::as_str) {
                Some("on_demand") => Mode::OnDemand,
                Some("interruptible") => Mode::Interruptible,
                Some(_) => return (StatusCode::BAD_REQUEST, "mode must be interruptible|on_demand").into_response(),
                None if slot.policy().mode == praxis_policy::SlotMode::OnDemand => Mode::OnDemand,
                None => Mode::Interruptible,
            };
            let scores = crate::performance::host_scores(&app.0, slot).unwrap_or_default();
            axum::Json(offers.into_iter().map(|o| {
                let check = crate::catalog::assess(&app.0,slot,&o,mode,praxis_policy::eligibility::rental_price(&o,mode,&slot.bid),slot.disk_gb);
                let mut value = serde_json::to_value(&o).unwrap();
                value["assessment"] = serde_json::to_value(check).unwrap();
                value["performance_score"] = serde_json::json!(scores.get(&(o.machine_id,crate::performance::allocation_key(&o,slot.disk_gb))));
                value
            }).collect::<Vec<_>>()).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("{e}")).into_response(),
    }
}

// ------------------------------------------------------------- Machines

/// Host-Bilanz (Fails + Blacklist) — Basis der Wake-Replace-Vermeidung.
pub async fn machines(app: AppCtx, req: Request) -> Response {
    guarded!(app, req);
    axum::Json(app.db.machine_stats()).into_response()
}

pub async fn machine_action(app: AppCtx, Path((machine_id, action)): Path<(i64, String)>, req: Request) -> Response {
    guarded!(app, req);
    match crate::catalog::machine_action(&app.0, machine_id, &action).await {
        Ok(()) => (StatusCode::OK, action).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

/// Raw history, newest first. Page using before=<last id>; bounded to 1000 rows.
pub async fn performance_history(app: AppCtx, Query(q): Query<HashMap<String,String>>, req: Request) -> Response {
    guarded!(app, req);
    let number=|name: &str| -> Result<Option<i64>, std::num::ParseIntError> {q.get(name).map(|s|s.parse()).transpose()};
    let (Ok(slot),Ok(machine),Ok(before),Ok(limit))=(number("slot"),number("machine_id"),number("before"),number("limit")) else {
        return (StatusCode::BAD_REQUEST,"invalid integer filter").into_response();
    };
    let limit=limit.unwrap_or(100);
    if !(1..=1000).contains(&limit) {return (StatusCode::BAD_REQUEST,"limit must be 1..1000").into_response();}
    match app.db.performance_samples(slot,machine,before,limit as usize) {
        Ok(rows)=>axum::Json(rows).into_response(),
        Err(e)=>(StatusCode::INTERNAL_SERVER_ERROR,e.to_string()).into_response(),
    }
}

pub async fn performance_summary(app: AppCtx, req: Request) -> Response {
    guarded!(app, req);
    match app.db.performance_catalogue() {
        Ok(rows)=>axum::Json(rows.into_iter().map(|r|serde_json::json!({"means":r.means(),"history":r})).collect::<Vec<_>>()).into_response(),
        Err(e)=>(StatusCode::INTERNAL_SERVER_ERROR,e.to_string()).into_response(),
    }
}

// ------------------------------------------------------------- Slot actions

pub async fn slot_action(app: AppCtx, Path((slot_id, action)): Path<(i64, String)>, req: Request) -> Response {
    guarded!(app, req);
    let body = axum::body::to_bytes(req.into_body(), 64 * 1024).await.unwrap_or_default();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::json!({}));
    let reason = format!("api: slot {slot_id} {action}");
    if app.cfg().slot(slot_id).is_none() {
        return (StatusCode::NOT_FOUND, "unknown slot").into_response();
    }

    match action.as_str() {
        "wake" => {
            // Force-Wake (nur via Bearer-API — z. B. Praxis mit ROUTER_TOKEN):
            // schaltet die Auto-Miete ZURÜCK an und weckt den Slot. Das ist
            // der Nachtmodus-Ausstieg: 20:00-Sleep fährt alles runter, die
            // erste gepairte Discord-Nachricht morgens/nachts weckt alles
            // wieder hoch (Budget-Caps greifen weiter).
            let force = payload.get("force").and_then(|f| f.as_bool()).unwrap_or(false);
            if !app.db.auto_rent_enabled() {
                if !force {
                    app.events.emit(
                        &app.db,
                        "wake_blocked",
                        Some(slot_id),
                        None,
                        "Auto-Miete ist ausgeschaltet — Wake ignoriert (Dashboard-Schalter)",
                        &serde_json::json!({}),
                    );
                    return (
                        StatusCode::CONFLICT,
                        "auto_rent disabled — wake ignored",
                    )
                        .into_response();
                }
                if let Err(e) = crate::reconciler::set_auto_rent(&app.0, true, "api: force wake").await {
                    return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
                }
            }
            let _ = app.db.set_slot_desired_audited(slot_id, true, "api: wake");
            app.events.emit(&app.db, "wake", Some(slot_id), None, &reason, &payload);
            app.reconcile_now.notify_one();
            (StatusCode::OK, "waking").into_response()
        }
        "sleep" | "stop" => {
            let _ = app.db.set_slot_desired_audited(slot_id, false, "api: sleep")
                .map_err(|e| tracing::warn!(%e, "audit write"));
            if let Some(active) = app.db.active_instance(slot_id) {
                if let Err(e) = crate::reconciler::stop_instance(&app.0, active, &reason).await {
                    return (StatusCode::BAD_GATEWAY, format!("stop pending: {e}")).into_response();
                }
            }
            (StatusCode::OK, "stopping").into_response()
        }
        "destroy" => {
            match crate::reconciler::destroy_all_instances(&app.0, Some(&[slot_id]), &reason).await {
                Ok(n) => (StatusCode::OK, format!("destroyed {n}")).into_response(),
                Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
            }
        }
        "start" => {
            if let Some(active) = app.db.active_instance(slot_id) {
                let _ = app.db.set_slot_desired_audited(slot_id, true, "api: slot start")
                    .map_err(|e| tracing::warn!(%e, "audit write"));
                if let Err(e) = crate::reconciler::start_instance(&app.0, active, &reason).await {
                    return (StatusCode::BAD_GATEWAY, format!("start failed: {e}")).into_response();
                }
            } else {
                return (StatusCode::NOT_FOUND, "no active instance").into_response();
            }
            (StatusCode::OK, "starting").into_response()
        }
        "swap" => {
            let _ = app.db.set_slot_desired_audited(slot_id, true, "api: swap")
                .map_err(|e| tracing::warn!(%e, "audit write"));
            app.reconcile_now.notify_one();
            (StatusCode::OK, "swap requested").into_response()
        }
        "pin" => {
            let _lock = app.management.lock().await;
            if let Some(active) = app.db.active_instance(slot_id) {
                let _ = app.db.set_slot_pin(slot_id, Some(active));
                let _ = app.db.update_instance_pinned(active, true);
                app.events.emit(&app.db, "pinned", Some(slot_id), Some(active), &reason, &payload);
            }
            (StatusCode::OK, "pinned").into_response()
        }
        "unpin" => {
            let _lock = app.management.lock().await;
            let _ = app.db.set_slot_pin(slot_id, None);
            for inst in app.db.instances(false) {
                if inst.slot_id == slot_id && inst.pinned {
                    let _ = app.db.update_instance_pinned(inst.vast_id, false);
                }
            }
            app.events.emit(&app.db, "unpinned", Some(slot_id), None, &reason, &payload);
            (StatusCode::OK, "unpinned").into_response()
        }
        "bid" => {
            let Some(price) = payload.get("price").and_then(|p| p.as_f64()) else {
                return (StatusCode::BAD_REQUEST, "missing price").into_response();
            };
            if let Some(active) = app.db.active_instance(slot_id) {
                match crate::operations::change_bid(&app.0, active, price, &reason).await {
                    Ok(()) => (StatusCode::OK, "bid updated").into_response(),
                    Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
                }
            } else {
                (StatusCode::NOT_FOUND, "no active instance").into_response()
            }
        }
        _ => (StatusCode::NOT_FOUND, "unknown action").into_response(),
    }
}

// ------------------------------------------------------------- Instances

#[derive(Deserialize)]
pub struct CreateInstanceBody {
    pub offer_id: i64,
    pub slot_id: i64,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub price_usd_h: Option<f64>,
    #[serde(default)]
    pub disk_gb: Option<i64>,
    #[serde(default)]
    pub lifecycle: Option<praxis_common::Lifecycle>,
    #[serde(default)]
    #[allow(dead_code)]
    pub search: Option<String>, // live-Suche statt offer_id
    /// Legacy field: force overrides are rejected, never bypass admission.
    #[serde(default)]
    pub force: bool,
}

pub async fn instance_create(app: AppCtx, req: Request) -> Response {
    guarded!(app, req);
    let body = axum::body::to_bytes(req.into_body(), 256 * 1024).await.unwrap_or_default();
    let Ok(payload) = serde_json::from_slice::<CreateInstanceBody>(&body) else {
        return (StatusCode::BAD_REQUEST, "bad json").into_response();
    };
    let Some(slot) = app.cfg().slot(payload.slot_id).cloned() else {
        return (StatusCode::NOT_FOUND, "unknown slot").into_response();
    };

    // Angebot live nachschlagen (Dashboard übergibt offer_id aus Suche).
    let offers = crate::reconciler::search_slot_offers(&app.0, payload.slot_id, true)
        .await
        .unwrap_or_default();
    let offer = offers.iter().find(|o| o.id == payload.offer_id);
    let Some(offer) = offer else {
        return (StatusCode::NOT_FOUND, "offer not found in current search").into_response();
    };

    let price = payload.price_usd_h.or_else(|| match payload.mode {
        Mode::Interruptible => Some((offer.min_bid * (1.0 + slot.bid.margin)).min(slot.bid.ceiling_usd_h)),
        _ => Some(offer.dph_total),
    });
    let Some(price) = price else {
        return (StatusCode::BAD_REQUEST, "no price").into_response();
    };
    if payload.force { return (StatusCode::BAD_REQUEST,"force overrides disabled; configure limits explicitly").into_response(); }
    let disk = payload.disk_gb.unwrap_or(slot.disk_gb);
    let check = crate::catalog::assess(&app.0,&slot,offer,payload.mode,price,disk);
    if !check.eligible {return (StatusCode::BAD_REQUEST,check.reasons.join("; ")).into_response();}
    if !price.is_finite() || price <= 0.0 || disk <= 0 {
        return (StatusCode::BAD_REQUEST, "price and disk must be positive").into_response();
    }
    match crate::reconciler::create_instance(
        &app.0,
        payload.slot_id,
        offer,
        payload.mode,
        price,
        disk,
        payload.lifecycle.clone().unwrap_or_default(),
        false,
        "api: manual rent",
    )
    .await
    {
        Ok(vast_id) => (StatusCode::OK, format!("{{\"vast_id\":{vast_id}}}")).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("{e}")).into_response(),
    }
}

pub async fn instance_action(app: AppCtx, Path((vast_id, action)): Path<(i64, String)>, req: Request) -> Response {
    guarded!(app, req);
    let body = axum::body::to_bytes(req.into_body(), 256 * 1024).await.unwrap_or_default();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::json!({}));
    let reason = format!("api: instance {vast_id} {action}");
    if app.db.instance(vast_id).is_none() {
        return (StatusCode::NOT_FOUND, "unknown instance").into_response();
    }

    match action.as_str() {
        "benchmark" => match crate::performance::start_benchmark(&app.0, vast_id).await {
            Ok(()) => (StatusCode::ACCEPTED, "benchmark scheduled; results: /api/v1/performance").into_response(),
            Err(e) => (StatusCode::CONFLICT, e.to_string()).into_response(),
        },
        "stop" => match crate::reconciler::stop_instance(&app.0, vast_id, &reason).await {
            Ok(_) => (StatusCode::OK, "stopping").into_response(),
            Err(e) => (StatusCode::BAD_GATEWAY, format!("{e}")).into_response(),
        },
        "start" => {
            let _ = app.db.set_slot_desired_audited(app.db.instance(vast_id).map(|i| i.slot_id).unwrap_or(1), true, "api: instance start")
                .map_err(|e| tracing::warn!(%e, "audit write"));
            match crate::reconciler::start_instance(&app.0, vast_id, &reason).await {
                Ok(_) => (StatusCode::OK, "starting").into_response(),
                Err(e) => (StatusCode::BAD_GATEWAY, format!("{e}")).into_response(),
            }
        }
        "destroy" => match crate::reconciler::destroy_instance(&app.0, vast_id, &reason).await {
            Ok(_) => (StatusCode::OK, "destroying").into_response(),
            Err(e) => (StatusCode::BAD_GATEWAY, format!("{e}")).into_response(),
        },
        "pin" => {
            let _lock = app.management.lock().await;
            let _ = app.db.update_instance_pinned(vast_id, true);
            app.events.emit(&app.db, "pinned", None, Some(vast_id), &reason, &payload);
            (StatusCode::OK, "pinned").into_response()
        }
        // Lock: Instanz-Pin + Slot-Pin — die Box wird von NOTHING angefasst
        // (kein Idle-Destroy, kein Preempt-/Kosten-Swap, keine Lifecycle-
        // Aktionen) UND der Slot mietet keinen Ersatz (kein Wake-Replace,
        // kein Pool-Refill, kein Auto-Rent). Manuelles Stop/Destroy bleibt
        // möglich (bewusste Aktion schlägt immer den Lock).
        "lock" => {
            let _lock = app.management.lock().await;
            let slot_id = app.db.instance(vast_id).map(|i| i.slot_id);
            let _ = app.db.update_instance_pinned(vast_id, true);
            if let Some(sid) = slot_id {
                let _ = app.db.set_slot_pin(sid, Some(vast_id));
            }
            app.events.emit(&app.db, "locked", slot_id, Some(vast_id), &reason, &payload);
            (StatusCode::OK, "locked").into_response()
        }
        "unlock" => {
            let _lock = app.management.lock().await;
            let slot_id = app.db.instance(vast_id).map(|i| i.slot_id);
            let _ = app.db.update_instance_pinned(vast_id, false);
            if let Some(sid) = slot_id {
                // Slot-Pin nur lösen, wenn er auf DIESE Instanz zeigt.
                let pins: std::collections::HashMap<i64, Option<i64>> = app.db.slot_pins().into_iter().collect();
                if pins.get(&sid).copied().flatten() == Some(vast_id) {
                    let _ = app.db.set_slot_pin(sid, None);
                }
                for inst in app.db.instances(false) {
                    if inst.slot_id == sid && inst.pinned && inst.vast_id != vast_id {
                        let _ = app.db.update_instance_pinned(inst.vast_id, false);
                    }
                }
            }
            app.events.emit(&app.db, "unlocked", slot_id, Some(vast_id), &reason, &payload);
            (StatusCode::OK, "unlocked").into_response()
        }
        "bid" => {
            let Some(price) = payload.get("price").and_then(|p| p.as_f64()) else {
                return (StatusCode::BAD_REQUEST, "missing price").into_response();
            };
            match crate::operations::change_bid(&app.0, vast_id, price, &reason).await {
                Ok(()) => (StatusCode::OK, "bid updated").into_response(),
                Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
            }
        }
        "mode" => {
            let Some(mode) = payload.get("mode").and_then(|m| serde_json::from_value::<Mode>(m.clone()).ok()) else {
                return (StatusCode::BAD_REQUEST, "bad mode").into_response();
            };
            if app.db.instance(vast_id).map(|i| i.mode) != Some(mode) {
                // A DB edit cannot convert the actual Vast contract and would
                // corrupt the meter. Require an explicit replacement rental.
                return (StatusCode::CONFLICT, "contract mode is immutable; rent a replacement in the desired mode").into_response();
            }
            (StatusCode::OK, "mode unchanged").into_response()
        }
        "lifecycle" => {
            let Some(lc) = payload.get("lifecycle").and_then(|l| serde_json::from_value::<praxis_common::Lifecycle>(l.clone()).ok()) else {
                return (StatusCode::BAD_REQUEST, "bad lifecycle").into_response();
            };
            let json = serde_json::to_string(&lc).unwrap_or_default();
            let _ = app.db.update_instance_lifecycle(vast_id, &json);
            app.events.emit(&app.db, "lifecycle_changed", None, Some(vast_id), &json, &payload);
            (StatusCode::OK, "lifecycle updated").into_response()
        }
        _ => (StatusCode::NOT_FOUND, "unknown action").into_response(),
    }
}

/// Logs: Vast request_logs + Agent-tail kombiniert.
pub async fn instance_logs(app: AppCtx, Path(vast_id): Path<i64>, req: Request) -> Response {
    guarded!(app, req);
    let mut lines: Vec<String> = Vec::new();
    let vast_client = app.vast.lock().unwrap().clone();
    if let Some(vast) = vast_client {
        if let Ok(logs) = vast.request_logs(vast_id).await {
            for l in logs.iter().take(200) {
                lines.push(l.log.trim_end().to_string());
            }
        }
    }
    // Agent-tail der wichtigsten Services.
    for svc in ["llama-chat", "comfyui", "netbird"] {
        let res = app
            .hub
            .command(vast_id, |id| praxis_common::node::RouterCommand::Cmd {
                id,
                command: praxis_common::node::Command::Tail {
                    file: format!("/workspace/logs/{svc}.log"),
                    lines: 50,
                },
            })
            .await;
        if let Ok(data) = res {
            if let Some(text) = data.get("output").and_then(|o| o.as_str()) {
                lines.push(format!("--- agent tail {svc}.log ---"));
                lines.push(text.to_string());
            }
        }
    }
    Json(serde_json::json!({ "vast_id": vast_id, "logs": lines })).into_response()
}

/// Dashboard-Terminal: WS ↔ Agent-TermOpen (Pipes-bash).
pub async fn term_ws(app: AppCtx, Path(vast_id): Path<i64>, ws: WebSocketUpgrade, req: Request) -> Response {
    // Browser-WebSocket (xterm.js) kann keine Authorization-Header setzen
    // — Session-Cookie zusaetzlich akzeptieren (wie events_sse).
    if !crate::dashboard::session_ok(&app.0, req.headers()) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    ws.on_upgrade(move |socket| async move {
        let (mut sink, mut stream) = socket.split();

        // TermOpen über den Command-Weg (hub.command): Der Agent vergibt die
        // Terminal-Session-ID SELBST (terms.open() → eigener Zähler) und
        // antwortet mit {"term_id": N}. Vorher wurde diese Antwort verworfen
        // und das Relay unter der ROUTER-eigenen ID (new_term_id) registriert
        // → die Agent-Frames (Agent-ID!) fielen in relay_term still durch,
        // das Dashboard-Terminal blieb leer (21.09. live reproduziert).
        let open = app
            .hub
            .command(vast_id, |id| praxis_common::node::RouterCommand::Cmd {
                id,
                command: praxis_common::node::Command::TermOpen { cols: 80, rows: 24 },
            })
            .await;
        let term_id = match open {
            Ok(data) => data.get("term_id").and_then(|v| v.as_u64()).unwrap_or(0),
            Err(_) => 0,
        };
        if term_id == 0 {
            // Agent nicht verbunden/falsche Antwort → sauber beenden statt
            // still leer zu bleiben (Frontend zeigt „[Session beendet]“).
            let _ = sink.send(Message::Text(r#"{"type":"exit"}"#.into())).await;
            return;
        }

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<serde_json::Value>(64);
        app.hub.register_term(vast_id, term_id, out_tx);

        let hub = app.hub.clone();
        loop {
            tokio::select! {
                from_client = stream.next() => {
                    match from_client {
                        Some(Ok(Message::Text(t))) => {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                                match v.get("type").and_then(|t| t.as_str()) {
                                    Some("input") => {
                                        if let Some(data) = v.get("data").and_then(|d| d.as_str()) {
                                            let _ = hub.term_send(vast_id, praxis_common::node::RouterCommand::TermIn { id: term_id, data: data.to_string() });
                                        }
                                    }
                                    Some("resize") => {
                                        let cols = v.get("cols").and_then(|c| c.as_u64()).unwrap_or(80).min(500) as u16;
                                        let rows = v.get("rows").and_then(|r| r.as_u64()).unwrap_or(24).min(200) as u16;
                                        let _ = hub.term_send(vast_id, praxis_common::node::RouterCommand::TermResize { id: term_id, cols, rows });
                                    }
                                    Some("close") => {
                                        let _ = hub.term_send(vast_id, praxis_common::node::RouterCommand::TermClose { id: term_id });
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Some(Ok(Message::Binary(_))) => {}
                        Some(Ok(Message::Close(_))) | None => {
                            let _ = hub.term_send(vast_id, praxis_common::node::RouterCommand::TermClose { id: term_id });
                            break;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(_)) => break,
                    }
                }
                from_agent = out_rx.recv() => {
                    if let Some(frame) = from_agent {
                        if frame.get("type").and_then(|t| t.as_str()) == Some("term") {
                            // CRLF-Normalisierung: Der Agent pumpt Zeilen als
                            // "...\n" (pipes-bash) — xterm.js interpretiert \n
                            // aber als LF OHNE Wagenrücklauf → Treppeneffekt
                            // (jede Ausgabezeile startet in der Spalte der
                            // vorigen, 21.09. live gesehen). \r\n erzwingt
                            // sauberen Zeilenanfang (idempotent: CR-LF → LF → CRLF).
                            let data = frame
                                .get("data")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .replace("\r\n", "\n")
                                .replace('\n', "\r\n");
                            if sink.send(Message::Text(format!("{{\"type\":\"output\",\"data\":{}}}", serde_json::to_string(&data).unwrap_or_default()).into())).await.is_err() {
                                break;
                            }
                        } else if frame.get("type").and_then(|t| t.as_str()) == Some("term_end") {
                            let _ = sink.send(Message::Text(r#"{"type":"exit"}"#.into())).await;
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
        }
        let _ = hub.term_send(vast_id, praxis_common::node::RouterCommand::TermClose { id: term_id });
        app.hub.unregister_term(vast_id, term_id);
    })
}

/// Kommando an den Agent (supervisorctl-restart etc.).
pub async fn instance_cmd(app: AppCtx, Path((vast_id, cmd)): Path<(i64, String)>, req: Request) -> Response {
    guarded!(app, req);
    let body = axum::body::to_bytes(req.into_body(), 64 * 1024).await.unwrap_or_default();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let service = payload.get("service").and_then(|s| s.as_str()).unwrap_or("llama-chat").to_string();
    // sync_assets: Router PUSHt (Outbound-Pull der Boxen ist auf Vast
    // unzuverlässig) — gleichet Primitiv wie nach Session-Aufbau.
    if cmd == "sync_assets" {
        return match crate::assets::push_assets(&app.0, vast_id).await {
            Ok(v) => Json(v).into_response(),
            Err(e) => (StatusCode::BAD_GATEWAY, format!("{e}")).into_response(),
        };
    }
    let command = match cmd.as_str() {
        "restart" => praxis_common::node::Command::RestartService { service },
        "stop" => praxis_common::node::Command::StopService { service },
        "start" => praxis_common::node::Command::StartService { service },
        "run_acceptance" => praxis_common::node::Command::RunAcceptance,
        "nvidia-smi" => praxis_common::node::Command::Exec {
            argv: vec!["nvidia-smi".into()],
        },
        "exec" => {
            let Some(argv) = payload.get("argv").and_then(|a| serde_json::from_value::<Vec<String>>(a.clone()).ok()) else {
                return (StatusCode::BAD_REQUEST, "missing argv").into_response();
            };
            praxis_common::node::Command::Exec { argv }
        }
        _ => return (StatusCode::NOT_FOUND, "unknown cmd").into_response(),
    };
    match app
        .hub
        .command(vast_id, |id| praxis_common::node::RouterCommand::Cmd { id, command: command.clone() })
        .await
    {
        Ok(data) => Json(data).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

// ------------------------------------------------------------- kleine Helfer fürs Dashboard

pub fn role_str(r: Role) -> &'static str {
    match r {
        Role::Llm => "llm",
        Role::Media => "media",
    }
}
/// `POST /api/v1/settings/auto_rent` — Auto-Miete-Schalter (Dashboard-Toggle).
/// Body: `{"enabled": true|false}`. OFF = keine automatischen Mieten/Starts,
/// laufende Boxen werden gestoppt („aus ist aus“), Proxy antwortet kalt mit
/// `auto_rent_off`. ON = Schalter zurück (weckt nichts von selbst).
pub async fn auto_rent_set(app: AppCtx, req: Request) -> Response {
    guarded!(app, req);
    let body = axum::body::to_bytes(req.into_body(), 64 * 1024).await.unwrap_or_default();
    let payload: serde_json::Value =
        serde_json::from_slice(&body).unwrap_or(serde_json::json!({}));
    let Some(enabled) = payload.get("enabled").and_then(|v| v.as_bool()) else {
        return (
            StatusCode::BAD_REQUEST,
            "body: {\"enabled\": true|false}",
        )
            .into_response();
    };
    if let Err(e) = crate::reconciler::set_auto_rent(&app.0, enabled, "api").await {
        return (StatusCode::BAD_GATEWAY, e.to_string()).into_response();
    }
    (
        StatusCode::OK,
        format!(
            "auto_rent {}",
            if enabled { "enabled" } else { "disabled" }
        ),
    )
        .into_response()
}
