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
    let token = app.cfg.router_token();
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
        "soft_eur": app.cfg.budget.daily_soft_eur,
        "hard_eur": app.cfg.budget.daily_hard_eur,
        "monthly_eur": app.cfg.budget.monthly_eur,
        "usd_per_eur": app.cfg.budget.usd_per_eur,
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
    guarded!(app, req);
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
            let _ = app.db.set_slot_desired(slot_id, true);
            app.events.emit(&app.db, "wake", Some(slot_id), None, &reason, &payload);
            app.reconcile_now.notify_one();
            (StatusCode::OK, "waking").into_response()
        }
        "sleep" | "stop" => {
            let _ = app.db.set_slot_desired(slot_id, false);
            if let Some(active) = app.db.active_instance(slot_id) {
                let _ = crate::reconciler::stop_instance(&app.0, active, &reason);
            }
            (StatusCode::OK, "stopping").into_response()
        }
        "destroy" => {
            if let Some(active) = app.db.active_instance(slot_id) {
                let _ = crate::reconciler::destroy_instance(&app.0, active, &reason);
            }
            (StatusCode::OK, "destroying").into_response()
        }
        "start" => {
            if let Some(active) = app.db.active_instance(slot_id) {
                let _ = app.db.set_slot_desired(slot_id, true);
                let _ = crate::reconciler::start_instance(&app.0, active, &reason);
            }
            (StatusCode::OK, "starting").into_response()
        }
        "swap" => {
            let _ = app.db.set_slot_desired(slot_id, true);
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
}

pub async fn instance_create(app: AppCtx, req: Request) -> Response {
    guarded!(app, req);
    let body = axum::body::to_bytes(req.into_body(), 256 * 1024).await.unwrap_or_default();
    let Ok(payload) = serde_json::from_slice::<CreateInstanceBody>(&body) else {
        return (StatusCode::BAD_REQUEST, "bad json").into_response();
    };
    let Some(slot) = app.cfg.slot(payload.slot_id) else {
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
            let _ = app.db.set_slot_desired(app.db.instance(vast_id).map(|i| i.slot_id).unwrap_or(1), true);
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
        "unpin" => {
            let _ = app.db.update_instance_pinned(vast_id, false);
            app.events.emit(&app.db, "unpinned", None, Some(vast_id), &reason, &payload);
            (StatusCode::OK, "unpinned").into_response()
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
    guarded!(app, req);
    ws.on_upgrade(move |socket| async move {
        let (mut sink, mut stream) = socket.split();
        let term_id = app.hub.new_term_id();

        // Open an die Instanz.
        let open = praxis_common::node::RouterCommand::Cmd {
            id: term_id,
            command: praxis_common::node::Command::TermOpen { cols: 80, rows: 24 },
        };
        if app.hub.term_send(vast_id, open).is_err() {
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
                            let data = frame.get("data").and_then(|d| d.as_str()).unwrap_or("");
                            if sink.send(Message::Text(format!("{{\"type\":\"output\",\"data\":{}}}", serde_json::to_string(data).unwrap_or_default()).into())).await.is_err() {
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