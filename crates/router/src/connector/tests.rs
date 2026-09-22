use super::*;
use crate::test_support::{MockProvider, TestApp};
use serde_json::json;

/// Loopback-only agent; never touches NetBird, Vast or an actual GPU.
struct Agent {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Agent {
    async fn new(token: String, close: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            ws.send(Message::Text(json!({"type":"hello", "token":token,
                "role":"llm", "agent_version":"offline-test"}).to_string())).await.unwrap();
            ws.send(Message::Text(json!({"type":"heartbeat", "health":"healthy"}).to_string())).await.unwrap();
            // Wait until the router processed Hello and started its asset handshake.
            if let Some(Ok(Message::Text(_))) = ws.next().await {
                if close {
                    let _ = ws.close(None).await;
                } else {
                    std::future::pending::<()>().await;
                }
            }
        });
        Self { url, task }
    }
}

impl Drop for Agent {
    fn drop(&mut self) { self.task.abort(); }
}

#[tokio::test]
async fn closed_agent_is_removed_and_same_contract_can_reconnect() {
    let t = TestApp::new();
    t.insert(11, &crate::db::now_iso());
    let token = t.app.db.instance(11).unwrap().node_token;
    for _ in 0..2 {
        let agent = Agent::new(token.clone(), true).await;
        let result = tokio::time::timeout(Duration::from_secs(3), dial_session(&t.app, &agent.url, &token)).await.unwrap();
        assert!(result.is_err());
        assert!(t.app.hub.heartbeat(11).is_none(), "disconnected agent blocks future dial attempts");
        assert!(t.app.hub.last_seen(11).is_some(), "retain liveness history after a disconnect");
    }
}

#[tokio::test]
async fn stop_start_invalidates_old_socket_and_accepts_fresh_agent() {
    let t = TestApp::new();
    let provider = MockProvider::new().await;
    provider.attach(&t.app);
    t.insert(11, &crate::db::now_iso());
    let token = t.app.db.instance(11).unwrap().node_token;
    for attempt in 0..2 {
        let agent = Agent::new(token.clone(), false).await;
        let app = t.app.clone();
        let url = agent.url.clone();
        let token = token.clone();
        let task = tokio::spawn(async move { dial_session(&app, &url, &token).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !t.app.hub.heartbeat(11).is_some_and(|hb| hb.health_json == json!("healthy")) {
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        assert!(t.app.db.instance(11).unwrap().healthy);
        if attempt == 0 {
            crate::operations::stop_instance(&t.app, 11, "offline test stop").await.unwrap();
            // Simulate a half-open pre-stop socket: the mock agent stays alive.
            crate::operations::start_instance(&t.app, 11, "offline test restart").await.unwrap();
            assert!(tokio::time::timeout(Duration::from_secs(3), task).await.unwrap().unwrap().is_err());
            assert!(t.app.hub.heartbeat(11).is_none());
            assert!(t.app.hub.last_seen(11).is_none());
            assert_eq!(t.app.db.instance(11).unwrap().state, "start_requested");
            // Provider confirms allocation; the next loop establishes a fresh session.
            t.app.db.set_instance_state(11, "booting").unwrap();
        } else {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
    }
}

#[tokio::test]
async fn cancelled_dial_session_also_releases_its_registration() {
    let t = TestApp::new();
    t.insert(11, &crate::db::now_iso());
    let token = t.app.db.instance(11).unwrap().node_token;
    let agent = Agent::new(token.clone(), false).await;
    let app = t.app.clone();
    let url = agent.url.clone();
    let task = tokio::spawn(async move { dial_session(&app, &url, &token).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while t.app.hub.heartbeat(11).is_none() { tokio::task::yield_now().await; }
    }).await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(t.app.hub.heartbeat(11).is_none(), "aborted WebSocket tasks must not leave phantom agents");
}
