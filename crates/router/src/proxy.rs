//! Reverse-Proxy mit WS/SSE-Passthrough, In-flight-Zählung, Origin-Strip,
//! 503/Hold-Semantik und Debug-Direktzugriff (`/inst/<id>/`).
//!
//! Zwei Betriebsarten:
//! - **Passthrough-Listener**: pro Slot-Port ein eigener Socket
//!   (`8188 → media:comfy`, `11434 → llm:api`, ...).
//! - **Pfad-Routing** auf dem Dashboard-Port: `/gpu/<slot>/<svc>/...`,
//!   `/inst/<vast_id>/<svc>/...`.
//! STT (`2700`) zeigt bei `stt.mode = "local"` auf den Router-eigenen
//! Sidecar statt auf den media-Slot.

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod tests;

use crate::state::{AppCtx, SharedApp};
use axum::body::Body;
use axum::extract::ws::Message;
use axum::extract::{Path, Request};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use futures::StreamExt;
use axum::extract::FromRequest;
use hyper::body::Incoming;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use std::time::Duration;

pub type HttpClient = Client<HttpConnector, Body>;

/// Keep the traffic lease for the lifetime of the streamed body, including
/// early client disconnects. Response headers do NOT mean the request is done.
struct TrackedBody {
    body: Body,
    guard: Option<crate::state::InFlightGuard>,
    observer: Option<crate::performance::UsageObserver>,
}

impl Drop for TrackedBody {
    fn drop(&mut self) {
        if let Some(observer) = self.observer.take() {observer.finish(false);}
    }
}

impl hyper::body::Body for TrackedBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>)
        -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>>
    {
        let result = std::pin::Pin::new(&mut self.body).poll_frame(cx);
        if let std::task::Poll::Ready(Some(Ok(ref frame))) = result {
            if let (Some(observer), Some(data)) = (self.observer.as_mut(), frame.data_ref()) {observer.feed(data);}
        }
        let failed = matches!(result, std::task::Poll::Ready(Some(Err(_))));
        if matches!(result, std::task::Poll::Ready(None | Some(Err(_)))) || self.body.is_end_stream() {
            if let Some(observer) = self.observer.take() {observer.finish(!failed);}
            self.guard.take();
        }
        result
    }

    fn is_end_stream(&self) -> bool { self.body.is_end_stream() }
    fn size_hint(&self) -> hyper::body::SizeHint { self.body.size_hint() }
}

#[cfg(test)]
fn tracked_body(body: Body, guard: Option<crate::state::InFlightGuard>) -> Body {
    Body::new(TrackedBody { body, guard, observer: None })
}

pub fn http_client() -> HttpClient {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(Duration::from_secs(10)));
    connector.enforce_http(false);
    Client::builder(TokioExecutor::new()).build::<_, Body>(connector)
}

const HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn strip_hop_by_hop(headers: &mut HeaderMap, connection_tokens: &[String]) {
    for h in HOP_HEADERS {
        headers.remove(HeaderName::from_static(h));
    }
    for tok in connection_tokens {
        if let Ok(name) = HeaderName::from_bytes(tok.as_bytes()) {
            headers.remove(name);
        }
    }
}

fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers.get_all(header::CONNECTION).iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

pub struct UpstreamTarget {
    pub vast_id: i64,
    pub nb_ip: String,
    pub port: u16,
}

/// Response mit Router-Metadaten anreichern.
fn tag_response(mut resp: Response, slot_id: Option<i64>, instance: Option<i64>, state: &str) -> Response {
    let h = resp.headers_mut();
    if let Some(s) = slot_id {
        if let Ok(v) = HeaderValue::from_str(&s.to_string()) {
            h.insert("x-gpu-slot", v);
        }
    }
    if let Some(i) = instance {
        if let Ok(v) = HeaderValue::from_str(&i.to_string()) {
            h.insert("x-gpu-instance", v);
        }
    }
    if let Ok(v) = HeaderValue::from_str(state) {
        h.insert("x-router-state", v);
    }
    resp
}

fn service_unavailable(slot_id: i64, state: &str) -> Response {
    let body = format!(
        "{{\"error\":\"slot {slot_id} has no healthy backend\",\"state\":\"{state}\"}}"
    );
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(header::RETRY_AFTER, "10")
        .header("x-router-state", state)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
}

fn bad_gateway(what: &str) -> Response {
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(format!("{what}\n")))
        .unwrap()
}

/// Kern-Proxy: Request an `http://<nb_ip>:<port><path>` schicken.
/// Streamt Body/SSE durch, tunnelt WS-Upgrades (101).
async fn proxy_to_target(
    app: &SharedApp,
    client: &HttpClient,
    target: &UpstreamTarget,
    slot_id: Option<i64>,
    req: Request,
    path_and_query: String,
    routed: bool,
) -> Response {
    let (mut parts, body) = req.into_parts();
    let is_websocket = wants_upgrade(&parts.headers, &parts.method);
    let tokens = connection_tokens(&parts.headers);
    // Origin strippen: nginx im llama-Fork und Vosk-WS antworten sonst 403.
    parts.headers.remove(header::ORIGIN);
    strip_hop_by_hop(&mut parts.headers, &tokens);

    let authority = format!("{}:{}", target.nb_ip, target.port);
    let uri = format!("http://{authority}{path_and_query}");
    let Ok(upstream_uri) = uri.parse::<Uri>() else {
        return bad_gateway("invalid upstream uri");
    };

    let mut up_req = hyper::Request::builder()
        .method(parts.method.clone())
        .uri(upstream_uri)
        .version(parts.version);
    {
        let h = up_req.headers_mut().unwrap();
        for (k, v) in parts.headers.iter() {
            if k == header::HOST {
                continue;
            }
            h.append(k, v.clone());
        }
        h.insert(header::HOST, HeaderValue::from_str(&authority).unwrap_or(HeaderValue::from_static("upstream")));
    }
    if is_websocket {
        let h = up_req.headers_mut().unwrap();
        h.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    }

    let up_req = up_req.body(body).expect("upstream request");
    let slot_guard = match slot_id {
        Some(id) => match app.traffic.try_begin(id) {
            Some(guard) => Some(guard),
            None => return service_unavailable(id, "benchmarking"),
        },
        None => None,
    };
    // Target selection happens before admission (and can await warming). A
    // flip may have happened meanwhile. With the lease held, reject stale
    // routing before sending ANY bytes upstream; /inst debug remains explicit.
    if routed {
        if let Some(id) = slot_id {
            let current = app.targets.get(id).is_some_and(|(v, _, healthy)| healthy && v == target.vast_id)
                || app.pool_routes.healthy(id).iter().any(|(v, _)| *v == target.vast_id);
            if !current { return service_unavailable(id, "routing_changed"); }
        }
    }
    let mut observer = crate::performance::UsageObserver::new(app, target.vast_id, &path_and_query, &parts.method);

    let resp = match tokio::time::timeout(Duration::from_secs(120), client.request(up_req)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!(%e, %authority, "upstream request failed");
            if let Some(observer) = observer.take() {observer.finish(false);}
            return bad_gateway("upstream unreachable");
        }
        Err(_) => {
            if let Some(observer) = observer.take() {observer.finish(false);}
            return Response::builder().status(StatusCode::GATEWAY_TIMEOUT)
                .body(Body::from("upstream response headers timed out")).unwrap();
        },
    };

    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        return websocket_tunnel(target.vast_id, slot_id, resp, parts, tokens, slot_guard);
    }

    let (mut response_parts, body) = resp.into_parts();
    let tokens = connection_tokens(&response_parts.headers);
    strip_hop_by_hop(&mut response_parts.headers, &tokens);
    if let Some(observer) = observer.as_mut() {observer.response(response_parts.status, &response_parts.headers);}
    let body = Body::new(TrackedBody { body: Body::new(body), guard: slot_guard, observer });
    let resp = Response::from_parts(response_parts, body);
    tag_response(resp, slot_id, Some(target.vast_id), "proxy")
}

fn wants_upgrade(headers: &HeaderMap, _method: &Method) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().contains("websocket"))
        .unwrap_or(false)
}

/// 101-Tunnel: Client ↔ Upgrade-Streams bidirektional koppeln.
fn websocket_tunnel(
    vast_id: i64,
    slot_id: Option<i64>,
    upstream_resp: hyper::Response<Incoming>,
    client_parts: axum::http::request::Parts,
    tokens: Vec<String>,
    slot_guard: Option<crate::state::InFlightGuard>,
) -> Response {
    let mut builder = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    {
        let h = builder.headers_mut().unwrap();
        for (k, v) in upstream_resp.headers().iter() {
            h.insert(k, v.clone());
        }
        // Client-seitigen Hop-by-hop-Kram vom Original-Request übernehmen.
        let _ = tokens;
    }
    let client_resp = builder.body(Body::empty()).expect("101 response");

    // Client-Request für hyper::upgrade::on wieder zusammenbauen.
    let client_req = hyper::Request::from_parts(client_parts, Body::empty());

    tokio::spawn(async move {
        let up_fut = hyper::upgrade::on(upstream_resp);
        let client_fut = hyper::upgrade::on(client_req);
        let (Ok(client_up), Ok(up_up)) = tokio::join!(client_fut, up_fut) else {
            tracing::warn!(vast_id, "ws tunnel upgrade failed");
            return;
        };
        let mut client_up = hyper_util::rt::TokioIo::new(client_up);
        let mut up_up = hyper_util::rt::TokioIo::new(up_up);
        let res = tokio::io::copy_bidirectional(&mut client_up, &mut up_up).await;
        if let Err(e) = res {
            tracing::debug!(%e, vast_id, "ws tunnel closed");
        }
        drop(slot_guard);
    });
    tag_response(client_resp, slot_id, Some(vast_id), "proxy")
}

/// Ziel für Slot-Service auflösen (aktive Instanz + Port), mit Hold-Semantik.
async fn resolve_slot_target(
    app: &SharedApp,
    slot_id: i64,
    service: &str,
    req_headers: &HeaderMap,
) -> Result<UpstreamTarget, Response> {
    let Some(slot_cfg) = app.cfg().slot(slot_id).cloned() else {
        return Err(service_unavailable(slot_id, "unknown_slot"));
    };
    let Some(svc) = slot_cfg.services.get(service) else {
        return Err(service_unavailable(slot_id, "unknown_service"));
    };

    // Wait-Header: "X-Router-Wait: <sekunden>" — Verbindung halten bis healthy.
    let wait_s = req_headers
        .get("x-router-wait")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        .min(app.cfg().router.wait_for_backend_max_s);

    // Auto-Miete aus: kein impliziter Wake, kein Hold — Praxis/Cloud-Fallback
    // greift sofort statt X-Router-Wait-Sekunden zu verschwenden.
    if !app.db.auto_rent_enabled() {
        return Err(service_unavailable(slot_id, "auto_rent_off"));
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_s.max(1));
    // Pool-Routing: X-Router-Job-Id hält Batches auf EINER Box (Cache-
    // Lokalität für TTS-Sätze), sonst Round-Robin über alle healthy
    // Instanzen des Slots (parallele Chats auf verschiedenen Boxen).
    let job = req_headers
        .get("x-router-job-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    loop {
        if let Some((vast_id, nb_ip)) = app.pool_routes.pick(slot_id, job.as_deref()) {
            return Ok(UpstreamTarget { vast_id, nb_ip, port: svc.port });
        }
        // Fallback: klassisches Einzel-Target (z. B. Reconciler hat den Pool
        // noch nicht gefüllt, Boot-Race nach dem Start).
        if let Some((vast_id, Some(nb_ip), true)) = app.targets.get(slot_id) {
            if app.pool_routes.healthy(slot_id).is_empty() {
                return Ok(UpstreamTarget { vast_id, nb_ip, port: svc.port });
            }
        }
        if wait_s == 0 || tokio::time::Instant::now() >= deadline {
            let state = describe_slot_state(app, slot_id);
            return Err(service_unavailable(slot_id, &state));
        }
        // Kalt-Request = impliziter Wake.
        if !app.db.slot_desired(slot_id) {
            let _ = app.db.set_slot_desired_audited(slot_id, true, "proxy: impliziter Wake")
                .map_err(|e| tracing::warn!(%e, "audit write"));
            app.reconcile_now.notify_one();
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn describe_slot_state(app: &SharedApp, slot_id: i64) -> String {
    let target = app.targets.get(slot_id);
    // Herzstück: lebendige Progress-/Health-Daten des aktiven Agents, wenn
    // das Target auch wirklich healthy ist. Bei !healthy (z. B. frisch
    // gestoppte Box) ist der letzte Heartbeat STALE — dann DB-State melden.
    if let Some((vast_id, _, true)) = target {
        if let Some(hb) = app.hub.heartbeat(vast_id) {
            let health = hb.health_json.as_str().unwrap_or("");
            if health == "downloading" {
                let pct = hb
                    .progress_json
                    .get("pct")
                    .and_then(|p| p.as_f64())
                    .map(|p| format!("downloading {p:.0}%"))
                    .unwrap_or_else(|| "downloading".into());
                return pct;
            }
            return health.to_string();
        }
        return "warming".to_string();
    }
    // Kein brauchbares Target (keins / ungesund): aber es kann eine wärmende
    // oder gestoppte Box im Slot geben — Praxis soll im X-Router-State sehen,
    // ob ein Wake wartet (warming/progress) oder eine Kalt-Stop-Sequenz
    // durchlaufen wird (stopped), statt pauschal "cold".
    let mut best: Option<(String, i64)> = None;
    for row in app.db.instances(false).into_iter().filter(|r| r.slot_id == slot_id) {
        let label = match row.state.as_str() {
            "requested" | "provisioning" | "booting" | "agent_connected" => {
                let hb = app.hub.heartbeat(row.vast_id);
                if let Some(hb) = hb {
                    let health = hb.health_json.as_str().unwrap_or("");
                    if health == "downloading" {
                        hb.progress_json
                            .get("pct")
                            .and_then(|p| p.as_f64())
                            .map(|p| format!("downloading {p:.0}%"))
                            .unwrap_or_else(|| "downloading".into())
                    } else {
                        health.to_string()
                    }
                } else {
                    "warming".to_string()
                }
            }
            "healthy" => "warming".to_string(),
            "stopped" => "stopped".to_string(),
            "preempted" => "preempted".to_string(),
            "unreachable" => "unreachable".to_string(),
            _ => continue,
        };
        // Warming-Meldungen sind informativer als stopped; erster Treffer genügt.
        let is_stopped = label == "stopped";
        if best.is_none() || !is_stopped {
            best = Some((label, row.vast_id));
            if !is_stopped {
                break;
            }
        }
    }
    best.map(|(label, _)| label).unwrap_or_else(|| "cold".into())
}

/// Passthrough-Handler für Slot-Ports (8188/2700/11434/11435/11436).
pub async fn passthrough(
    app: SharedApp,
    slot_id: i64,
    service: String,
    req: Request,
) -> Response {
    app.traffic.mark(slot_id);
    app.db.mark_traffic(slot_id);

    let (parts, body) = req.into_parts();
    let path = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());

    // X-Router-Job-Id: Batch-Registrierung (Lücken zwischen Sätzen halten
    // den Slot busy, s. JobBatches::WINDOW_S).
    if let Some(job) = parts.headers.get("x-router-job-id").and_then(|v| v.to_str().ok()) {
        app.jobs.touch(slot_id, job);
    }

    // STT lokal: Sidecar statt media-Slot.
    if service == "stt" && app.cfg().stt.mode == "local" {
        return proxy_stt_local(&app, parts, body, &path).await;
    }

    let target = match resolve_slot_target(&app, slot_id, &service, &parts.headers).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let req = Request::from_parts(parts, body);
    proxy_to_target(&app, &app.proxy_client, &target, Some(slot_id), req, path, true).await
}

/// `/gpu/<slot>/<svc>/...` — Präfix strippen, Rest als Pfad an den Service.
pub async fn gpu_path(
    app: AppCtx,
    Path((slot_id, service)): Path<(i64, String)>,
    req: Request,
) -> Response {
    let req = strip_path_segments(req, 3); // "/gpu/<slot>/<svc>"
    passthrough(app.0, slot_id, service, req).await
}

/// `/inst/<vast_id>/<svc>/...` — Debug-Direktzugriff, umgeht Slot-Flip.
pub async fn inst_path(
    app: AppCtx,
    Path((vast_id, service)): Path<(i64, String)>,
    req: Request,
) -> Response {
    let req = strip_path_segments(req, 3); // "/inst/<id>/<svc>"
    let Some(inst) = app.db.instance(vast_id) else {
        return service_unavailable(vast_id, "unknown_instance");
    };
    let Some(slot_cfg) = app.cfg().slot(inst.slot_id).cloned() else {
        return service_unavailable(inst.slot_id, "unknown_slot");
    };
    let Some(svc) = slot_cfg.services.get(&service) else {
        return service_unavailable(inst.slot_id, "unknown_service");
    };
    let nb_ip = inst
        .nb_ip
        .clone()
        .or_else(|| app.hub.nb_ip(vast_id))
        .unwrap_or_default();
    if nb_ip.is_empty() {
        return service_unavailable(inst.slot_id, "no_nb_ip");
    }
    let (parts, body) = req.into_parts();
    let path = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    let target = UpstreamTarget { vast_id, nb_ip, port: svc.port };
    let req = Request::from_parts(parts, body);
    proxy_to_target(&app.0, &app.proxy_client, &target, Some(inst.slot_id), req, path, false).await
}

/// Entfernt die ersten `n` Pfad-Segmente (plus führenden Slash) aus der
/// Request-URI — für `/gpu/<slot>/<svc>/…` und `/inst/<id>/<svc>/…`, damit der
/// Upstream nur den Rest sieht (sonst 404 im Backend).
fn strip_path_segments(req: Request, n: usize) -> Request {
    let (mut parts, body) = req.into_parts();
    if let Some(pq) = parts.uri.path_and_query() {
        let raw_path = pq.path();
        let query = pq.query().map(|q| q.to_string());
        let rest = raw_path
            .splitn(n + 2, '/')
            .skip(n + 1)
            .next()
            .unwrap_or("");
        let new_path = if rest.is_empty() { "/".to_string() } else { format!("/{rest}") };
        let new_uri = match query {
            Some(q) => format!("{new_path}?{q}"),
            None => new_path,
        };
        if let Ok(uri) = new_uri.parse::<axum::http::Uri>() {
            parts.uri = uri;
        }
    }
    Request::from_parts(parts, body)
}

/// STT im Router-Compose (mode=local). WS-Sessions zählen fürs Dashboard.
async fn proxy_stt_local(
    app: &SharedApp,
    parts: axum::http::request::Parts,
    body: Body,
    path: &str,
) -> Response {
    let wants_ws = wants_upgrade(&parts.headers, &parts.method);
    let mut parts = parts;
    parts.headers.remove(header::ORIGIN);

    if wants_ws {
        // Vosk-WS: über axum-WS terminieren und Byte-Payload weiterreichen.
        return stt_ws_relay(app, parts, body, path).await;
    }

    let url = format!("{}{}", app.cfg().stt.url.trim_end_matches('/'), path);
    let client = reqwest::Client::new();
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut rreq = client.request(method, &url);
    let mut hm = reqwest::header::HeaderMap::new();
    for (k, v) in parts.headers.iter() {
        if k == header::HOST || k == header::ORIGIN {
            continue;
        }
        if let Ok(name) = reqwest::header::HeaderName::from_bytes(k.as_str().as_bytes()) {
            if let Ok(val) = reqwest::header::HeaderValue::from_bytes(v.as_bytes()) {
                hm.insert(name, val);
            }
        }
    }
    rreq = rreq.headers(hm);
    // Forward upload chunks with backpressure; never buffer the entire audio file.
    rreq = rreq.body(reqwest::Body::wrap_stream(body.into_data_stream()));

    match rreq.send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut builder = Response::builder().status(status);
            for (k, v) in resp.headers().iter() {
                if k != reqwest::header::HeaderName::from_static("transfer-encoding") {
                    if let Ok(hv) = HeaderValue::from_bytes(v.as_bytes()) {
                        builder = builder.header(k.as_str(), hv);
                    }
                }
            }
            let body = Body::from_stream(resp.bytes_stream());
            builder.body(body).unwrap_or_else(|_| bad_gateway("stt relay body"))
        }
        Err(e) => {
            tracing::warn!(%e, "stt sidecar unreachable");
            bad_gateway(&format!("stt sidecar unreachable: {e}"))
        }
    }
}

/// WS-Relay für den lokalen Vosk-Server: Binary-PCM und Text-Frames 1:1.
async fn stt_ws_relay(app: &SharedApp, parts: axum::http::request::Parts, _body: Body, path: &str) -> Response {
    use std::sync::atomic::Ordering;
    let ws_req = Request::from_parts(parts, Body::empty());
    let Ok(ws) = axum::extract::ws::WebSocketUpgrade::from_request(ws_req, &()).await else {
        return bad_gateway("expected websocket upgrade");
    };
    let stt_base = app.cfg().stt.url.trim_end_matches('/').to_string();
    let stt_base = stt_base.replacen("http://", "ws://", 1).replacen("https://", "wss://", 1);
    let path_owned = path.trim_start_matches('/').to_string();
    let app2 = app.clone();
    ws.on_upgrade(move |client_ws| async move {
        use futures::SinkExt;
        use tokio_tungstenite::tungstenite::Message as TMsg;
        struct Session(SharedApp);
        impl Drop for Session {
            fn drop(&mut self) { self.0.stt_sessions.fetch_sub(1, Ordering::Relaxed); }
        }
        app2.stt_sessions.fetch_add(1, Ordering::Relaxed);
        let _session = Session(app2);
        let ws_url = format!("{}/{}", stt_base, path_owned);
        let Ok(Ok((up_ws, _))) = tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::connect_async(&ws_url)).await else { return; };
        let (mut up_sink, mut up_stream) = up_ws.split();
        let (mut sink, mut stream) = client_ws.split();
        // Preserve frame types, apply backpressure, cancel the other direction
        // as soon as either peer closes. No detached tasks/unbounded queues.
        let upstream = async {
            while let Some(Ok(msg)) = up_stream.next().await {
                let msg = match msg {
                    TMsg::Binary(b) => Message::Binary(b),
                    TMsg::Text(t) => Message::Text(t),
                    TMsg::Ping(p) => Message::Ping(p),
                    TMsg::Pong(p) => Message::Pong(p),
                    TMsg::Close(_) => break,
                    TMsg::Frame(_) => continue,
                };
                if sink.send(msg).await.is_err() { break; }
            }
        };
        let downstream = async {
            while let Some(Ok(msg)) = stream.next().await {
                let msg = match msg {
                    Message::Binary(b) => TMsg::Binary(b),
                    Message::Text(t) => TMsg::Text(t),
                    Message::Ping(p) => TMsg::Ping(p),
                    Message::Pong(p) => TMsg::Pong(p),
                    Message::Close(_) => break,
                };
                if up_sink.send(msg).await.is_err() { break; }
            }
        };
        tokio::select! { _ = upstream => {}, _ = downstream => {} }
        // Dropping both sockets also covers a stalled/abruptly disconnected peer.
    })
}