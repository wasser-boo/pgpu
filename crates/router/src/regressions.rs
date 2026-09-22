use crate::{db::{parse_iso, Db}, state::AppCtx, test_support::{TestApp, MockProvider}};
use axum::{extract::Path, http::StatusCode};

fn at(s: &str) -> chrono::DateTime<chrono::Utc> { parse_iso(s).unwrap() }
fn close(actual: f64, expected: f64) { assert!((actual - expected).abs() < 1e-6, "{actual} != {expected}"); }

fn flip_fixture() -> TestApp {
    let t = TestApp::new();
    for id in [11, 12] {
        t.insert(id, &crate::db::now_iso());
        t.app.db.update_instance_agent(id, Some("127.0.0.1"), true, "healthy").unwrap();
    }
    t.app.db.set_active_instance(1, Some(11)).unwrap();
    crate::reconciler::refresh_slot_routes(&t.app, 1).unwrap();
    t
}

#[tokio::test]
async fn pin_serializes_with_management_and_stale_policy_actions_do_not_reach_provider() {
    let t = flip_fixture(); let p = MockProvider::new().await; p.attach(&t.app);
    t.app.db.set_auto_rent(true).unwrap();
    let guard = t.app.management.lock().await;
    let app = t.app.clone(); let req = t.request("{}");
    let pin = tokio::spawn(async move { crate::api::instance_action(AppCtx(app), Path((11, "pin".into())), req).await });
    tokio::task::yield_now().await;
    assert!(!pin.is_finished());
    assert!(!t.app.db.instance(11).unwrap().pinned);
    drop(guard);
    assert_eq!(pin.await.unwrap().status(), StatusCode::OK);
    use praxis_common::Action;
    for action in [
        Action::Stop { instance_id: 11, reason: "planned before pin".into() },
        Action::Destroy { instance_id: 11, reason: "planned before pin".into() },
        Action::Start { instance_id: 11, reason: "planned before pin".into() },
        Action::ChangeBid { instance_id: 11, price_usd_h: 1.1, reason: "planned before pin".into() },
    ] {
        assert!(crate::reconciler::apply_action(&t.app, &action).await.is_err());
    }
    assert!(t.app.db.pending_operations().unwrap().is_empty());
    assert!(p.calls.lock().unwrap().is_empty());
    // Explicit operator action remains permitted; pin only blocks automation.
    crate::operations::stop_instance(&t.app, 11, "explicit operator stop").await.unwrap();
    assert_eq!(p.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn flip_cannot_bypass_stream_lease_and_failed_flip_cannot_retire_active() {
    let t = flip_fixture(); let p = MockProvider::new().await; p.attach(&t.app);
    // warm=1 must expose ONLY the active instance, not the warming replacement.
    assert_eq!(t.app.pool_routes.healthy(1), vec![(11, "127.0.0.1".into())]);
    let lease = t.app.traffic.begin(1);
    assert!(crate::operations::flip_slot(&t.app, 1, 11, 12, "test").await.is_err());
    let retire = praxis_common::Action::SwapOut { instance_id: 11, destroy: true, reason: "test".into() };
    assert!(crate::reconciler::apply_action(&t.app, &retire).await.is_err());
    assert_eq!(t.app.db.active_instance(1), Some(11));
    assert!(p.calls.lock().unwrap().is_empty());
    drop(lease);
    assert!(crate::operations::flip_slot(&t.app, 1, 11, 12, "grace").await.is_err());
    // Once preempted and no request is alive, the ready replacement can serve.
    t.app.db.set_instance_state(11, "preempted").unwrap();
    t.app.db.update_instance_vast(11, "stopped", 1.0, 1.0, 1, "test", 0.1).unwrap();
    crate::operations::flip_slot(&t.app, 1, 11, 12, "test").await.unwrap();
    assert_eq!(t.app.db.active_instance(1), Some(12));
    assert_eq!(t.app.targets.get(1).unwrap().0, 12);
    assert_eq!(t.app.pool_routes.healthy(1), vec![(12, "127.0.0.1".into())]);
}

#[tokio::test]
async fn flip_revalidates_pins_readiness_busy_pending_and_snapshot() {
    for case in 0..6 {
        let t = flip_fixture();
        match case {
            0 => t.app.db.update_instance_pinned(11, true).unwrap(),
            1 => t.app.db.update_instance_busy(11, true, "job").unwrap(),
            2 => t.app.db.update_instance_busy(12, true, "job").unwrap(),
            3 => t.app.db.set_instance_state(12, "booting").unwrap(),
            4 => t.app.db.queue_operation(12, "destroy", "test").unwrap(),
            _ => (),
        }
        let from = if case == 5 { 999 } else { 11 };
        assert!(crate::operations::flip_slot(&t.app, 1, from, 12, "test").await.is_err(), "case {case}");
        assert_eq!(t.app.db.active_instance(1), Some(11));
        assert!(!t.app.db.events(20, None).iter().any(|e| e.kind == "slot_flipped"));
    }
}

#[test]
fn idle_mutation_gate_excludes_streams_and_benchmarks() {
    let traffic = crate::state::Traffic::default();
    let lease = traffic.begin(1);
    assert!(traffic.when_idle(1, |_| 42).is_none());
    drop(lease);
    let lease = traffic.try_benchmark(1).unwrap();
    assert!(traffic.when_idle(1, |_| 42).is_none());
    drop(lease);
    assert_eq!(traffic.when_idle(1, |_| 42), Some(42));
}

#[tokio::test]
async fn retiring_old_backer_does_not_disable_new_active_pool() {
    let t = flip_fixture(); let p = MockProvider::new().await; p.attach(&t.app);
    t.app.db.set_slot_desired_audited(1, true, "test").unwrap();
    crate::operations::flip_slot(&t.app, 1, 11, 12, "test").await.unwrap();
    crate::operations::stop_instance(&t.app, 11, "retire old backer").await.unwrap();
    assert!(t.app.db.slot_desired(1));
    assert_eq!(t.app.db.active_instance(1), Some(12));
}

#[test]
fn current_boot_clock_is_persistent_and_does_not_reset_on_repeated_state_update() {
    let t = TestApp::new(); t.insert(11, "2000-01-01T00:00:00Z");
    t.app.db.set_instance_state(11, "stopped").unwrap();
    t.app.db.set_instance_state(11, "booting").unwrap();
    let started = t.app.db.boot_started_at(11).unwrap().unwrap();
    assert!(started > at("2000-01-01T00:00:00Z"));
    // Repeated allocated-boot updates cannot extend this attempt. Provider
    // loading regression is separately covered at the reconciliation boundary.
    t.app.db.set_instance_state(11, "booting").unwrap();
    t.app.db.set_instance_state(11, "booting").unwrap();
    let reopened = Db::open(&t.dir.join("test.sqlite")).unwrap();
    assert_eq!(reopened.boot_started_at(11).unwrap(), Some(started));
    assert_eq!(reopened.instance(11).unwrap().created_at, "2000-01-01T00:00:00Z");
}

#[test]
fn metering_is_incremental_and_survives_restart() {
    let t = TestApp::new();
    t.insert(11, "2026-01-01T00:00:00Z");
    let db = &t.app.db;
    db.meter_until(at("2026-01-01T01:00:00Z"), chrono_tz::UTC).unwrap();
    db.meter_until(at("2026-01-01T01:00:00Z"), chrono_tz::UTC).unwrap();
    close(db.spent_today("2026-01-01"), 1.1);
    let reopened = Db::open(&t.dir.join("test.sqlite")).unwrap();
    reopened.meter_until(at("2026-01-01T02:00:00Z"), chrono_tz::UTC).unwrap();
    close(db.spent_today("2026-01-01"), 2.2);
    reopened.meter_until(at("2026-01-01T00:30:00Z"), chrono_tz::UTC).unwrap();
    reopened.meter_until(at("2026-01-01T03:00:00Z"), chrono_tz::UTC).unwrap();
    close(db.spent_today("2026-01-01"), 3.3);
}

#[test]
fn metering_splits_local_midnight_and_dst() {
    let t = TestApp::new();
    // Berlin's spring-forward day has 23 hours, not 24.
    t.insert(11, "2026-03-28T23:00:00Z");
    t.app.db.meter_until(at("2026-03-29T23:00:00Z"), chrono_tz::Europe::Berlin).unwrap();
    close(t.app.db.spent_today("2026-03-29"), 23.0 * 1.1);
    close(t.app.db.spent_today("2026-03-30"), 1.1);
}

#[test]
fn storage_continues_when_stopped_and_failed_stop_still_bills_gpu() {
    let t = TestApp::new();
    t.insert(11, "2026-01-01T00:00:00Z");
    t.app.db.set_instance_state(11, "stopped").unwrap();
    // Local desired state is not proof that the provider stopped billing.
    t.app.db.meter_until(at("2026-01-01T01:00:00Z"), chrono_tz::UTC).unwrap();
    close(t.app.db.spent_today("2026-01-01"), 1.1);
    t.app.db.update_instance_vast(11, "stopped", 1.0, 1.0, 1, "test", 0.1).unwrap();
    t.app.db.meter_until(at("2026-01-01T02:00:00Z"), chrono_tz::UTC).unwrap();
    close(t.app.db.spent_today("2026-01-01"), 1.2);
    t.app.db.mark_destroyed(11).unwrap();
    t.app.db.meter_until(at("2026-01-01T03:00:00Z"), chrono_tz::UTC).unwrap();
    close(t.app.db.spent_today("2026-01-01"), 1.2);
}

#[test]
fn lifecycle_timestamps_set_preserve_and_clear() {
    let t = TestApp::new();
    t.insert(11, &crate::db::now_iso());
    t.app.db.set_instance_state(11, "stopped").unwrap();
    let stopped = t.app.db.instance(11).unwrap();
    assert!(stopped.stopped_since.is_some());
    assert!(stopped.healthy_since.is_none());
    t.app.db.set_instance_state(11, "stopped").unwrap();
    assert_eq!(t.app.db.instance(11).unwrap().stopped_since, stopped.stopped_since);
    t.app.db.set_instance_state(11, "healthy").unwrap();
    let healthy = t.app.db.instance(11).unwrap();
    assert!(healthy.stopped_since.is_none());
    assert!(healthy.healthy_since.is_some());
}

#[tokio::test]
async fn failed_destroy_is_durable_and_retry_confirms_before_local_deletion() {
    let t = TestApp::new(); let p = MockProvider::new().await; p.attach(&t.app);
    t.insert(11, &crate::db::now_iso());
    t.app.targets.set(1, Some(11), Some("127.0.0.1".into()), true);
    t.app.pool_routes.set_healthy(1, vec![(11, "127.0.0.1".into())]);
    p.respond(503, "unavailable");
    assert!(crate::operations::destroy_instance(&t.app, 11, "test").await.is_err());
    assert_eq!(t.app.db.instance(11).unwrap().state, "healthy");
    assert_eq!(t.app.db.active_instance(1), Some(11));
    let reopened = Db::open(&t.dir.join("test.sqlite")).unwrap();
    assert_eq!(reopened.pending_operations().unwrap()[0].1, "destroy");
    assert!(!t.app.db.events(20, Some(11)).iter().any(|e| e.kind == "instance_destroyed"));
    p.respond(200, r#"{"success":true}"#);
    crate::operations::retry_pending(&t.app).await.unwrap();
    assert_eq!(t.app.db.instance(11).unwrap().state, "destroyed");
    assert!(t.app.db.pending_operations().unwrap().is_empty());
    assert!(t.app.targets.get(1).is_none());
    assert!(t.app.pool_routes.healthy(1).is_empty());
    assert_eq!(p.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn failed_stop_is_not_reported_as_success_and_is_retried() {
    let t = TestApp::new(); let p = MockProvider::new().await; p.attach(&t.app);
    t.insert(11, &crate::db::now_iso());
    // Some APIs reject in a 200 response body; that is also failure.
    p.respond(200, r#"{"success":false,"msg":"rejected"}"#);
    let response = crate::api::instance_action(AppCtx(t.app.clone()), Path((11, "stop".into())), t.request("{}")).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(t.app.db.instance(11).unwrap().state, "healthy");
    assert_eq!(t.app.db.pending_operations().unwrap().len(), 1);
    p.respond(200, r#"{"success":true}"#);
    crate::operations::retry_pending(&t.app).await.unwrap();
    assert_eq!(t.app.db.instance(11).unwrap().state, "stopped");
    assert!(t.app.db.pending_operations().unwrap().is_empty());
}

#[tokio::test]
async fn delete_404_is_idempotent_success() {
    let t = TestApp::new(); let p = MockProvider::new().await; p.attach(&t.app);
    t.insert(11, &crate::db::now_iso()); p.respond(404, "already gone");
    crate::operations::destroy_instance(&t.app, 11, "test").await.unwrap();
    assert_eq!(t.app.db.instance(11).unwrap().state, "destroyed");
}

#[tokio::test]
async fn slot_start_actually_awaits_provider_and_reports_failure() {
    let t = TestApp::new(); let p = MockProvider::new().await; p.attach(&t.app);
    t.insert(11, &crate::db::now_iso());
    t.app.db.set_instance_state(11, "stopped").unwrap();
    p.respond(503, "outage");
    let r = crate::api::slot_action(AppCtx(t.app.clone()), Path((1, "start".into())), t.request("{}")).await;
    assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
    // Even a 503 can follow an accepted request: never infer disk/boot failure.
    assert_eq!(t.app.db.instance(11).unwrap().state, "start_requested");
    p.respond(200, r#"{"success":true}"#);
    let r = crate::api::slot_action(AppCtx(t.app.clone()), Path((1, "start".into())), t.request("{}")).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(t.app.db.instance(11).unwrap().state, "start_requested");
    t.app.db.set_auto_rent(false).unwrap();
    assert!(crate::operations::start_automatic_instance(&t.app, 11, "stale policy action").await.is_err());
    let calls = p.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|(method, path, body)| method == "PUT" && path == "/api/v0/instances/11/" && body["state"] == "running"));
}

#[tokio::test]
async fn pending_destroy_cannot_be_overridden_by_start_or_stop() {
    let t = TestApp::new(); let p = MockProvider::new().await; p.attach(&t.app);
    t.insert(11, &crate::db::now_iso());
    t.app.db.queue_operation(11, "destroy", "test").unwrap();
    t.app.db.queue_operation(11, "stop", "test").unwrap();
    assert_eq!(t.app.db.pending_operations().unwrap()[0].1, "destroy");
    assert!(crate::operations::start_instance(&t.app, 11, "test").await.is_err());
    assert!(p.calls.lock().unwrap().is_empty());
}

#[test]
fn meter_transaction_rolls_back_all_ledgers_and_cursor_on_failure() {
    let t = TestApp::new();
    t.insert(11, "2026-01-01T00:00:00Z");
    let conn = rusqlite::Connection::open(t.dir.join("test.sqlite")).unwrap();
    conn.execute_batch("CREATE TRIGGER simulate_disk_error BEFORE INSERT ON budget_days BEGIN SELECT RAISE(ABORT,'test failure'); END;").unwrap();
    assert!(t.app.db.meter_until(at("2026-01-01T01:00:00Z"), chrono_tz::UTC).is_err());
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM instance_meter", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 0);
    conn.execute_batch("DROP TRIGGER simulate_disk_error;").unwrap();
    t.app.db.meter_until(at("2026-01-01T01:00:00Z"), chrono_tz::UTC).unwrap();
    close(t.app.db.spent_today("2026-01-01"), 1.1);
}

#[test]
fn legacy_meter_migration_does_not_rebill_instance_lifetime() {
    let t = TestApp::new();
    t.insert(11, "2020-01-01T00:00:00Z");
    let conn = rusqlite::Connection::open(t.dir.join("test.sqlite")).unwrap();
    conn.execute_batch("ALTER TABLE instances DROP COLUMN last_metered_at;").unwrap();
    let reopened = Db::open(&t.dir.join("test.sqlite")).unwrap();
    reopened.meter_until(chrono::Utc::now(), chrono_tz::UTC).unwrap();
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    assert!(reopened.spent_today(&today) < 0.01);
}

#[test]
fn admission_enforces_live_capacity_and_cost_limits() {
    let t = TestApp::new();
    t.insert(11, &crate::db::now_iso());
    let mut cfg = (*t.app.cfg()).clone();
    cfg.limits.max_instances = 1;
    t.app.cfg_swap(cfg.clone());
    assert!(crate::operations::check_admission(&t.app, 1, 0.1, 0.0, None).is_err());
    cfg.limits.max_instances = 3;
    cfg.limits.max_total_rate_usd_h = 0.5;
    t.app.cfg_swap(cfg.clone());
    assert!(crate::operations::check_admission(&t.app, 1, 1.0, 0.0, Some(11)).is_err());
    cfg.limits.max_total_rate_usd_h = 10.0;
    cfg.budget.daily_hard_eur = 0.0;
    cfg.budget.daily_soft_eur = 0.0;
    t.app.cfg_swap(cfg);
    assert!(crate::operations::check_admission(&t.app, 1, 1.0, 0.0, Some(11)).is_err());
}

#[tokio::test]
async fn invalid_bulk_destroy_request_has_no_side_effects() {
    let t = TestApp::new(); let p = MockProvider::new().await; p.attach(&t.app);
    t.insert(11, &crate::db::now_iso());
    for body in ["{", r#"{"slots":"typo"}"#, r#"{"slot":1}"#, r#"{"slots":[999]}"#] {
        let r = crate::api::destroy_all(AppCtx(t.app.clone()), t.request(body)).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }
    assert!(p.calls.lock().unwrap().is_empty());
    assert!(t.app.db.pending_operations().unwrap().is_empty());
    assert_eq!(t.app.db.instance(11).unwrap().state, "healthy");
}

#[tokio::test]
async fn bulk_failure_returns_error_and_prevents_immediate_rerental() {
    let t = TestApp::new(); let p = MockProvider::new().await; p.attach(&t.app);
    t.insert(11, &crate::db::now_iso());
    t.app.db.set_slot_desired(1, true).unwrap();
    p.respond(503, "outage");
    let r = crate::api::destroy_all(AppCtx(t.app.clone()), t.request(r#"{"auto_rent":false}"#)).await;
    assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
    assert!(!t.app.db.auto_rent_enabled());
    assert!(!t.app.db.slot_desired(1));
    assert_eq!(t.app.db.pending_operations().unwrap().len(), 1);
}

#[test]
fn invalid_config_is_rejected_and_defaults_are_consistent() {
    let base = "[[slots]]\nid=1\nrole='llm'\nname='test'\n";
    let cfg = crate::config::Config::load_str(base).unwrap();
    assert_eq!(cfg.vast.poll_interval_s, 30);
    assert_eq!(cfg.slots[0].swap.max_warmup_s, 2700);
    assert!(cfg.slots[0].swap.on_preempt);
    for bad in [
        format!("{base}{base}"),
        format!("[router]\ntz='typo'\n{base}"),
        format!("[budget]\ndaily_soft_eur=3.0\ndaily_hard_eur=2.0\nusd_per_eur=1.0\n{base}"),
        format!("{base}[slots.bid]\nceiling_usd_h=nan\n"),
        format!("{base}[slots.pool]\nwarm=3\ntotal=1\n"),
        format!("{base}[slots.passthrough]\n'8080'='nonexistent'\n"),
    ] {
        assert!(crate::config::Config::load_str(&bad).is_err(), "accepted invalid config: {bad}");
    }
}

#[test]
fn browser_cookie_auth_rejects_cross_origin_terminal_access() {
    let t = TestApp::new();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("cookie", format!("pgpu_session={}", t.app.cfg().router_token()).parse().unwrap());
    headers.insert("host", "router:8080".parse().unwrap());
    headers.insert("origin", "http://attacker:8080".parse().unwrap());
    assert!(!crate::dashboard::session_ok(&t.app, &headers));
    headers.insert("origin", "http://router:8080".parse().unwrap());
    assert!(crate::dashboard::session_ok(&t.app, &headers));
    headers.insert("sec-fetch-site", "cross-site".parse().unwrap());
    assert!(!crate::dashboard::session_ok(&t.app, &headers));
}

#[tokio::test]
async fn terminals_are_scoped_by_agent_and_bounded() {
    let hub = crate::hub::Hub::default();
    let (tx1, mut rx1) = tokio::sync::mpsc::channel(1);
    let (tx2, mut rx2) = tokio::sync::mpsc::channel(1);
    hub.register_term(11, 1, tx1);
    hub.register_term(22, 1, tx2);
    hub.relay_term(11, serde_json::json!({"id":1,"data":"agent-11"}));
    assert_eq!(rx1.recv().await.unwrap()["data"], "agent-11");
    assert!(rx2.try_recv().is_err());
    hub.relay_term(22, serde_json::json!({"id":1,"data":"first"}));
    hub.relay_term(22, serde_json::json!({"id":1,"data":"overflow"}));
    assert_eq!(rx2.recv().await.unwrap()["data"], "first");
    assert!(rx2.recv().await.is_none());
    hub.unregister_term(11, 1);
    assert!(rx1.recv().await.is_none());
}

#[test]
fn balance_reconciliation_accumulates_deltas_not_just_last_hour() {
    let t = TestApp::new();
    close(t.app.db.record_credit("2026-01-01", 100.0).unwrap(), 0.0);
    close(t.app.db.record_credit("2026-01-01", 99.0).unwrap(), 1.0);
    close(t.app.db.record_credit("2026-01-01", 98.0).unwrap(), 2.0);
    close(t.app.db.record_credit("2026-01-01", 98.0).unwrap(), 2.0);
    close(t.app.db.record_credit("2026-01-01", 110.0).unwrap(), 2.0);
}

#[test]
fn readiness_history_survives_preemption_and_restart() {
    let t = TestApp::new();
    t.insert(11, &crate::db::now_iso());
    t.app.db.set_instance_state(11, "preempted").unwrap();
    let reopened = Db::open(&t.dir.join("test.sqlite")).unwrap();
    let row = reopened.instance(11).unwrap();
    assert!(!row.healthy);
    assert!(row.ever_healthy);
}

#[tokio::test]
async fn bids_are_validated_before_provider_and_contract_mode_cannot_be_faked() {
    let t = TestApp::new(); let p = MockProvider::new().await; p.attach(&t.app);
    t.insert(11, &crate::db::now_iso());
    for bid in [-1.0, 0.0, 100.0, f64::NAN] {
        assert!(crate::operations::change_bid(&t.app, 11, bid, "test").await.is_err());
    }
    assert!(p.calls.lock().unwrap().is_empty());
    let r = crate::api::instance_action(AppCtx(t.app.clone()), Path((11, "mode".into())), t.request(r#"{"mode":"on_demand"}"#)).await;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    assert_eq!(t.app.db.instance(11).unwrap().mode, praxis_common::Mode::Interruptible);
    crate::operations::change_bid(&t.app, 11, 1.2, "test").await.unwrap();
    close(t.app.db.instance(11).unwrap().bid_usd_h, 1.2);
}

#[test]
fn shipped_example_configs_validate() {
    crate::config::Config::load_str(include_str!("../../../config.example.toml")).unwrap();
    crate::config::Config::load_str(include_str!("../../../deploy/vps/config.example.toml")).unwrap();
}

#[tokio::test]
async fn readiness_requires_recent_success_and_rejects_shutdown() {
    use std::sync::atomic::Ordering;
    let t = TestApp::new();
    assert_eq!(crate::api::ready(AppCtx(t.app.clone())).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    t.app.last_reconcile.store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
    assert_eq!(crate::api::ready(AppCtx(t.app.clone())).await.status(), StatusCode::OK);
    t.app.last_reconcile.store(chrono::Utc::now().timestamp() - 3600, Ordering::Relaxed);
    assert_eq!(crate::api::ready(AppCtx(t.app.clone())).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    t.app.last_reconcile.store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
    t.app.shutting_down.store(true, Ordering::Relaxed);
    assert_eq!(crate::api::ready(AppCtx(t.app.clone())).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(crate::operations::check_admission(&t.app, 1, 0.1, 0.0, None).is_err());
}

#[tokio::test]
async fn vast_urls_and_inventory_fail_closed() {
    let p = MockProvider::new().await;
    let v = praxis_vast::Vast::with_api_root("test", &p.server.url()).unwrap();
    p.respond(200, r#"{"credit":10.0}"#);
    v.current_user().await.unwrap();
    assert_eq!(p.calls.lock().unwrap()[0].1, "/api/v0/users/current/");
    p.respond(200, r#"{"error":"not an inventory"}"#);
    assert!(v.instances().await.is_err());
    p.respond(200, r#"{"instances":[],"next_token":"forever"}"#);
    assert!(v.instances().await.is_err()); // never return a silently truncated list
}
