use super::*;
use crate::test_support::TestApp;

#[tokio::test]
async fn lock_transitions_are_informative_durable_and_do_not_repeat() {
    let t=TestApp::new();let now=chrono::Utc::now().timestamp();
    t.app.db.initialize_notifications(now).unwrap();
    crate::operations::set_slot_lock(&t.app,1,true,"test").await.unwrap();
    let events=t.app.db.claim_notification_events().unwrap();assert_eq!(events.len(),1);
    let (kind,message)=event_message(&t.app,&events[0]).unwrap();
    assert_eq!(kind,"slot_lock_changed");assert!(message.contains("LOCKED"));assert!(message.contains("Laufende Kosten"));assert!(message.contains("Budget-Drain"));
    crate::operations::set_slot_lock(&t.app,1,true,"same state").await.unwrap();
    assert!(t.app.db.claim_notification_events().unwrap().is_empty());
    crate::operations::set_slot_lock(&t.app,1,false,"test").await.unwrap();
    let events=t.app.db.claim_notification_events().unwrap();assert_eq!(events.len(),1);
    assert!(event_message(&t.app,&events[0]).unwrap().1.contains("UNLOCKED"));
    let db=crate::db::Db::open(&t.dir.join("test.sqlite")).unwrap();db.initialize_notifications(now).unwrap();assert!(db.claim_notification_events().unwrap().is_empty());
    assert!(t.app.db.instances(true).is_empty());
}

#[test]
fn capacity_and_start_errors_explain_disk_retention_without_claiming_readiness() {
    assert!(state_hint("scheduling").contains("Wartet auf GPU-Kapazität"));
    assert!(state_hint("scheduling").contains("kein Ersatz/Destroy"));
    assert!(state_hint("start_requested").contains("Keine Bereitschaftszusage"));
    assert!(state_hint("start_failed").contains("Startfehler"));
    assert!(state_hint("start_failed").contains("kein blindes Neumieten"));
}
