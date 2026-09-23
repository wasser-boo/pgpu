//! Opt-in OpenAI-compatible LLM fallback. GPU routing remains the first choice.
//! Quota mechanics live in the SQLite store; no key/prompt is stored there.
pub mod config;
mod headers;
mod status;
#[cfg(test)]
mod tests;
pub mod ui;

use crate::{db::free_router_store::Feedback, state::SharedApp};
use axum::{
    body::Body,
    extract::Request,
    http::{header, Method, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::{collections::HashSet, sync::Mutex, time::Duration};

pub struct Router {
    client: reqwest::Client,
    next: Mutex<usize>,
}
struct Attempt {
    provider: config::Provider,
    key: String,
    payload: Vec<u8>,
    tokens: u64,
    reservation: crate::db::free_router_store::Reservation,
}
impl Router {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .read_timeout(Duration::from_secs(120))
                .build()?,
            next: Mutex::new(0),
        })
    }

    /// Advance to the next *admitted* provider, not merely the next configured
    /// entry. Cursor + reservation are synchronous/atomic even under concurrency;
    /// disabled/cooling entries cannot bias the jumper toward their neighbor.
    fn reserve(
        &self,
        app: &SharedApp,
        cfg: &config::Config,
        body: &serde_json::Value,
        tried: &mut HashSet<usize>,
        retry: &mut Option<u64>,
    ) -> anyhow::Result<Option<Attempt>> {
        let mut cursor = self.next.lock().unwrap();
        let start = if cfg.jumper { *cursor } else { 0 };
        for offset in 0..cfg.providers.len() {
            let index = (start + offset) % cfg.providers.len();
            if !tried.insert(index) {
                continue;
            }
            let p = &cfg.providers[index];
            if !p.enabled {
                continue;
            }
            let key = p.key();
            if key.is_empty() {
                continue;
            }
            let mut payload = body.clone();
            // Only chat parameters cross the trust boundary. In particular,
            // OpenRouter `models`/`route`/`provider`/plugins must not enable a
            // paid fallback behind the explicitly configured free model.
            payload.as_object_mut().unwrap().retain(|name, _| {
                matches!(
                    name.as_str(),
                    "messages"
                        | "model"
                        | "max_tokens"
                        | "max_completion_tokens"
                        | "stream"
                        | "stream_options"
                        | "temperature"
                        | "top_p"
                        | "seed"
                        | "frequency_penalty"
                        | "presence_penalty"
                        | "stop"
                        | "tools"
                        | "tool_choice"
                        | "parallel_tool_calls"
                        | "response_format"
                        | "logprobs"
                        | "top_logprobs"
                        | "user"
                        | "n"
                        | "reasoning_effort"
                        | "verbosity"
                        | "functions"
                        | "function_call"
                )
            });
            payload["model"] = p.model.clone().into();
            let cap = [body.get("max_tokens"), body.get("max_completion_tokens")]
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_u64())
                .min()
                .unwrap_or(p.max_output_tokens as u64)
                .min(p.max_output_tokens as u64);
            let output_field = if payload.get("max_completion_tokens").is_some() {
                "max_completion_tokens"
            } else {
                "max_tokens"
            };
            if output_field == "max_completion_tokens" {
                payload.as_object_mut().unwrap().remove("max_tokens");
            }
            payload[output_field] = cap.into();
            let payload = serde_json::to_vec(&payload)?;
            // Conservative byte-based input estimate + full output cap. Never
            // refund unused output, including SSE and cancelled requests.
            let tokens = payload.len() as u64 + cap;
            match app.db.reserve_free_request(
                &p.bucket(&key),
                p,
                p.buffer(cfg),
                tokens,
                chrono::Utc::now().timestamp_millis(),
            )? {
                Ok(reservation) => {
                    if cfg.jumper {
                        *cursor = (index + 1) % cfg.providers.len();
                    }
                    return Ok(Some(Attempt {
                        provider: p.clone(),
                        key,
                        payload,
                        tokens,
                        reservation,
                    }));
                }
                Err(q) => *retry = Some(retry.map_or(q.retry_after_s, |r| r.min(q.retry_after_s))),
            }
        }
        Ok(None)
    }
}

pub fn eligible(app: &SharedApp, slot_id: i64, service: &str, req: &Request) -> bool {
    let cfg = app.cfg();
    cfg.free_router.use_when_all_offline
        && service == "api"
        && cfg
            .slot(slot_id)
            .is_some_and(|s| s.role == praxis_common::Role::Llm && s.services.contains_key(service))
        && app.pool_routes.healthy(slot_id).is_empty()
        && !app
            .targets
            .get(slot_id)
            .is_some_and(|(_, ip, healthy)| healthy && ip.is_some())
        && ((req.method() == Method::POST && req.uri().path() == "/v1/chat/completions")
            || (req.method() == Method::GET && req.uri().path() == "/v1/models"))
}

fn error(status: StatusCode, state: &str, message: &str, retry: Option<u64>) -> Response {
    let mut response = (
        status,
        Json(serde_json::json!({"error":{"message":message,"type":state}})),
    )
        .into_response();
    response
        .headers_mut()
        .insert("x-router-state", state.parse().unwrap());
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    if let Some(retry) = retry {
        response.headers_mut().insert(
            header::RETRY_AFTER,
            retry.max(1).to_string().parse().unwrap(),
        );
    }
    response
}

/// v1 deliberately accepts text chat only. Image/audio token costs cannot be
/// bounded by the JSON byte size, so do not pretend to enforce their quotas.
fn text_chat(value: &serde_json::Value) -> bool {
    value
        .get("messages")
        .and_then(|v| v.as_array())
        .is_some_and(|messages| {
            !messages.is_empty()
                && messages.iter().all(|m| {
                    m.is_object()
                        && m.get("audio").is_none_or(|v| v.is_null())
                        && m.get("role").is_some_and(|v| v.is_string())
                        && m.get("content").is_none_or(|c| {
                            c.is_null()
                                || c.is_string()
                                || c.as_array().is_some_and(|parts| {
                                    parts.iter().all(|p| {
                                        p.get("type").and_then(|v| v.as_str()) == Some("text")
                                            && p.get("text").is_some_and(|v| v.is_string())
                                    })
                                })
                        })
                })
        })
        && value.get("n").is_none_or(|v| v.as_u64() == Some(1))
        && value.get("audio").is_none_or(|v| v.is_null())
        && value.get("tools").is_none_or(|v| {
            v.is_null()
                || v.as_array().is_some_and(|tools| {
                    tools
                        .iter()
                        .all(|tool| tool.get("type").and_then(|v| v.as_str()) == Some("function"))
                })
        })
        && value
            .get("modalities")
            .is_none_or(|v| v == &serde_json::json!(["text"]))
}

/// Independent agent endpoint on the dashboard port. This is a dynamic fallback
/// so changing endpoint_path works immediately, without listener restarts. Only
/// the exact configured paths are claimed; unrelated unknown routes remain 404.
pub async fn endpoint(app: crate::state::AppCtx, req: Request) -> Response {
    let cfg = app.cfg();
    let suffix = req.uri().path().strip_prefix(&cfg.free_router.endpoint_path);
    let Some(suffix) = suffix.filter(|s| matches!(*s, "" | "/" | "/health" | "/models" | "/chat/completions")) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !crate::api::check_token(&app.0, &req) {
        return error(StatusCode::UNAUTHORIZED, "free_router_unauthorized", "Use the PGPU Router token as API key", None);
    }
    let chat = suffix == "/chat/completions";
    if (chat && req.method() != Method::POST) || (!chat && req.method() != Method::GET) {
        let mut response = error(StatusCode::METHOD_NOT_ALLOWED, "free_router_invalid_request", "Method not allowed", None);
        response.headers_mut().insert(header::ALLOW, if chat { "POST" } else { "GET" }.parse().unwrap());
        return response;
    }
    if matches!(suffix, "" | "/" | "/health") {
        let status = status::snapshot(&app.db, &cfg.free_router, chrono::Utc::now().timestamp_millis());
        let code = if status.available { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
        let mut response = (code, Json(serde_json::json!({"active":status.active,"available":status.available,
            "retry_after_s":status.retry_after_s,"endpoint_path":cfg.free_router.endpoint_path,
            "mode":if cfg.free_router.jumper { "jumper" } else { "priority" }}))).into_response();
        response.headers_mut().insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
        if !status.available { response.headers_mut().insert(header::RETRY_AFTER, status.retry_after_s.to_string().parse().unwrap()); }
        return response;
    }
    if !cfg.free_router.providers.iter().any(|p| p.enabled && !p.key().is_empty()) {
        return error(StatusCode::SERVICE_UNAVAILABLE, "free_router_unconfigured", "No enabled provider with an API key; configure one in Settings", Some(60));
    }
    // Deliberately bypass GPU availability and the optional GPU fallback switch.
    proxy(&app.0, None, req).await
}

pub async fn proxy(app: &SharedApp, slot_id: Option<i64>, req: Request) -> Response {
    let cfg = app.cfg();
    let free = &cfg.free_router;
    if req.method() == Method::GET {
        let mut response = Json(serde_json::json!({"object":"list","data":[{"id":"free-router","object":"model","created":0,"owned_by":"pgpu"}]})).into_response();
        response
            .headers_mut()
            .insert("x-router-state", "free_router".parse().unwrap());
        return response;
    }
    let Ok(Ok(bytes)) = tokio::time::timeout(
        Duration::from_secs(15),
        axum::body::to_bytes(req.into_body(), 2 * 1024 * 1024),
    )
    .await
    else {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "free_router_invalid_request",
            "Body exceeds 2 MiB or upload timed out",
            None,
        );
    };
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return error(
            StatusCode::BAD_REQUEST,
            "free_router_invalid_request",
            "Expected JSON chat request",
            None,
        );
    };
    if !text_chat(&body) {
        return error(
            StatusCode::BAD_REQUEST,
            "free_router_invalid_request",
            "Free router supports text chat with n=1 only",
            None,
        );
    }
    for field in ["max_tokens", "max_completion_tokens"] {
        if body
            .get(field)
            .is_some_and(|v| v.as_u64().is_none_or(|n| n == 0 || n > 131072))
        {
            return error(
                StatusCode::BAD_REQUEST,
                "free_router_invalid_request",
                "Invalid output token limit",
                None,
            );
        }
    }
    let mut tried = HashSet::new();
    let mut retry = None::<u64>;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    // At most one attempt/provider. No retry once response streaming begins.
    for _ in 0..free.providers.len() {
        let Attempt {
            provider: p,
            key,
            payload,
            tokens,
            reservation,
        } = match app
            .free_router
            .reserve(app, free, &body, &mut tried, &mut retry)
        {
            Ok(Some(attempt)) => attempt,
            Ok(None) => break,
            Err(_) => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "free_router_ledger_unavailable",
                    "Quota ledger unavailable; routing stopped",
                    Some(10),
                )
            }
        };
        let attempt_deadline = deadline.min(tokio::time::Instant::now() + Duration::from_secs(15));
        let result = tokio::time::timeout_at(
            attempt_deadline,
            app.free_router
                .client
                .post(format!(
                    "{}/chat/completions",
                    p.base_url.trim_end_matches('/')
                ))
                .bearer_auth(&key)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .body(payload)
                .send(),
        )
        .await;
        let now = chrono::Utc::now().timestamp_millis();
        let feedback = match &result {
            Ok(Ok(response)) => headers::feedback(response.status(), response.headers(), now),
            _ => Feedback {
                cooldown_until: now + 30_000,
                reason: "connection failure / timeout",
                ..Default::default()
            },
        };
        let retryable = feedback.cooldown_until > now;
        if app
            .db
            .free_request_feedback(&reservation, feedback, now)
            .is_err()
        {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "free_router_ledger_unavailable",
                "Could not persist provider quota; routing stopped",
                Some(10),
            );
        }
        if retryable {
            // The attempt itself may also have exhausted a daily/token quota.
            // Retry-After must include that, not just the HTTP error cooldown.
            let Ok(quota) =
                app.db
                    .free_router_quota(&reservation.bucket, &p, p.buffer(free), tokens, now)
            else {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "free_router_ledger_unavailable",
                    "Quota ledger unavailable; routing stopped",
                    Some(10),
                );
            };
            let wait = quota.retry_after_s.max(1);
            retry = Some(retry.map_or(wait, |r| r.min(wait)));
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            continue;
        }
        let Ok(Ok(response)) = result else {
            unreachable!("transport errors always have a cooldown");
        };
        let status = response.status();
        if !status.is_success() {
            // Do not replay invalid requests, follow redirects or echo provider
            // error bodies that might contain credentials/private request data.
            return error(
                if status.is_client_error() {
                    status
                } else {
                    StatusCode::BAD_GATEWAY
                },
                "free_router_rejected",
                "Provider rejected request; check model capabilities and parameters",
                None,
            );
        }
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .cloned()
            .unwrap_or_else(|| "application/json".parse().unwrap());
        let mut response = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CACHE_CONTROL, "no-store")
            .header("x-router-state", "free_router")
            .header("x-free-router-provider", &p.id)
            .header(
                "x-free-router-mode",
                if free.jumper { "jumper" } else { "priority" },
            )
            .body(Body::from_stream(response.bytes_stream()))
            .unwrap();
        if let Some(slot_id) = slot_id {
            response.headers_mut().insert("x-gpu-slot", slot_id.to_string().parse().unwrap());
        }
        return response;
    }
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "free_router_exhausted",
        "No configured free provider currently has usable quota; retry later",
        Some(retry.unwrap_or(60)),
    )
}
