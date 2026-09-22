use super::*;
use crate::test_support::{LocalServer, TestApp};
use http_body_util::BodyExt;

#[tokio::test]
async fn streaming_lease_lasts_until_eof_and_preserves_headers() {
    let t = TestApp::new();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(2);
    let receiver = std::sync::Arc::new(std::sync::Mutex::new(Some(rx)));
    let server = LocalServer::new(axum::Router::new().fallback(move |req: Request| {
        let rx = receiver.lock().unwrap().take().unwrap();
        async move {
            assert!(!req.headers().contains_key("x-private-hop"));
            let stream = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
            Response::builder().header("content-type", "text/event-stream")
                .header("set-cookie", "a=1").header("set-cookie", "b=2")
                .header("connection", "x-upstream-hop").header("x-upstream-hop", "remove")
                .body(Body::from_stream(stream)).unwrap()
        }
    })).await;
    let req = Request::builder().header("connection", "x-private-hop").header("x-private-hop", "remove").body(Body::empty()).unwrap();
    let target = UpstreamTarget { vast_id: 11, nb_ip: "127.0.0.1".into(), port: server.addr.port() };
    t.app.targets.set(1, Some(11), Some("127.0.0.1".into()), true);
    let response = proxy_to_target(&t.app, &t.app.proxy_client, &target, Some(1), req, "/".into(), true).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get_all("set-cookie").iter().count(), 2);
    assert!(!response.headers().contains_key("x-upstream-hop"));
    assert_eq!(t.app.traffic.snapshot(1).in_flight, 1);
    let mut body = response.into_body();
    tx.send(Ok(bytes::Bytes::from_static(b"data: hello\n\n"))).await.unwrap();
    assert!(body.frame().await.unwrap().unwrap().is_data());
    assert_eq!(t.app.traffic.snapshot(1).in_flight, 1);
    drop(tx);
    assert!(body.frame().await.is_none());
    assert_eq!(t.app.traffic.snapshot(1).in_flight, 0);
}

#[tokio::test]
async fn lease_releases_on_body_error_and_client_disconnect() {
    let traffic = crate::state::Traffic::default();
    let stream = futures::stream::once(async { Err::<bytes::Bytes, _>(std::io::Error::other("broken upstream")) });
    let mut body = tracked_body(Body::from_stream(stream), Some(traffic.begin(1)));
    assert!(body.frame().await.unwrap().is_err());
    assert_eq!(traffic.snapshot(1).in_flight, 0);
    let body = tracked_body(Body::from_stream(futures::stream::pending::<Result<bytes::Bytes, std::io::Error>>()), Some(traffic.begin(1)));
    assert_eq!(traffic.snapshot(1).in_flight, 1);
    drop(body);
    assert_eq!(traffic.snapshot(1).in_flight, 0);
}

#[tokio::test]
async fn stt_websocket_preserves_text_and_binary_and_releases_session() {
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message as TMsg;
    let t = TestApp::new();
    let upstream = LocalServer::new(axum::Router::new().route("/", axum::routing::get(|ws: axum::extract::WebSocketUpgrade| async {
        ws.on_upgrade(|mut socket| async move {
            while let Some(Ok(msg)) = socket.next().await {
                if matches!(msg, Message::Close(_)) { break; }
                if socket.send(msg).await.is_err() { break; }
            }
        })
    }))).await;
    let mut cfg = (*t.app.cfg()).clone();
    cfg.stt.url = upstream.url(); // http config must become ws for the handshake
    t.app.cfg_swap(cfg);
    let app = t.app.clone();
    let proxy = LocalServer::new(axum::Router::new().fallback(move |req: Request| {
        let app = app.clone();
        async move { passthrough(app, 0, "stt".into(), req).await }
    })).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{}/", proxy.addr)).await.unwrap();
    for message in [TMsg::Text("{\"config\":{\"sample_rate\":16000}}".into()), TMsg::Binary(vec![1,2,3])] {
        socket.send(message.clone()).await.unwrap();
        let echoed = tokio::time::timeout(Duration::from_secs(2), socket.next()).await.unwrap().unwrap().unwrap();
        assert_eq!(echoed, message);
    }
    assert_eq!(t.app.stt_sessions.load(std::sync::atomic::Ordering::Relaxed), 1);
    socket.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while t.app.stt_sessions.load(std::sync::atomic::Ordering::Relaxed) != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.unwrap();
}

#[tokio::test]
async fn real_proxy_observer_preserves_body_records_metrics_and_honors_benchmark_gate() {
    let t=TestApp::new();t.insert(11,&crate::db::now_iso());
    let mut cfg=(*t.app.cfg()).clone();
    cfg.slots[0].performance.enabled=true;cfg.slots[0].performance.collect_usage=true;
    cfg.slots[0].performance.profile="test-model".into();t.app.cfg_swap(cfg);
    let calls=std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted=calls.clone();
    const PAYLOAD:&str=r#"{"model":"test","choices":[{"message":{"content":"PRIVATE TEXT"}}],"usage":{"completion_tokens":20},"timings":{"predicted_per_second":40}}"#;
    let server=LocalServer::new(axum::Router::new().fallback(move || {
        counted.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
        async {Response::builder().header("content-type","application/json").body(Body::from(PAYLOAD)).unwrap()}
    })).await;
    let target=UpstreamTarget {vast_id:11,nb_ip:"127.0.0.1".into(),port:server.addr.port()};
    let req=||Request::builder().method("POST").body(Body::from("{}" )).unwrap();
    t.app.targets.set(1,Some(11),Some("127.0.0.1".into()),true);
    let response=proxy_to_target(&t.app,&t.app.proxy_client,&target,Some(1),req(),"/v1/chat/completions".into(),true).await;
    assert_eq!(response.into_body().collect().await.unwrap().to_bytes().as_ref(),PAYLOAD.as_bytes());
    let rows=t.app.db.performance_samples(None,None,None,10).unwrap();
    assert_eq!(rows.len(),1);assert!(rows[0].success);assert_eq!(rows[0].metrics.decode_tps,Some(40.0));
    assert!(!serde_json::to_string(&rows).unwrap().contains("PRIVATE"));
    assert_eq!(t.app.traffic.snapshot(1).in_flight,0);
    let _lease=t.app.traffic.try_benchmark(1).unwrap();
    let response=proxy_to_target(&t.app,&t.app.proxy_client,&target,Some(1),req(),"/v1/chat/completions".into(),true).await;
    assert_eq!(response.status(),StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["x-router-state"],"benchmarking");
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed),1);
}

#[tokio::test]
async fn stale_routed_target_sends_nothing_but_explicit_instance_access_still_works() {
    let t = TestApp::new();
    let p = crate::test_support::MockProvider::new().await;
    let target = UpstreamTarget { vast_id: 11, nb_ip: "127.0.0.1".into(), port: p.server.addr.port() };
    t.app.targets.set(1, Some(12), Some("127.0.0.1".into()), true);
    let req = || Request::builder().method("POST").body(Body::from("{}" )).unwrap();
    let response = proxy_to_target(&t.app, &t.app.proxy_client, &target, Some(1), req(), "/".into(), true).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["x-router-state"], "routing_changed");
    assert!(p.calls.lock().unwrap().is_empty());
    assert_eq!(t.app.traffic.snapshot(1).in_flight, 0);
    let response = proxy_to_target(&t.app, &t.app.proxy_client, &target, Some(1), req(), "/".into(), false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(p.calls.lock().unwrap().len(), 1);
}

#[test]
fn path_routing_preserves_trailing_slash_and_query() {
    let req = Request::builder().uri("/gpu/1/api/directory/?q=hello%20world").body(Body::empty()).unwrap();
    assert_eq!(strip_path_segments(req, 3).uri(), "/directory/?q=hello%20world");
}
