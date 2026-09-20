//! Node-Side: WS-Call-home `/api/v1/node` + REST für Agents
//! (Asset-Pull, Reports, Events). Auth: per-Instanz `PRAXIS_NODE_TOKEN`.

use crate::state::AppCtx;
use crate::state::SharedApp;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use praxis_common::node::NodeMessage;
use praxis_common::node::RouterCommand;
use futures::{SinkExt, StreamExt};

/// Bearer-Token aus Header ziehen.
pub fn bearer(req_headers: &axum::http::HeaderMap) -> Option<String> {
    req_headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.to_string())
}

/// Agent-Auth: Instanz über Node-Token.
pub fn instance_by_token(app: &SharedApp, token: &str) -> Option<crate::db::InstanceRow> {
    app.db.instance_by_token(token)
}

/// `WS /api/v1/node` — Agent-Call-home.
pub async fn node_ws(app: AppCtx, ws: WebSocketUpgrade, req: Request) -> Response {
    let _ = req;
    ws.on_upgrade(move |socket| async move {
        node_session(app.0, socket).await;
    })
}

async fn node_session(app: SharedApp, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    #[allow(unused_assignments)]
    let mut registered_vast_id: Option<i64> = None;
    #[allow(unused_assignments)]
    let mut rx: Option<tokio::sync::mpsc::UnboundedReceiver<RouterCommand>> = None;

    // 1. Hello mit Token abwarten (60 s).
    let hello = tokio::time::timeout(std::time::Duration::from_secs(60), stream.next()).await;
    let raw = match hello {
        Ok(Some(Ok(Message::Text(raw)))) => raw,
        _ => return,
    };
    let msg: NodeMessage = match serde_json::from_str(&raw) {
        Ok(m) => m,
        Err(_) => {
            let _ = sink.send(Message::Text(r#"{"type":"error","reason":"bad hello"}"#.into())).await;
            return;
        }
    };
    let NodeMessage::Hello { token, role, agent_version, nb_ip, hostname, services } = msg else {
        return;
    };
    let Some(inst) = app.db.instance_by_token(&token) else {
        tracing::warn!(?hostname, ?nb_ip, "node hello: unknown token");
        let _ = sink.send(Message::Text(r#"{"type":"error","reason":"unknown token"}"#.into())).await;
        return;
    };
    let vast_id = inst.vast_id;
    registered_vast_id = Some(vast_id);
    tracing::info!(vast_id, role, %agent_version, ?nb_ip, "agent connected");
    let _ = app
        .db
        .update_instance_agent(vast_id, nb_ip.as_deref(), false, "agent_connected");
    app.hub.register(vast_id, inst.slot_id, nb_ip.clone(), services.clone());
    rx = Some(app.hub.register(vast_id, inst.slot_id, nb_ip.clone(), services));
    app.reconcile_now.notify_one();

    // Asset-Push (Call-home-Pfad: gleiche Session, Router schiebt).
    {
        let app2 = app.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::assets::push_assets(&app2, vast_id).await {
                tracing::warn!(%e, vast_id, "asset-push fehlgeschlagen");
            }
        });
    }

    let mut last_health: Option<serde_json::Value> = None;
    loop {
        tokio::select! {
            from_agent = stream.next() => {
                match from_agent {
                    Some(Ok(Message::Text(raw))) => {
                        match serde_json::from_str::<NodeMessage>(&raw) {
                            Ok(NodeMessage::Hello { token, .. }) => {
                                // Reconnect: neu registrieren.
                                if let Some(inst2) = app.db.instance_by_token(&token) {
                                    let _ = app.db.update_instance_agent(inst2.vast_id, nb_ip.as_deref(), false, "agent_connected");
                                    app.reconcile_now.notify_one();
                                }
                            }
                            Ok(NodeMessage::Heartbeat { health, busy, busy_reason, gpu, progress, disk_free_gb, .. }) => {
                                let health_json = serde_json::to_value(&health).unwrap_or_default();
                                let hb = crate::hub::HeartbeatData {
                                    health_json: health_json.clone(),
                                    busy,
                                    busy_reason: busy_reason.clone(),
                                    gpu_json: serde_json::to_value(&gpu).unwrap_or_default(),
                                    progress_json: serde_json::to_value(&progress).unwrap_or_default(),
                                    disk_free_gb,
                                };
                                let health_changed = last_health.as_ref() != Some(&health_json);
                                last_health = Some(health_json);
                                app.hub.record_heartbeat(vast_id, hb, None);
                                let healthy = matches!(health, praxis_common::AgentHealth::Healthy);
                                let state = if healthy { "healthy" } else { "booting" };
                                let _ = app.db.update_instance_agent(vast_id, None, healthy, state);
                                if health_changed {
                                    app.reconcile_now.notify_one();
                                }
                            }
                            Ok(NodeMessage::Event { kind, payload }) => {
                                app.events.emit(&app.db, &kind, Some(inst.slot_id), Some(vast_id), &format!("{kind} von Agent {vast_id}"), &payload);
                            }
                            Ok(NodeMessage::CmdResult { id, ok, data }) => {
                                app.hub.resolve(id, if ok { Ok(data) } else { Err(data.get("error").and_then(|e| e.as_str()).unwrap_or("command failed").to_string()) });
                            }
                            Ok(NodeMessage::Term { .. }) | Ok(NodeMessage::TermEnd { .. }) => {
                                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                                    app.hub.relay_term(v);
                                }
                            }
                            Err(e) => {
                                tracing::debug!(%e, "bad agent message");
                            }
                        }
                    }
                    Some(Ok(Message::Binary(b))) => {
                        // Binary = Audio-PCM? Agents nutzen Text; ignorieren.
                        let _ = b;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {} // ping/pong von axum gehandhabt
                    Some(Err(_)) => break,
                }
            }
            from_router = async {
                match rx {
                    Some(ref mut r) => r.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(cmd) = from_router {
                    let Ok(text) = serde_json::to_string(&cmd) else { continue };
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
    if let Some(v) = registered_vast_id {
        tracing::info!(v, "agent disconnected");
        app.hub.unregister(v);
        // NICHT sofort unreachable: Blips (Agent-Restart, NetBird-Zucken)
        // soll der Connector in ~10 s wieder flicken. Der Reconciler
        // setzt unreachable erst nach >3 min Agent-Stille — und genau dort
        // wird der Maschinen-Fail gezählt (Blacklist-Basis).
        app.reconcile_now.notify_one();
    }
}

/// `GET /api/v1/node/assets/manifest`
pub async fn manifest(app: AppCtx, req: Request) -> Response {
    let Some(token) = bearer(req.headers()) else {
        return (axum::http::StatusCode::UNAUTHORIZED, "missing bearer").into_response();
    };
    crate::assets::node_manifest(app.0.clone(), token).await
}

/// `GET /api/v1/node/assets/{id}`
pub async fn asset(app: AppCtx, axum::extract::Path(id): axum::extract::Path<String>, req: Request) -> Response {
    let Some(token) = bearer(req.headers()) else {
        return (axum::http::StatusCode::UNAUTHORIZED, "missing bearer").into_response();
    };
    crate::assets::node_asset(app.0.clone(), token, axum::extract::Path(id), req).await
}

/// `POST /api/v1/node/reports` — Acceptance/Download-Reports.
pub async fn report(app: AppCtx, req: Request) -> Response {
    use axum::http::StatusCode;
    let Some(token) = bearer(req.headers()) else {
        return (StatusCode::UNAUTHORIZED, "missing bearer").into_response();
    };
    let Some(inst) = instance_by_token(&app.0, &token) else {
        return (StatusCode::UNAUTHORIZED, "unknown token").into_response();
    };
    let body = axum::body::to_bytes(req.into_body(), 256 * 1024).await.unwrap_or_default();
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return (StatusCode::BAD_REQUEST, "bad json").into_response();
    };
    let kind = payload.get("kind").and_then(|k| k.as_str()).unwrap_or("report").to_string();
    // Traffic-Report: GB → Budget buchen.
    if let Some(gb) = payload.get("downloaded_gb").and_then(|g| g.as_f64()) {
        let offer_cost = 0.005; // $/GB — Offer-abhängig, konservativ.
        let date = local_date(&app.0);
        let _ = app.db.meter_traffic(&date, gb * offer_cost);
        app.events.emit(
            &app.db,
            "traffic",
            Some(inst.slot_id),
            Some(inst.vast_id),
            &format!("{gb:.1} GB gezogen (≈ {cost:.3} $)", cost = gb * offer_cost),
            &payload,
        );
    }
    app.events.emit(&app.db, &kind, Some(inst.slot_id), Some(inst.vast_id), &kind, &payload);
    (StatusCode::OK, "ok").into_response()
}

/// `POST /api/v1/node/events` — nicht-periodische Events.
pub async fn node_event(app: AppCtx, req: Request) -> Response {
    use axum::http::StatusCode;
    let Some(token) = bearer(req.headers()) else {
        return (StatusCode::UNAUTHORIZED, "missing bearer").into_response();
    };
    let Some(inst) = instance_by_token(&app.0, &token) else {
        return (StatusCode::UNAUTHORIZED, "unknown token").into_response();
    };
    let body = axum::body::to_bytes(req.into_body(), 256 * 1024).await.unwrap_or_default();
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return (StatusCode::BAD_REQUEST, "bad json").into_response();
    };
    let kind = payload.get("kind").and_then(|k| k.as_str()).unwrap_or("agent_event").to_string();
    app.events.emit(&app.db, &kind, Some(inst.slot_id), Some(inst.vast_id), &kind, &payload);
    (StatusCode::OK, "ok").into_response()
}

pub fn local_date(app: &SharedApp) -> String {
    let tz: chrono_tz::Tz = app
        .cfg
        .router
        .tz
        .parse()
        .unwrap_or(chrono_tz::Europe::Berlin);
    chrono::Utc::now().with_timezone(&tz).format("%Y-%m-%d").to_string()
}