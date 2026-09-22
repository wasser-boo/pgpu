use super::*;
use std::time::Duration;

#[tokio::test]
async fn session_drop_cancels_pending_commands_and_terminals_but_keeps_liveness_history() {
    let hub = Hub::default();
    let (session, mut rx) = hub.register_session(11, 1, None, Default::default());
    let (term_tx, mut term_rx) = mpsc::channel(1);
    hub.register_term(11, 7, term_tx);
    let other = hub.clone();
    let pending = tokio::spawn(async move {
        other.command(11, |id| RouterCommand::Cmd { id, command: praxis_common::node::Command::Drain }).await
    });
    tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.unwrap().unwrap();
    drop(session);
    assert!(tokio::time::timeout(Duration::from_secs(3), pending).await.unwrap().unwrap().is_err());
    assert!(term_rx.recv().await.is_none());
    assert!(hub.heartbeat(11).is_none());
    assert!(hub.last_seen(11).is_some());
    assert!(hub.try_dial(11).is_some());
}

#[tokio::test]
async fn old_session_cleanup_cannot_evict_replacement_or_cancel_its_commands() {
    let hub = Hub::default();
    let (old, mut old_rx) = hub.register_session(11, 1, None, Default::default());
    let (new, mut new_rx) = hub.register_session(11, 1, None, Default::default());
    assert!(!old.is_current());
    assert!(old_rx.recv().await.is_none());
    let other = hub.clone();
    let pending = tokio::spawn(async move {
        other.command(11, |id| RouterCommand::Cmd { id, command: praxis_common::node::Command::Drain }).await
    });
    let RouterCommand::Cmd { id, .. } = tokio::time::timeout(Duration::from_secs(3), new_rx.recv()).await.unwrap().unwrap() else { panic!("expected command"); };
    drop(old);
    assert!(new.is_current());
    assert!(hub.heartbeat(11).is_some());
    hub.resolve(11, id, Ok(serde_json::json!({"ok":true})));
    assert!(pending.await.unwrap().is_ok());
    drop(new);
    assert!(hub.heartbeat(11).is_none());
}

#[tokio::test]
async fn new_boot_invalidates_socket_as_well_as_cached_health() {
    let hub = Hub::default();
    let (old, mut rx) = hub.register_session(11, 1, None, Default::default());
    hub.clear_boot_health(11);
    assert!(!old.is_current());
    assert!(hub.last_seen(11).is_none());
    assert!(hub.heartbeat(11).is_none());
    assert!(rx.recv().await.is_none());
    let (new, _rx) = hub.register_session(11, 1, None, Default::default());
    drop(old);
    assert!(new.is_current());
}

#[tokio::test]
async fn one_dial_per_instance_and_cancelled_attempt_can_be_retried() {
    let hub = Hub::default();
    let attempt = hub.try_dial(11).unwrap();
    assert!(hub.try_dial(11).is_none());
    assert!(hub.try_dial(22).is_some());
    let task = tokio::spawn(async move {
        let _attempt = attempt;
        std::future::pending::<()>().await;
    });
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(hub.try_dial(11).is_some());
    let (_session, _rx) = hub.register_session(11, 1, None, Default::default());
    assert!(hub.try_dial(11).is_none());
}
