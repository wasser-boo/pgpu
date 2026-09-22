use crate::test_support::{LocalServer, TestApp};
use futures::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn superseded_call_home_socket_exits_without_unregistering_replacement() {
    let t = TestApp::new();
    t.insert(11, &crate::db::now_iso());
    let server = LocalServer::new(axum::Router::new()
        .route("/node", axum::routing::get(super::node_ws))
        .with_state(t.app.clone())).await;
    let url = format!("ws://{}/node", server.addr);
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    socket.send(Message::Text(serde_json::json!({
        "type":"hello", "token":t.app.db.instance(11).unwrap().node_token,
        "role":"llm", "agent_version":"offline-test"
    }).to_string())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while t.app.hub.heartbeat(11).is_none() { tokio::task::yield_now().await; }
    }).await.unwrap();
    let (replacement, _rx) = {
        let _management = t.app.management.lock().await;
        t.app.hub.register_session(11, 1, None, Default::default())
    };
    // Closing the superseded command channel must end the socket, not busy-loop.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match socket.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                _ => {}
            }
        }
    }).await.unwrap();
    assert!(replacement.is_current());
    assert!(t.app.hub.heartbeat(11).is_some());
}
