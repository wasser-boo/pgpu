use super::*;
use crate::{
    db::Db,
    state::AppCtx,
    test_support::{LocalServer, MockProvider, TestApp},
};
use axum::http::HeaderMap;
use config::{Config, Provider};
use http_body_util::BodyExt;
use std::sync::atomic::{AtomicUsize, Ordering};

fn provider(id: &str, url: &str) -> Provider {
    Provider {
        id: id.into(),
        enabled: true,
        base_url: format!("{url}/v1"),
        api_key: format!("key-{id}"),
        model: format!("model-{id}"),
        requests_per_minute: 8,
        max_output_tokens: 20,
        ..Default::default()
    }
}
fn configure(t: &TestApp, providers: Vec<Provider>, jumper: bool) {
    let mut cfg = (*t.app.cfg()).clone();
    cfg.free_router = Config {
        use_when_all_offline: true,
        jumper,
        providers,
        ..Default::default()
    };
    cfg.slots[0].services.insert(
        "api".into(),
        crate::config::ServiceCfg {
            port: 11434,
            health: None,
            busy: Default::default(),
        },
    );
    t.app.cfg_swap(cfg);
}
fn request() -> Request {
    Request::builder().method("POST").uri("/v1/chat/completions")
        .header("authorization", "Bearer ROUTER-PRIVATE").header("cookie", "PRIVATE-COOKIE")
        .header("x-api-key", "PRIVATE-KEY").header("x-router-wait", "600")
        .body(Body::from(r#"{"model":"local-gpu","messages":[{"role":"user","content":"hello"}],"max_tokens":1000}"#)).unwrap()
}
async fn route(t: &TestApp) -> Response {
    crate::proxy::passthrough(t.app.clone(), 1, "api".into(), request()).await
}

#[test]
fn configuration_defaults_and_validation_fail_closed() {
    let mut cfg = Config::default();
    assert!(!cfg.use_when_all_offline && !cfg.jumper);
    assert_eq!(cfg.safety_buffer_requests, 5);
    let mut p = provider("a", "https://api.example");
    cfg.providers.push(p.clone());
    cfg.validate().unwrap();
    p.requests_per_minute = 5;
    cfg.providers[0] = p.clone();
    assert!(cfg.validate().is_err());
    p.safety_buffer_requests = Some(1);
    cfg.providers[0] = p;
    cfg.validate().unwrap();
    cfg.providers[0].base_url = "http://public.example/v1".into();
    assert!(cfg.validate().is_err());
    cfg.providers[0].base_url = "https://secret@example.org/v1".into();
    assert!(!cfg.validate().unwrap_err().to_string().contains("secret"));
    cfg.providers[0] = provider("a", "https://api.example");
    cfg.providers.push(cfg.providers[0].clone());
    assert!(cfg.validate().is_err());
    assert!(serde_json::from_str::<Config>(r#"{"jumpre":true}"#).is_err());
    assert!(!format!("{:?}", cfg.providers[0]).contains("key-a"));
}

#[test]
fn reservations_keep_five_spare_and_survive_restart_and_renaming() {
    let t = TestApp::new();
    let p = provider("a", "https://api.example");
    let bucket = p.bucket(&p.key());
    let now = 1_000_000;
    for _ in 0..3 {
        assert!(t
            .app
            .db
            .reserve_free_request(&bucket, &p, 5, 10, now)
            .unwrap()
            .is_ok());
    }
    let q = t
        .app
        .db
        .reserve_free_request(&bucket, &p, 5, 10, now)
        .unwrap()
        .unwrap_err();
    assert_eq!((q.remaining_requests, q.retry_after_s), (0, 60));
    let reopened = Db::open(&t.dir.join("test.sqlite")).unwrap();
    let mut renamed = p.clone();
    renamed.id = "other".into();
    renamed.model = "another-model".into();
    renamed.base_url.push_str("/alternate");
    assert_eq!(renamed.bucket(&renamed.key()), bucket);
    assert!(reopened
        .reserve_free_request(&bucket, &renamed, 5, 10, now + 1)
        .unwrap()
        .is_err());
    assert!(reopened
        .reserve_free_request(&bucket, &renamed, 5, 10, now + 60_000)
        .unwrap()
        .is_ok());
}

#[test]
fn concurrent_reservation_cannot_overshoot() {
    let t = TestApp::new();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(20));
    let handles: Vec<_> = (0..20)
        .map(|_| {
            let db = t.app.db.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                db.reserve_free_request(
                    "shared",
                    &provider("a", "https://api.example"),
                    5,
                    20,
                    1_000_000,
                )
                .unwrap()
                .is_ok()
            })
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .map(|h| usize::from(h.join().unwrap()))
            .sum::<usize>(),
        3
    );
}

#[test]
fn daily_monthly_token_and_spacing_limits_are_independent() {
    let t = TestApp::new();
    let now = 1_000_000;
    let mut p = provider("a", "https://api.example");
    p.requests_per_day = Some(6);
    t.app
        .db
        .reserve_free_request("day", &p, 5, 10, now)
        .unwrap()
        .unwrap();
    assert_eq!(
        t.app
            .db
            .reserve_free_request("day", &p, 5, 10, now + 60_000)
            .unwrap()
            .unwrap_err()
            .retry_after_s,
        86340
    );
    assert!(t
        .app
        .db
        .reserve_free_request("day", &p, 5, 10, now + 86_400_000)
        .unwrap()
        .is_ok());
    p.requests_per_day = None;
    p.requests_per_month = Some(6);
    t.app
        .db
        .reserve_free_request("month", &p, 5, 10, now)
        .unwrap()
        .unwrap();
    assert!(t
        .app
        .db
        .reserve_free_request("month", &p, 5, 10, now + 30 * 86_400_000)
        .unwrap()
        .is_err());
    assert!(t
        .app
        .db
        .reserve_free_request("month", &p, 5, 10, now + 31 * 86_400_000)
        .unwrap()
        .is_ok());
    p.requests_per_month = None;
    p.tokens_per_minute = Some(30);
    t.app
        .db
        .reserve_free_request("tokens", &p, 5, 20, now)
        .unwrap()
        .unwrap();
    assert_eq!(
        t.app
            .db
            .reserve_free_request("tokens", &p, 5, 20, now)
            .unwrap()
            .unwrap_err()
            .reason,
        "tokens/minute"
    );
    p.tokens_per_minute = None;
    p.min_interval_ms = 1000;
    t.app
        .db
        .reserve_free_request("spacing", &p, 5, 10, now)
        .unwrap()
        .unwrap();
    assert!(t
        .app
        .db
        .reserve_free_request("spacing", &p, 5, 10, now + 999)
        .unwrap()
        .is_err());
    assert!(t
        .app
        .db
        .reserve_free_request("spacing", &p, 5, 10, now + 1000)
        .unwrap()
        .is_ok());
}

#[test]
fn provider_headers_constrain_local_quota_and_never_restore_it_out_of_order() {
    let t = TestApp::new();
    let p = provider("a", "https://api.example");
    let now = 1_000_000;
    let a = t
        .app
        .db
        .reserve_free_request("remote", &p, 5, 10, now)
        .unwrap()
        .unwrap();
    let b = t
        .app
        .db
        .reserve_free_request("remote", &p, 5, 10, now)
        .unwrap()
        .unwrap();
    let mut h = HeaderMap::new();
    h.insert("x-ratelimit-remaining-requests", "6".parse().unwrap());
    h.insert("x-ratelimit-reset-requests", "2m".parse().unwrap());
    t.app
        .db
        .free_request_feedback(&b, headers::feedback(StatusCode::OK, &h, now), now)
        .unwrap();
    h.insert("x-ratelimit-remaining-requests", "100".parse().unwrap());
    t.app
        .db
        .free_request_feedback(&a, headers::feedback(StatusCode::OK, &h, now), now)
        .unwrap();
    assert_eq!(
        t.app
            .db
            .reserve_free_request("remote", &p, 5, 10, now + 60_000)
            .unwrap()
            .unwrap_err()
            .retry_after_s,
        60
    );
    assert!(t
        .app
        .db
        .reserve_free_request("remote", &p, 5, 10, now + 120_000)
        .unwrap()
        .is_ok());
}

#[tokio::test]
async fn priority_rotates_at_reserve_and_exhaustion_returns_retry_after() {
    let t = TestApp::new();
    let a = MockProvider::new().await;
    let b = MockProvider::new().await;
    configure(
        &t,
        vec![
            provider("a", &a.server.url()),
            provider("b", &b.server.url()),
        ],
        false,
    );
    t.app.db.set_auto_rent(false).unwrap();
    for expected in ["a", "a", "a", "b", "b", "b"] {
        let r = route(&t).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()["x-free-router-provider"], expected);
    }
    let r = route(&t).await;
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.headers()["x-router-state"], "free_router_exhausted");
    assert!(
        r.headers()["retry-after"]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > 0
    );
    assert_eq!(a.calls.lock().unwrap()[0].1, "/v1/chat/completions");
    assert_eq!(a.calls.lock().unwrap()[0].2["model"], "model-a");
    assert_eq!(a.calls.lock().unwrap()[0].2["max_tokens"], 20);
    assert!(!t.app.db.slot_desired(1));
    assert_eq!(t.app.traffic.snapshot(1).last_request, 0);
    assert!(t.app.db.instances(true).is_empty());
}

#[tokio::test]
async fn jumper_round_robins_and_skips_disabled_missing_keys_and_exhausted_providers() {
    let t = TestApp::new();
    let a = MockProvider::new().await;
    let b = MockProvider::new().await;
    let mut disabled = provider("disabled", &a.server.url());
    disabled.enabled = false;
    let mut missing = provider("missing", &a.server.url());
    missing.api_key.clear();
    configure(
        &t,
        vec![
            provider("a", &a.server.url()),
            provider("b", &b.server.url()),
        ],
        true,
    );
    for expected in ["a", "b", "a", "b"] {
        let r = route(&t).await;
        assert_eq!(r.headers()["x-free-router-provider"], expected);
        assert_eq!(r.headers()["x-free-router-mode"], "jumper");
    }
    // Persisted quota is retained across hot reload; the cursor never bypasses it.
    configure(
        &t,
        vec![
            disabled,
            missing,
            provider("a", &a.server.url()),
            provider("b", &b.server.url()),
        ],
        true,
    );
    for _ in 0..2 {
        assert_eq!(route(&t).await.status(), StatusCode::OK);
    }
    assert_eq!(route(&t).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(a.calls.lock().unwrap().len(), 3);
    assert_eq!(b.calls.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn retry_after_skips_provider_and_persists_cooldown() {
    let t = TestApp::new();
    let b = MockProvider::new().await;
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let a = LocalServer::new(axum::Router::new().fallback(move || {
        count.fetch_add(1, Ordering::Relaxed);
        async {
            Response::builder()
                .status(429)
                .header("retry-after", "120")
                .body(Body::empty())
                .unwrap()
        }
    }))
    .await;
    configure(
        &t,
        vec![provider("a", &a.url()), provider("b", &b.server.url())],
        false,
    );
    for _ in 0..2 {
        assert_eq!(route(&t).await.headers()["x-free-router-provider"], "b");
    }
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let db = Db::open(&t.dir.join("test.sqlite")).unwrap();
    let p = &t.app.cfg().free_router.providers[0];
    let q = db
        .free_router_quota(
            &p.bucket(&p.key()),
            p,
            5,
            10,
            chrono::Utc::now().timestamp_millis(),
        )
        .unwrap();
    assert!(q.retry_after_s >= 119);
}

#[tokio::test]
async fn streaming_preserves_sse_and_never_forwards_local_credentials_or_cookies() {
    let t = TestApp::new();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(2);
    let rx = std::sync::Arc::new(std::sync::Mutex::new(Some(rx)));
    let a =
        LocalServer::new(axum::Router::new().fallback(move |req: Request| {
            let rx = rx.lock().unwrap().take().unwrap();
            async move {
                assert_eq!(req.headers()["authorization"], "Bearer key-a");
                for h in ["cookie", "x-api-key", "x-router-wait", "origin"] {
                    assert!(!req.headers().contains_key(h));
                }
                let stream = futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|v| (v, rx))
                });
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .header("set-cookie", "evil=1")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
        }))
        .await;
    configure(&t, vec![provider("a", &a.url())], false);
    let r = route(&t).await;
    assert_eq!(r.headers()["content-type"], "text/event-stream");
    assert!(!r.headers().contains_key("set-cookie"));
    let mut body = r.into_body();
    let frame = bytes::Bytes::from_static(b"data: {\"choices\":[]}\n\n");
    tx.send(Ok(frame.clone())).await.unwrap();
    assert_eq!(
        body.frame().await.unwrap().unwrap().into_data().unwrap(),
        frame
    );
    drop(body);
    drop(tx); // A cancelled stream still consumed its reservation.
    let p = &t.app.cfg().free_router.providers[0];
    assert_eq!(
        t.app
            .db
            .free_router_quota(
                &p.bucket(&p.key()),
                p,
                5,
                1,
                chrono::Utc::now().timestamp_millis()
            )
            .unwrap()
            .remaining_requests,
        2
    );
}

#[tokio::test]
async fn healthy_gpu_wins_even_with_jumper_or_rent_off_and_fallback_switch_is_opt_in() {
    let t = TestApp::new();
    let gpu = MockProvider::new().await;
    let free = MockProvider::new().await;
    configure(&t, vec![provider("free", &free.server.url())], true);
    let mut cfg = (*t.app.cfg()).clone();
    cfg.slots[0].services.get_mut("api").unwrap().port = gpu.server.addr.port();
    t.app.cfg_swap(cfg);
    t.app
        .targets
        .set(1, Some(11), Some("127.0.0.1".into()), true);
    t.app.db.set_auto_rent(false).unwrap();
    assert_eq!(route(&t).await.headers()["x-router-state"], "proxy");
    assert!(free.calls.lock().unwrap().is_empty());
    t.app.targets.set(1, None, None, false);
    assert_eq!(route(&t).await.headers()["x-router-state"], "free_router");
    let mut cfg = (*t.app.cfg()).clone();
    cfg.free_router.use_when_all_offline = false;
    t.app.cfg_swap(cfg);
    assert_eq!(route(&t).await.headers()["x-router-state"], "auto_rent_off");
    assert_eq!(free.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unsupported_paths_media_and_invalid_payloads_are_not_sent() {
    let t = TestApp::new();
    let free = MockProvider::new().await;
    configure(&t, vec![provider("free", &free.server.url())], false);
    t.app.db.set_auto_rent(false).unwrap();
    for path in ["/v1/embeddings", "/api/chat", "/v1/chat/completions/extra"] {
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            crate::proxy::passthrough(t.app.clone(), 1, "api".into(), req)
                .await
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
    for body in [
        "bad-json",
        "{}",
        r#"{"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example/a"}}]}]}"#,
        r#"{"messages":[{"role":"user","content":"hi"}],"n":2}"#,
    ] {
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .body(Body::from(body))
            .unwrap();
        assert_eq!(
            crate::proxy::passthrough(t.app.clone(), 1, "api".into(), req)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let mut cfg = (*t.app.cfg()).clone();
    cfg.slots[0].role = praxis_common::Role::Media;
    t.app.cfg_swap(cfg);
    assert_eq!(route(&t).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(free.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn client_error_is_not_retried_and_redirects_are_not_followed() {
    let t = TestApp::new();
    let a = MockProvider::new().await;
    let b = MockProvider::new().await;
    configure(
        &t,
        vec![
            provider("a", &a.server.url()),
            provider("b", &b.server.url()),
        ],
        false,
    );
    a.respond(400, "secret error");
    let r = route(&t).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(
        !String::from_utf8_lossy(&axum::body::to_bytes(r.into_body(), 8192).await.unwrap())
            .contains("secret")
    );
    assert!(b.calls.lock().unwrap().is_empty());
    let location = format!("{}/steal", b.server.url());
    let redirect = LocalServer::new(axum::Router::new().fallback(move || {
        let location = location.clone();
        async move {
            Response::builder()
                .status(307)
                .header("location", location)
                .body(Body::empty())
                .unwrap()
        }
    }))
    .await;
    configure(&t, vec![provider("redirect", &redirect.url())], false);
    assert_eq!(route(&t).await.status(), StatusCode::BAD_GATEWAY);
    assert!(b.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn settings_are_authenticated_and_keys_never_appear_in_status() {
    let t = TestApp::new();
    configure(&t, vec![provider("a", "https://example.org")], true);
    let r = ui::get(AppCtx(t.app.clone()), Request::new(Body::empty())).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = ui::save(AppCtx(t.app.clone()), Request::new(Body::empty())).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = ui::get(AppCtx(t.app.clone()), t.request("")).await;
    assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
    let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("key-a"));
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["config"]["jumper"], true);
    assert_eq!(json["statuses"][0]["key_configured"], true);
}

#[tokio::test]
async fn client_cannot_override_free_model_with_paid_routing_extras() {
    let t = TestApp::new();
    let server = MockProvider::new().await;
    configure(&t, vec![provider("a", &server.server.url())], false);
    let body = serde_json::json!({"model":"paid", "models":["paid"], "route":"fallback",
        "provider":{"order":["paid"]}, "plugins":[{"id":"web"}], "cache_prompt":true,
        "messages":[{"role":"user","content":"hello"}], "temperature":0.5,
        "tools":[{"type":"function","function":{"name":"test","parameters":{"type":"object"}}}],
        "max_tokens":1000,"max_completion_tokens":12});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .body(Body::from(body.to_string()))
        .unwrap();
    assert_eq!(
        crate::proxy::passthrough(t.app.clone(), 1, "api".into(), req)
            .await
            .status(),
        StatusCode::OK
    );
    let calls = server.calls.lock().unwrap();
    let sent = &calls[0].2;
    assert_eq!(sent["model"], "model-a");
    assert_eq!(sent["temperature"], 0.5);
    assert_eq!(sent["max_completion_tokens"], 12);
    assert!(sent.get("tools").is_some());
    for field in [
        "models",
        "route",
        "provider",
        "plugins",
        "cache_prompt",
        "max_tokens",
    ] {
        assert!(sent.get(field).is_none());
    }
}

#[tokio::test]
async fn failure_retry_after_includes_the_quota_spent_by_the_failed_attempt() {
    let t = TestApp::new();
    let server = MockProvider::new().await;
    server.respond(500, "unavailable");
    let mut p = provider("a", &server.server.url());
    p.requests_per_day = Some(6);
    configure(&t, vec![p], false);
    let response = route(&t).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        response.headers()[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            >= 86399
    );
    assert_eq!(server.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn path_routing_and_model_listing_use_the_same_router_without_spending_model_quota() {
    let t = TestApp::new();
    let server = MockProvider::new().await;
    configure(&t, vec![provider("a", &server.server.url())], true);
    let mut req = request();
    *req.uri_mut() = "/gpu/1/api/v1/chat/completions?local=not-forwarded"
        .parse()
        .unwrap();
    let response = crate::proxy::gpu_path(
        AppCtx(t.app.clone()),
        axum::extract::Path((1, "api".into())),
        req,
    )
    .await;
    assert_eq!(response.headers()["x-free-router-provider"], "a");
    assert_eq!(server.calls.lock().unwrap()[0].1, "/v1/chat/completions");
    let req = Request::builder()
        .uri("/gpu/1/api/v1/models")
        .body(Body::empty())
        .unwrap();
    let response = crate::proxy::gpu_path(
        AppCtx(t.app.clone()),
        axum::extract::Path((1, "api".into())),
        req,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["data"][0]["id"], "free-router");
    assert_eq!(server.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn ten_provider_jumper_distributes_250_concurrent_requests_without_spending_reserve() {
    let t = TestApp::new();
    let server = MockProvider::new().await;
    let providers = (0..10)
        .map(|i| {
            let mut p = provider(&format!("provider-{i}"), &server.server.url());
            p.requests_per_minute = 30;
            p
        })
        .collect();
    configure(&t, providers, true);
    let responses = futures::future::join_all((0..250).map(|_| route(&t))).await;
    let mut counts = std::collections::HashMap::<String, usize>::new();
    for response in responses {
        assert_eq!(response.status(), StatusCode::OK);
        *counts
            .entry(
                response.headers()["x-free-router-provider"]
                    .to_str()
                    .unwrap()
                    .into(),
            )
            .or_default() += 1;
    }
    assert_eq!(counts.len(), 10);
    assert!(counts.values().all(|n| *n == 25));
    assert_eq!(route(&t).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(server.calls.lock().unwrap().len(), 250);
}

#[tokio::test]
async fn broken_ledger_never_sends_requests() {
    let t = TestApp::new();
    let server = MockProvider::new().await;
    configure(&t, vec![provider("a", &server.server.url())], false);
    rusqlite::Connection::open(t.dir.join("test.sqlite"))
        .unwrap()
        .execute("DROP TABLE free_router_requests", [])
        .unwrap();
    let response = route(&t).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers()["x-router-state"],
        "free_router_ledger_unavailable"
    );
    assert!(server.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn settings_save_reorders_preserves_keys_and_rejects_stale_or_cross_origin_edits() {
    let t = TestApp::new();
    let raw = include_str!("../../../../config.example.toml")
        .replace(
            "token = \"\"",
            "token = \"test-token-not-a-real-secret-12345678\"",
        )
        .replace(
            "api_key_env = \"GROQ_API_KEY\"",
            "api_key = \"KEEP-PRIVATE\"",
        );
    std::fs::write(&t.app.config_path, &raw).unwrap();
    t.app
        .cfg_swap(crate::config::Config::load_str(&raw).unwrap());
    let response = ui::get(AppCtx(t.app.clone()), t.request("")).await;
    let mut data: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    data["config"]["jumper"] = true.into();
    data["config"]["use_when_all_offline"] = true.into();
    data["config"]["providers"]
        .as_array_mut()
        .unwrap()
        .swap(0, 1);
    let input =
        serde_json::json!({"config":data["config"],"revision":data["revision"],"clear_keys":[]})
            .to_string();
    let denied = Request::builder()
        .header(
            "cookie",
            format!("pgpu_session={}", t.app.cfg().router_token()),
        )
        .header("host", "router.example")
        .header("origin", "https://foreign.example")
        .body(Body::from(input.clone()))
        .unwrap();
    assert_eq!(
        ui::save(AppCtx(t.app.clone()), denied).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        ui::save(AppCtx(t.app.clone()), t.request(&input))
            .await
            .status(),
        StatusCode::OK
    );
    assert!(t.app.cfg().free_router.jumper);
    assert_eq!(t.app.cfg().free_router.safety_buffer_requests, 5);
    assert_eq!(t.app.cfg().free_router.providers[1].api_key, "KEEP-PRIVATE");
    assert_eq!(t.app.cfg().free_router.providers[0].id, "openrouter");
    assert_eq!(
        ui::save(AppCtx(t.app.clone()), t.request(&input))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let saved = std::fs::read_to_string(&t.app.config_path).unwrap();
    assert!(saved.contains("# Router-Token:"));
    assert!(
        crate::config::Config::load_str(&saved)
            .unwrap()
            .free_router
            .jumper
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&t.app.config_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(t.app.db.instances(true).is_empty());
}

#[test]
fn catalog_covers_directory_and_produces_disabled_valid_presets() {
    let catalog: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("catalog.json")).unwrap();
    assert_eq!(catalog.len(), 40);
    let mut ids = std::collections::HashSet::new();
    for entry in catalog {
        let id = entry["id"].as_str().unwrap();
        assert!(ids.insert(id.to_string()));
        if entry["tier"] == "adapter" {
            continue;
        }
        let p = Provider {
            id: id.into(),
            base_url: entry["base_url"].as_str().unwrap().into(),
            requests_per_minute: entry["rpm"].as_u64().unwrap() as u32,
            ..Default::default()
        };
        Config {
            providers: vec![p],
            ..Default::default()
        }
        .validate()
        .unwrap();
    }
}
