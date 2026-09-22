use super::*;
use crate::test_support::{LocalServer, MockProvider, TestApp};
use serde_json::{json, Value};
fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-8, "{a} != {b}");
}

#[tokio::test]
async fn search_prices_are_mode_specific_for_bid_only_overlapping_and_on_demand_only_offers() {
    let t = TestApp::new();
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let server=LocalServer::new(axum::Router::new().fallback(move |axum::Json(query):axum::Json<Value>| {
        recorded.lock().unwrap().push(query.clone());
        async move {
            let make=|id,min_bid,base,total|json!({"id":id,"machine_id":id,"gpu_name":"RTX 3090","gpu_ram":24576,"cpu_ram":65536,"num_gpus":1,"disk_space":120,"inet_down":1000,"reliability2":0.99,"geolocation":"DE","min_bid":min_bid,"dph_base":base,"dph_total":total,"storage_total_cost":0.01,"storage_cost":0.12});
            axum::Json(if query["type"]=="bid" {json!({"offers":[make(1,0.14,Some(0.14),0.15),make(2,0.17,Some(0.17),0.18)]})}
                else {json!({"offers":[make(2,0.17,Some(0.46),0.47),make(3,0.14,None,0.21)]})})
        }
    })).await;
    *t.app.vast.lock().unwrap() =
        Some(praxis_vast::Vast::with_api_root("fake-key", &server.url()).unwrap());
    let mut cfg = (*t.app.cfg()).clone();
    cfg.slots[0].bid.ceiling_usd_h = 0.20;
    cfg.slots[0].disk_gb = 60;
    t.app.cfg_swap(cfg);
    let offers = search_slot_offers(&t.app, 1, true).await.unwrap();
    let by_id = |id| offers.iter().find(|o| o.id == id).unwrap();
    close(by_id(1).dph_total, 0.0);
    close(by_id(1).min_bid, 0.14);
    close(by_id(2).dph_total, 0.46);
    close(by_id(2).min_bid, 0.17);
    close(by_id(3).dph_total, 0.20);
    close(by_id(3).min_bid, 0.0);
    let cfg = t.app.cfg();
    let slot = cfg.slot(1).unwrap();
    assert!(!crate::catalog::assess(&t.app, slot, by_id(1), Mode::OnDemand, 0.14, 60).eligible);
    assert!(!crate::catalog::assess(&t.app, slot, by_id(2), Mode::OnDemand, 0.17, 60).eligible);
    assert!(crate::catalog::assess(&t.app, slot, by_id(3), Mode::OnDemand, 0.20, 60).eligible);
    assert!(create_instance(
        &t.app,
        1,
        by_id(1),
        Mode::OnDemand,
        0.14,
        60,
        Default::default(),
        false,
        "test"
    )
    .await
    .is_err());
    assert_eq!(
        calls.lock().unwrap().len(),
        2,
        "no provider mutation for missing on-demand quote"
    );
    assert_eq!(calls.lock().unwrap()[0]["type"], "bid");
    assert_eq!(calls.lock().unwrap()[1]["type"], "on-demand");
    assert_eq!(calls.lock().unwrap()[1]["allocated_storage"], 60.0);
    let cached = search_slot_offers(&t.app, 1, false).await.unwrap();
    assert_eq!(cached.len(), 3);
    assert_eq!(calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn sync_uses_actual_compute_and_storage_but_preserves_prior_spending() {
    let t = TestApp::new();
    t.insert(11, &crate::db::now_iso());
    let conn = rusqlite::Connection::open(t.dir.join("test.sqlite")).unwrap();
    conn.execute("UPDATE instances SET mode='on_demand',bid_usd_h=0.144444,dph_total=0.468444,storage_usd_h=0.02 WHERE vast_id=11",[]).unwrap();
    let day = crate::node::local_date(&t.app);
    conn.execute(
        "INSERT INTO budget_days(date,metered_usd) VALUES(?1,1.234)",
        rusqlite::params![day],
    )
    .unwrap();
    let v:praxis_vast::Instance=serde_json::from_value(json!({"id":11,"actual_status":"running","intended_status":"running","dph_base":0.4573333333,"dph_total":0.4684444444,"storage_total_cost":0.0111111111})).unwrap();
    sync_vast(&t.app, &[v],chrono::Utc::now()).await.unwrap();
    let row = t.app.db.instance(11).unwrap();
    close(row.compute_usd_h(), 0.4573333333);
    close(row.storage_usd_h, 0.0111111111);
    close(crate::costs::hourly([&row]).unwrap().total(), 0.4684444444);
    assert!(t.app.db.spent_today(&day) >= 1.234);
}

#[tokio::test]
async fn resume_and_bid_admission_count_existing_storage_once_and_never_omit_it() {
    let t = TestApp::new();
    let p = MockProvider::new().await;
    p.attach(&t.app);
    t.insert(11, &crate::db::now_iso());
    let mut cfg = (*t.app.cfg()).clone();
    cfg.limits.max_total_rate_usd_h = 1.100001;
    t.app.cfg_swap(cfg);
    crate::operations::check_admission(&t.app, 1, 1.0, 0.1, Some(11)).unwrap();
    let mut cfg = (*t.app.cfg()).clone();
    cfg.limits.max_total_rate_usd_h = 1.05;
    t.app.cfg_swap(cfg);
    let error = crate::operations::check_admission(&t.app, 1, 1.0, 0.1, Some(11))
        .unwrap_err()
        .to_string();
    for expected in [
        "existing compute 0.0000",
        "requested compute 1.0000",
        "storage 0.1000",
        "1.0500",
        "not an API request limit",
    ] {
        assert!(error.contains(expected), "{error}");
    }
    t.app
        .db
        .update_instance_vast(11, "stopped", 1.0, 1.0, 1, "test", 0.1)
        .unwrap();
    t.app.db.set_instance_state(11, "stopped").unwrap();
    assert!(crate::operations::start_instance(&t.app, 11, "test")
        .await
        .is_err());
    assert!(crate::operations::change_bid(&t.app, 11, 1.01, "test")
        .await
        .is_err());
    assert!(p.calls.lock().unwrap().is_empty());
}

#[test]
fn hourly_view_accounts_for_loading_and_stopped_disks_but_not_destroyed_contracts() {
    let t = TestApp::new();
    t.insert(11, &crate::db::now_iso());
    let mut row = t.app.db.instance(11).unwrap();
    row.mode = Mode::OnDemand;
    row.dph_total = 0.457;
    row.bid_usd_h = 0.14;
    row.storage_usd_h = 0.011;
    row.actual_status = "loading".into();
    row.state = "provisioning".into();
    close(crate::costs::hourly([&row]).unwrap().total(), 0.468);
    row.dph_total = 0.0;
    assert!(crate::costs::hourly([&row]).is_err());
    row.dph_total = 0.457;
    row.actual_status = "stopped".into();
    row.state = "stopped".into();
    close(crate::costs::hourly([&row]).unwrap().total(), 0.011);
    row.destroyed_at = Some(crate::db::now_iso());
    close(crate::costs::hourly([&row]).unwrap().total(), 0.0);
    row.destroyed_at = None;
    row.storage_usd_h = f64::NAN;
    assert!(crate::costs::hourly([&row]).is_err());
}

#[tokio::test]
async fn provider_loading_does_not_regress_agent_booting_on_every_poll() {
    let t = TestApp::new();
    t.insert(11, &crate::db::now_iso());
    t.app.db.set_instance_state(11, "booting").unwrap();
    t.app
        .db
        .initialize_notifications(chrono::Utc::now().timestamp())
        .unwrap();
    let v: praxis_vast::Instance = serde_json::from_value(
        json!({"id":11,"actual_status":"loading","intended_status":"running"}),
    )
    .unwrap();
    for _ in 0..3 {
        sync_vast(&t.app, std::slice::from_ref(&v),chrono::Utc::now()).await.unwrap();
    }
    assert_eq!(t.app.db.instance(11).unwrap().state, "booting");
    assert!(t.app.db.claim_notification_events().unwrap().is_empty());
}
