use super::*;
use crate::test_support::{LocalServer, MockProvider, TestApp};
use axum::{body::Body, extract::Request, response::Response};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

type Calls = Arc<Mutex<Vec<(String, String)>>>;
struct NetbirdMock {
    server: LocalServer,
    calls: Calls,
    delete_status: Arc<Mutex<u16>>,
}
impl NetbirdMock {
    async fn new(peers: Vec<Value>, changed: Vec<Value>) -> Self {
        let calls: Calls = Default::default();
        let delete_status = Arc::new(Mutex::new(204));
        let (c, status) = (calls.clone(), delete_status.clone());
        let server = LocalServer::new(axum::Router::new().fallback(move |req: Request| {
            let (calls, status, peers, changed) =
                (c.clone(), status.clone(), peers.clone(), changed.clone());
            async move {
                let method = req.method().to_string();
                let path = req.uri().path().to_string();
                calls.lock().unwrap().push((method.clone(), path.clone()));
                let (status, body) = if method == "DELETE" {
                    (*status.lock().unwrap(), String::new())
                } else if path == "/api/peers" {
                    (200, serde_json::to_string(&peers).unwrap())
                } else {
                    let id = path.rsplit('/').next().unwrap();
                    match changed.iter().chain(peers.iter()).find(|p| p["id"] == id) {
                        Some(peer) => (200, peer.to_string()),
                        None => (404, "{}".into()),
                    }
                };
                Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap()
            }
        }))
        .await;
        Self {
            server,
            calls,
            delete_status,
        }
    }
    fn attach(&self, t: &TestApp) {
        let mut cfg = (*t.app.cfg()).clone();
        cfg.netbird.api_url = self.server.url();
        cfg.netbird.api_token = "fake-token".into();
        t.app.cfg_swap(cfg);
    }
    fn deleted(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| method == "DELETE")
            .map(|(_, path)| path.clone())
            .collect()
    }
}
fn p(id: &str, instance: i64, connected: bool) -> Value {
    json!({"id":id,"name":format!("gpu-llm-{instance:08x}"),"connected":connected})
}
fn retire(t: &TestApp, id: i64) {
    t.insert(id, "2026-01-01T00:00:00Z");
    t.app.db.mark_destroyed(id).unwrap();
}
fn later() -> DateTime<Utc> {
    Utc::now() + chrono::Duration::seconds(GRACE_S + 1)
}

#[tokio::test]
async fn cleanup_protects_sleeping_live_unknown_reconnected_and_colliding_peers() {
    let t = TestApp::new();
    for id in [11, 14, 15, 17, 18, 19] {
        retire(&t, id);
    }
    t.insert(12, "2026-01-01T00:00:00Z");
    t.app.db.complete_stop(12).unwrap();
    t.insert(13, "2026-01-01T00:00:00Z");
    t.app.db.update_instance_pinned(13, true).unwrap();
    let n = NetbirdMock::new(vec![
        p("old1",11,false), p("old2",11,false), p("sleep",12,false), p("pinned",13,false),
        p("connected",14,true), p("reconnected",15,false), p("collision",17,false), p("renamed",18,false),
        p("still-in-vast",19,false), json!({"id":"laptop","name":"my-laptop","connected":false}),
        json!({"id":"ambiguous","dns_label":"gpu-llm-0000000b-other.example","connected":false}),
        json!({"id":"unknown-status","name":"gpu-llm-0000000b"}),
    ], vec![p("reconnected",15,true),json!({"id":"renamed","name":"adopted-other-device","connected":false})]).await;
    n.attach(&t);
    let inventory = vec![
        praxis_vast::Instance {
            id: 12,
            actual_status: Some("stopped".into()),
            ..Default::default()
        },
        praxis_vast::Instance {
            id: 19,
            actual_status: Some("stopped".into()),
            ..Default::default()
        },
        praxis_vast::Instance {
            id: 999,
            label: Some("praxis-llm-s1-00000011".into()),
            ..Default::default()
        },
    ];
    assert_eq!(cleanup(&t.app, &inventory, later()).await.unwrap(), 2);
    assert_eq!(n.deleted(), ["/api/peers/old1", "/api/peers/old2"]);
    assert_eq!(t.app.db.instance(12).unwrap().state, "stopped");
    assert!(t.app.db.instance(13).unwrap().pinned);
}

#[tokio::test]
async fn delete_failures_retry_and_404_is_idempotent() {
    let t = TestApp::new();
    retire(&t, 11);
    let n = NetbirdMock::new(vec![p("old", 11, false)], vec![]).await;
    n.attach(&t);
    *n.delete_status.lock().unwrap() = 503;
    assert!(cleanup(&t.app, &[], later()).await.is_err());
    assert!(t
        .app
        .db
        .last_event_of_kind("netbird_peer_deleted")
        .is_none());
    *n.delete_status.lock().unwrap() = 204;
    assert_eq!(cleanup(&t.app, &[], later()).await.unwrap(), 1);
    *n.delete_status.lock().unwrap() = 404;
    assert_eq!(cleanup(&t.app, &[], later()).await.unwrap(), 0);
    assert_eq!(n.deleted().len(), 3);
}

#[tokio::test]
async fn grace_period_disable_and_partial_local_inventory_fail_closed() {
    let t = TestApp::new();
    retire(&t, 11);
    let n = NetbirdMock::new(vec![p("old", 11, false)], vec![]).await;
    n.attach(&t);
    assert_eq!(cleanup(&t.app, &[], Utc::now()).await.unwrap(), 0);
    assert!(n.calls.lock().unwrap().is_empty());
    let mut cfg = (*t.app.cfg()).clone();
    cfg.netbird.cleanup_unused_peers = false;
    t.app.cfg_swap(cfg);
    assert_eq!(cleanup(&t.app, &[], later()).await.unwrap(), 0);
    assert!(n.calls.lock().unwrap().is_empty());
    let mut cfg = (*t.app.cfg()).clone();
    cfg.netbird.cleanup_unused_peers = true;
    t.app.cfg_swap(cfg);
    t.insert(12, "2026-01-01T00:00:00Z");
    let connection = rusqlite::Connection::open(t.dir.join("test.sqlite")).unwrap();
    connection
        .execute(
            "UPDATE instances SET machine_id='invalid' WHERE vast_id=12",
            [],
        )
        .unwrap();
    assert!(cleanup(&t.app, &[], later()).await.is_err());
    assert!(n.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn failed_provider_inventory_never_triggers_peer_cleanup() {
    let t = TestApp::new();
    retire(&t, 11);
    let n = NetbirdMock::new(vec![p("old", 11, false)], vec![]).await;
    n.attach(&t);
    let vast = MockProvider::new().await;
    vast.attach(&t.app);
    vast.respond(503, "{}");
    assert!(crate::reconciler::tick(&t.app).await.is_err());
    assert!(n.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn failed_or_malformed_peer_listing_never_means_empty_success() {
    let t = TestApp::new();
    retire(&t, 11);
    let server = MockProvider::new().await;
    let mut cfg = (*t.app.cfg()).clone();
    cfg.netbird.api_url = server.server.url();
    cfg.netbird.api_token = "fake".into();
    t.app.cfg_swap(cfg);
    for (status, body) in [
        (503, "[]"),
        (200, r#"{"message":"not an inventory"}"#),
        (200, "not JSON"),
    ] {
        server.respond(status, body);
        assert!(cleanup(&t.app, &[], later()).await.is_err());
    }
    assert!(server
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(|(method, _, _)| method == "GET"));
}
