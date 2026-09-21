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
        return true; // offener Modus (nur lokal testen!)
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
    crate::reconciler::set_auto_rent(&app.0, false, "api: sleep_all").await;
    (StatusCode::OK, "sleeping: auto_rent off, instances stopped").into_response()
}

/// `POST /api/v1/destroy_all` — ALLE nicht gepinnten Instanzen zerstören
/// (optional body {"slots":[1,2]} oder {"auto_rent":false}). Fürs manuelle
/// Aufräumen + als Ziel für externe Cronjobs.
pub async fn destroy_all(app: AppCtx, req: Request) -> Response {
    guarded!(app, req);
    let body = axum::body::to_bytes(req.into_body(), 64 * 1024).await.unwrap_or_default();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let slots: Option<Vec<i64>> = payload
        .get("slots")
        .and_then(|s| serde_json::from_value(s.clone()).ok());
    let n = crate::reconciler::destroy_all_instances(&app.0, slots.as_deref(), "api: destroy_all").await;
    if payload.get("auto_rent").and_then(|a| a.as_bool()).unwrap_or(false) {
        crate::reconciler::set_auto_rent(&app.0, false, "api: destroy_all").await;
    }
    (StatusCode::OK, format!("destroyed {n} instances")).into_response()
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

    // Slot-DB synchronisieren (neue Slots, geänderte Rollen/Namen).
    app.db.init_slots(&new_cfg)?;
    // Atomar schreiben: tmp + rename — der Router startet nach einem Crash
    // nie mit halber TOML.
    let tmp = format!("{}.tmp", app.config_path);
    std::fs::write(&tmp, raw)?;
    std::fs::rename(&tmp, &app.config_path)?;

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

// ------------------------------------------------------------- State

pub async fn state(app: AppCtx, req: Request) -> Response {
    guarded!(app, req);
    axum::Json(crate::dashboard::state_json(&app.0)).into_response()
}

pub async fn budget(app: AppCtx, req: Request) -> Response {
    guarded!(app, req);
    axum::Json(budget_json(&app.0)).into_response()
}

pub fn budget_json(app: &SharedApp) -> serde_json::Value {
    let date = crate::node::local_date(app);
    let spent = app.db.spent_today(&date);
    let month = app.db.spent_month(&date[..7]);
    serde_json::json!({
        "date": date,
        "spent_today_usd": spent,
        "spent_month_usd": month,
        "soft_eur": app.cfg().budget.daily_soft_eur,
        "hard_eur": app.cfg().budget.daily_hard_eur,
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
        Ok(offers) => axum::Json(offers).into_response(),
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
    let (set, note) = match action.as_str() {
        "blacklist" => (true, "manuell blacklisted (API)"),
        "unblacklist" => (false, ""),
        _ => return (StatusCode::BAD_REQUEST, "unknown action; use blacklist|unblacklist").into_response(),
    };
    if app.db.machine_stat(machine_id).is_none() && !set {
        return (StatusCode::NOT_FOUND, "machine unknown").into_response();
    }
    let _ = app.db.set_machine_blacklist(machine_id, set, note);
    app.events.emit(
        &app.0.db,
        if set { "machine_blacklisted" } else { "machine_unblacklisted" },
        None,
        None,
        &format!("Host {machine_id} {} (API)", if set { "blacklisted" } else { "von Blacklist entfernt" }),
        &serde_json::json!({"machine_id": machine_id}),
    );
    (StatusCode::OK, if set { "blacklisted" } else { "unblacklisted" }).into_response()
}

// ------------------------------------------------------------- Slot actions

pub async fn slot_action(app: AppCtx, Path((slot_id, action)): Path<(i64, String)>, req: Request) -> Response {
    guarded!(app, req);
    let body = axum::body::to_bytes(req.into_body(), 64 * 1024).await.unwrap_or_default();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::json!({}));
    let reason = format!("api: slot {slot_id} {action}");

    match action.as_str() {
        "wake" => {
            if !app.db.auto_rent_enabled() {
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
            let _ = app.db.set_slot_desired_audited(slot_id, true, "api: wake");
            app.events.emit(&app.db, "wake", Some(slot_id), None, &reason, &payload);
            app.reconcile_now.notify_one();
            (StatusCode::OK, "waking").into_response()
        }
        "sleep" | "stop" => {
            let _ = app.db.set_slot_desired_audited(slot_id, false, "api: sleep")
                .map_err(|e| tracing::warn!(%e, "audit write"));
            if let Some(active) = app.db.active_instance(slot_id) {
                let _ = crate::reconciler::stop_instance(&app.0, active, &reason);
            }
            (StatusCode::OK, "stopping").into_response()
        }
        "destroy" => {
            // Bugfix 21.09.: bisher nur die ACTIVE Instanz — während
            // Warmup/Boot ist active None → Button war ein stilles No-Op
            // ("der Destroy-Button zerstört die Instanz nicht"). Jetzt:
            // ALLE Instanzen des Slots.
            let mut n = 0;
            for inst in app.db.instances(false) {
                if inst.slot_id == slot_id && inst.destroyed_at.is_none() && !inst.pinned {
                    if crate::reconciler::destroy_instance(&app.0, inst.vast_id, &reason).await.is_ok() {
                        n += 1;
                    }
                }
            }
            (StatusCode::OK, format!("destroying {n}")).into_response()
        }
        "start" => {
            if let Some(active) = app.db.active_instance(slot_id) {
                let _ = app.db.set_slot_desired_audited(slot_id, true, "api: slot start")
                    .map_err(|e| tracing::warn!(%e, "audit write"));
                let _ = crate::reconciler::start_instance(&app.0, active, &reason);
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
            if let Some(active) = app.db.active_instance(slot_id) {
                let _ = app.db.set_slot_pin(slot_id, Some(active));
                let _ = app.db.update_instance_pinned(active, true);
                app.events.emit(&app.db, "pinned", Some(slot_id), Some(active), &reason, &payload);
            }
            (StatusCode::OK, "pinned").into_response()
        }
        "unpin" => {
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
                let vast_client = app.vast.lock().unwrap().clone();
        if let Some(vast) = vast_client {
                    match vast.set_bid(active, price).await {
                        Ok(_) => {
                            let _ = app.db.set_instance_bid(active, price);
                            app.events.emit(&app.db, "bid_changed", Some(slot_id), Some(active), &format!("Gebot → {price:.4} $/h ({reason})"), &payload);
                            (StatusCode::OK, "bid updated").into_response()
                        }
                        Err(e) => (StatusCode::BAD_GATEWAY, format!("{e}")).into_response(),
                    }
                } else {
                    (StatusCode::SERVICE_UNAVAILABLE, "vast api not configured").into_response()
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
    /// true = bewusst über Ceiling mieten (Notfall). Default: nein — die
    /// Ceiling ist ein hartes Budget, manuelles Mieten umgeht sie NICHT mehr
    /// (21.09.: „20 €/h-Instanz"-Überraschungen über do_rent).
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
    // Hartes Preisfenster durchsetzen (search filtert bereits — das hier
    // fängt explizite price_usd_h-Übergaben und UI-Edge-Cases ab).
    if !payload.force {
        let rate = if payload.mode == Mode::Interruptible { offer.min_bid } else { offer.dph_total };
        if rate < slot.bid.rent_min_usd_h || rate > slot.bid.ceiling_usd_h {
            return (
                StatusCode::BAD_REQUEST,
                format!(
                    "price {:.4} $/h außerhalb Fenster [{:.4}, {:.4}] $/h (slot {}) — force=true zum bewussten Überschreiten",
                    rate, slot.bid.rent_min_usd_h, slot.bid.ceiling_usd_h, slot.id
                ),
            )
                .into_response();
        }
    }
    let disk = payload.disk_gb.unwrap_or(slot.disk_gb);
    match crate::reconciler::create_instance(
        &app.0,
        payload.slot_id,
        offer,
        payload.mode,
        price,
        disk,
        payload.lifecycle.clone().unwrap_or_default(),
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
        "stop" => match crate::reconciler::stop_instance(&app.0, vast_id, &reason) {
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
            let slot_id = app.db.instance(vast_id).map(|i| i.slot_id);
            let _ = app.db.update_instance_pinned(vast_id, true);
            if let Some(sid) = slot_id {
                let _ = app.db.set_slot_pin(sid, Some(vast_id));
            }
            app.events.emit(&app.db, "locked", slot_id, Some(vast_id), &reason, &payload);
            (StatusCode::OK, "locked").into_response()
        }
        "unlock" => {
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
            let vast_client = app.vast.lock().unwrap().clone();
            let Some(vast) = vast_client else {
                return (StatusCode::SERVICE_UNAVAILABLE, "vast api not configured").into_response();
            };
            match vast.set_bid(vast_id, price).await {
                Ok(_) => {
                    let _ = app.db.set_instance_bid(vast_id, price);
                    app.events.emit(&app.db, "bid_changed", None, Some(vast_id), &format!("Gebot → {price:.4} $/h"), &payload);
                    (StatusCode::OK, "bid updated").into_response()
                }
                Err(e) => (StatusCode::BAD_GATEWAY, format!("{e}")).into_response(),
            }
        }
        "mode" => {
            let Some(mode) = payload.get("mode").and_then(|m| serde_json::from_value::<Mode>(m.clone()).ok()) else {
                return (StatusCode::BAD_REQUEST, "bad mode").into_response();
            };
            let _ = app.db.update_instance_mode(vast_id, mode);
            app.events.emit(
                &app.db,
                "mode_changed",
                None,
                Some(vast_id),
                &format!("Modus → {mode} (Hot-Swap bei nächster Gelegenheit)"),
                &payload,
            );
            (StatusCode::OK, "mode updated").into_response()
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

        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
        app.hub.register_term(term_id, out_tx);

        let hub = app.hub.clone();
        loop {
            tokio::select! {
                from_client = stream.next() => {
                    match from_client {
                        Some(Ok(Message::Text(t))) => {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                                if v.get("type").and_then(|t| t.as_str()) == Some("input") {
                                    if let Some(data) = v.get("data").and_then(|d| d.as_str()) {
                                        let _ = hub.term_send(vast_id, praxis_common::node::RouterCommand::TermIn { id: term_id, data: data.to_string() });
                                    }
                                } else if v.get("type").and_then(|t| t.as_str()) == Some("close") {
                                    let _ = hub.term_send(vast_id, praxis_common::node::RouterCommand::TermClose { id: term_id });
                                    break;
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
        app.hub.unregister_term(term_id);
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
    crate::reconciler::set_auto_rent(&app.0, enabled, "api").await;
    (
        StatusCode::OK,
        format!(
            "auto_rent {}",
            if enabled { "enabled" } else { "disabled" }
        ),
    )
        .into_response()
}
