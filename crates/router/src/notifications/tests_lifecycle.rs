use super::*;
use crate::test_support::{MockProvider, TestApp};
use praxis_common::{Lifecycle, Mode};
use serde_json::json;

fn configured(t: &TestApp, p: &MockProvider) {
    let mut cfg = (*t.app.cfg()).clone();
    cfg.alerts.webhook_urls = vec![format!("{}/SECRET-WEBHOOK", p.server.url())];
    cfg.slots[0].services.insert(
        "chat".into(),
        crate::config::ServiceCfg {
            port: 8080,
            health: Some("/health".into()),
            busy: Default::default(),
        },
    );
    cfg.slots[0]
        .env
        .insert("PRIVATE_CREDENTIAL".into(), "ENV-SECRET".into());
    t.app.cfg_swap(cfg);
}
async fn wait_calls(p: &MockProvider, n: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while p.calls.lock().unwrap().len() < n {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
fn connect_healthy(t: &TestApp, id: i64) {
    let _rx = t
        .app
        .hub
        .register(id, 1, Some("100.80.0.1".into()), Default::default());
    t.app.hub.record_heartbeat(
        id,
        crate::hub::HeartbeatData {
            health_json: json!("healthy"),
            ..Default::default()
        },
        None,
    );
}
fn offer() -> praxis_policy::OfferSnapshot {
    praxis_policy::OfferSnapshot {
        id: 55,
        machine_id: 7,
        gpu_name: "RTX 3090".into(),
        gpu_ram_gb: 24.0,
        cpu_ram_gb: 64.0,
        disk_gb: 120.0,
        num_gpus: 1,
        inet_down: 1000.0,
        reliability2: 0.99,
        min_bid: 0.14,
        dph_total: 0.4573333333,
        storage_cost: 0.1333333333,
        inet_down_cost: 0.01,
        geolocation: Some("DE".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn rental_notification_requires_successful_provider_and_ownership_for_manual_and_auto() {
    for automatic in [false, true] {
        for success in [false, true] {
            let t = TestApp::new();
            let provider = MockProvider::new().await;
            let sink = MockProvider::new().await;
            provider.attach(&t.app);
            configured(&t, &sink);
            t.app.db.set_auto_rent(true).unwrap();
            provider.respond(
                if success { 200 } else { 503 },
                if success {
                    r#"{"success":true,"new_contract":111}"#
                } else {
                    "PRIVATE_PROVIDER_ERROR"
                },
            );
            let result = crate::reconciler::create_instance(
                &t.app,
                1,
                &offer(),
                Mode::OnDemand,
                0.14,
                60,
                Lifecycle::Auto,
                automatic,
                "test",
            )
            .await;
            assert_eq!(result.is_ok(), success, "{result:?}");
            if success {
                wait_calls(&sink, 1).await;
                let body = sink.calls.lock().unwrap()[0].2.clone();
                assert_eq!(body["kind"], "instance_rented");
                let message = body["message"].as_str().unwrap();
                for expected in [
                    "111",
                    "RTX 3090",
                    "24.0",
                    "64.0",
                    "DE",
                    "on_demand",
                    "0.4573",
                    "0.0111",
                    "noch",
                    "NICHT einsatzbereit",
                ] {
                    assert!(message.contains(expected), "missing {expected}: {message}");
                }
                for secret in [
                    "ENV-SECRET",
                    "SECRET-WEBHOOK",
                    &t.app.cfg().router_token(),
                    &t.app.db.instance(111).unwrap().node_token,
                ] {
                    assert!(!message.contains(secret));
                }
                assert!(
                    provider.calls.lock().unwrap()[0].2.get("price").is_none(),
                    "real on-demand must omit bid price"
                );
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                assert!(sink.calls.lock().unwrap().is_empty());
                assert!(t.app.db.instances(true).is_empty());
            }
        }
    }
}

#[tokio::test]
async fn ready_requires_health_and_publication_and_is_durable_once_per_rental() {
    let t = TestApp::new();
    let sink = MockProvider::new().await;
    configured(&t, &sink);
    t.insert(11, &crate::db::now_iso());
    let row = t.app.db.instance(11).unwrap();
    ready(&t.app, &row); // persisted healthy flag alone
    let _rx = t
        .app
        .hub
        .register(11, 1, Some("100.80.0.1".into()), Default::default());
    crate::reconciler::refresh_slot_routes(&t.app, 1).unwrap(); // connected, no healthy heartbeat
    assert!(t.app.db.setting("instance_ready_notified:11").is_none());
    connect_healthy(&t, 11);
    t.app.pool_routes.set_healthy(1, vec![]);
    ready(&t.app, &row); // cannot announce before routing
    assert!(t.app.db.setting("instance_ready_notified:11").is_none());
    crate::reconciler::refresh_slot_routes(&t.app, 1).unwrap();
    wait_calls(&sink, 1).await;
    for _ in 0..5 {
        crate::reconciler::refresh_slot_routes(&t.app, 1).unwrap();
    }
    let reopened = crate::db::Db::open(&t.dir.join("test.sqlite")).unwrap();
    assert!(!reopened.claim_once("instance_ready_notified:11").unwrap());
    let message = sink.calls.lock().unwrap()[0].2["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.contains("einsatzbereit"));
    assert!(message.contains("Router"));
    assert!(message.contains("kein Inferenz-Benchmark"));
    assert!(!backend_ready_at(
        &t.app,
        &row,
        chrono::Utc::now().timestamp() + 61
    ));
    t.app.hub.unregister(11);
    assert!(!backend_ready(&t.app, &row));
    assert_eq!(sink.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unchecked_services_pending_stop_and_nonselected_replacement_never_announce_ready() {
    let t = TestApp::new();
    let sink = MockProvider::new().await;
    configured(&t, &sink);
    t.insert(11, &crate::db::now_iso());
    t.insert(12, &crate::db::now_iso());
    t.app.db.set_active_instance(1, Some(11)).unwrap();
    connect_healthy(&t, 11);
    connect_healthy(&t, 12);
    t.app.db.queue_operation(11, "stop", "test").unwrap();
    crate::reconciler::refresh_slot_routes(&t.app, 1).unwrap();
    assert!(t.app.db.setting("instance_ready_notified:11").is_none());
    assert!(t.app.db.setting("instance_ready_notified:12").is_none());
    let mut cfg = (*t.app.cfg()).clone();
    cfg.slots[0].services.clear();
    cfg.slots[0].pool.warm = 2;
    t.app.cfg_swap(cfg);
    crate::reconciler::refresh_slot_routes(&t.app, 1).unwrap();
    assert!(t.app.db.setting("instance_ready_notified:12").is_none());
    assert!(sink.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn state_dispatch_captures_brief_transitions_no_heartbeat_spam_or_restart_replay() {
    let t = TestApp::new();
    let sink = MockProvider::new().await;
    configured(&t, &sink);
    t.insert(11, &crate::db::now_iso());
    let now = chrono::Utc::now().timestamp();
    t.app.db.initialize_notifications(now).unwrap();
    for state in ["unreachable", "booting", "healthy", "healthy"] {
        t.app.db.set_instance_state(11, state).unwrap();
    }
    lifecycle::step(&t.app, chrono::Utc::now().timestamp()).unwrap();
    wait_calls(&sink, 3).await;
    let calls = sink.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 3);
    for (_, _, body) in &calls {
        assert_eq!(body["kind"], "instance_state_changed");
        let message = body["message"].as_str().unwrap();
        for expected in [
            "Slot 1",
            "Instanz 11",
            "→",
            "USD/h",
            "Provider:",
            "Zeitpunkt:",
        ] {
            assert!(message.contains(expected));
        }
        assert!(!message.contains("ENV-SECRET"));
        assert!(!message.contains("SECRET-WEBHOOK"));
    }
    t.app.db.initialize_notifications(now + 5).unwrap();
    lifecycle::step(&t.app, now + 5).unwrap();
    let reopened = crate::db::Db::open(&t.dir.join("test.sqlite")).unwrap();
    assert!(reopened.claim_notification_events().unwrap().is_empty());
    assert_eq!(sink.calls.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn replacement_and_cold_slot_messages_have_context_without_gpu_mutations() {
    let t = TestApp::new();
    let sink = MockProvider::new().await;
    let provider = MockProvider::new().await;
    provider.attach(&t.app);
    configured(&t, &sink);
    t.insert(11, &crate::db::now_iso());
    t.app.db.set_instance_state(11, "booting").unwrap();
    t.app
        .last_reconcile
        .store(1, std::sync::atomic::Ordering::Relaxed);
    let now = chrono::Utc::now().timestamp();
    t.app.db.initialize_notifications(now).unwrap();
    lifecycle::step(&t.app, now).unwrap();
    t.insert(12, &crate::db::now_iso());
    t.app.db.mark_destroyed(11).unwrap();
    t.app.db.mark_destroyed(12).unwrap();
    lifecycle::step(&t.app, chrono::Utc::now().timestamp()).unwrap();
    wait_calls(&sink, 4).await;
    let bodies = sink
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|c| c.2.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(bodies.contains("Backend ersetzt"));
    assert!(bodies.contains("#11"));
    assert!(bodies.contains("#12"));
    assert!(bodies.contains("cold"));
    assert!(bodies.contains("Slot-Limit"));
    assert!(provider.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn four_hour_digest_persists_interval_is_informative_and_does_not_change_budgets() {
    let t = TestApp::new();
    let sink = MockProvider::new().await;
    let provider = MockProvider::new().await;
    provider.attach(&t.app);
    configured(&t, &sink);
    assert_eq!(t.app.cfg().alerts.spend_summary_interval_s, 14400);
    assert!(t.app.cfg().alerts.state_changes);
    let now = chrono::Utc::now().timestamp();
    t.app.db.initialize_notifications(now).unwrap();
    lifecycle::step(&t.app, now + 14399).unwrap();
    assert!(sink.calls.lock().unwrap().is_empty());
    lifecycle::step(&t.app, now + 14400).unwrap();
    wait_calls(&sink, 1).await;
    let body = sink.calls.lock().unwrap()[0].2.clone();
    assert_eq!(body["kind"], "spend_summary");
    let message = body["message"].as_str().unwrap();
    for expected in [
        "heute",
        "Monat",
        "Tageslimit",
        "verbleibend",
        "USD/h",
        "Vast-Charges noch nicht verfügbar",
        "Slot 1",
        "keine Limits",
    ] {
        assert!(message.contains(expected), "missing {expected}");
    }
    t.app.db.initialize_notifications(now + 14401).unwrap();
    lifecycle::step(&t.app, now + 14401).unwrap();
    assert_eq!(sink.calls.lock().unwrap().len(), 1);
    lifecycle::step(&t.app, now + 28800).unwrap();
    wait_calls(&sink, 2).await;
    assert_eq!(t.app.cfg().budget.daily_soft_eur, 1000.0);
    assert!(t.app.db.instances(true).is_empty());
    assert!(provider.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn disabled_notifications_are_not_replayed_on_enable() {
    let t = TestApp::new();
    let sink = MockProvider::new().await;
    configured(&t, &sink);
    t.insert(11, &crate::db::now_iso());
    let mut cfg = (*t.app.cfg()).clone();
    cfg.alerts.state_changes = false;
    cfg.alerts.spend_summary_interval_s = 0;
    t.app.cfg_swap(cfg);
    let now = chrono::Utc::now().timestamp();
    t.app.db.initialize_notifications(now).unwrap();
    t.app.db.set_instance_state(11, "stopped").unwrap();
    lifecycle::step(&t.app, now + 1).unwrap();
    lifecycle::step(&t.app, now + 20000).unwrap();
    assert!(sink.calls.lock().unwrap().is_empty());
    let mut cfg = (*t.app.cfg()).clone();
    cfg.alerts.state_changes = true;
    t.app.cfg_swap(cfg);
    lifecycle::step(&t.app, now + 20001).unwrap();
    assert!(sink.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn delivery_rechecks_category_preferences_without_disabling_budget_alerts() {
    let t = TestApp::new();
    let sink = MockProvider::new().await;
    configured(&t, &sink);
    let mut cfg = (*t.app.cfg()).clone();
    cfg.alerts.state_changes = false;
    cfg.alerts.spend_summary_interval_s = 0;
    t.app.cfg_swap(cfg);
    alert(&t.app, "instance_state_changed", "disabled");
    alert(&t.app, "spend_summary", "disabled");
    alert(&t.app, "budget_hard", "still enabled");
    wait_calls(&sink, 1).await;
    assert_eq!(sink.calls.lock().unwrap().len(), 1);
    assert_eq!(sink.calls.lock().unwrap()[0].2["kind"], "budget_hard");
}

#[test]
fn digest_configuration_defaults_and_interval_bounds() {
    let slots = "\n[[slots]]\nid=1\nrole='llm'\nname='test'\n";
    for alerts in ["", "[alerts]\n"] {
        let cfg = crate::config::Config::load_str(&format!("{alerts}{slots}")).unwrap();
        assert_eq!(cfg.alerts.spend_summary_interval_s, 14400);
        assert!(cfg.alerts.state_changes);
    }
    for interval in [0, 3600, 14400, 604800] {
        assert!(crate::config::Config::load_str(&format!(
            "[alerts]\nspend_summary_interval_s={interval}{slots}"
        ))
        .is_ok());
    }
    for interval in [1, 3599, 604801, u64::MAX] {
        assert!(crate::config::Config::load_str(&format!(
            "[alerts]\nspend_summary_interval_s={interval}{slots}"
        ))
        .is_err());
    }
}
