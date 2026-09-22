use super::*;
use crate::test_support::{LocalServer, MockProvider, TestApp};
use axum::{extract::Request, Json};
use serde_json::{json, Value};

fn charge(id: i64, amount: f64, label: &str) -> praxis_vast::Charge {
    serde_json::from_value(json!({"type":"instance","source":format!("instance-{id}"),"amount":amount,"metadata":{"label":label}})).unwrap()
}
fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 0.000001, "{a} != {b}");
}

#[test]
fn scope_uses_exact_slot_labels_including_sleeping_and_deleted_not_account_payments() {
    let t = TestApp::new();
    let mut known = HashMap::new();
    let mut current = HashMap::new();
    for (id, state) in [(11, "running"), (12, "stopped"), (13, "destroyed")] {
        t.insert(id, "2026-01-01T00:00:00Z");
        let mut row = t.app.db.instance(id).unwrap();
        row.label = format!("praxis-llm-s1-{id:08x}");
        row.state = state.into();
        if state != "destroyed" {
            current.insert(
                id,
                praxis_vast::Instance {
                    id,
                    label: Some(row.label.clone()),
                    actual_status: Some(state.into()),
                    ..Default::default()
                },
            );
        }
        known.insert(id, row);
    }
    let mut charges = vec![
        charge(11, 1.0, "praxis-llm-s1-0000000b"),
        charge(12, 2.0, "praxis-llm-s1-0000000c"),
        charge(13, 3.0, "praxis-llm-s1-0000000d"),
        charge(14, 100.0, "praxis-llm-s10-0000000e"),
        charge(15, 100.0, "praxis-media-s1-0000000f"),
    ];
    let mut deposit = charge(16, 1000.0, "praxis-llm-s1-00000010");
    deposit.kind = "payment".into();
    charges.push(deposit);
    let mut refund = charge(17, -1000.0, "praxis-llm-s1-00000011");
    refund.kind = "refund".into();
    charges.push(refund);
    let estimates = HashMap::from([(11, 0.4), (12, 0.5), (13, 0.6)]);
    let result = compare(
        &t.app.cfg(),
        "2026-01".into(),
        0,
        100,
        &charges,
        &current,
        &known,
        &estimates,
    )
    .unwrap();
    close(result.provider_usd, 6.0);
    close(result.estimated_usd, 1.5);
    assert_eq!(result.rows.len(), 3);
    assert_eq!(result.ignored_contracts, 4);
    assert!(result
        .rows
        .iter()
        .any(|r| r.state == "stopped" && r.provider_usd == Some(2.0)));
    assert!(result
        .rows
        .iter()
        .any(|r| r.state == "destroyed" && r.provider_usd == Some(3.0)));
    // Explicitly retagged contracts no longer match historical metadata.
    current.get_mut(&11).unwrap().label = Some("unrelated".into());
    close(
        compare(
            &t.app.cfg(),
            "2026-01".into(),
            0,
            100,
            &charges,
            &current,
            &known,
            &estimates,
        )
        .unwrap()
        .provider_usd,
        5.0,
    );
}

#[test]
fn tagged_contract_without_local_history_has_unknown_estimate_not_a_fake_zero() {
    let t = TestApp::new();
    let result = compare(
        &t.app.cfg(),
        "2026-01".into(),
        0,
        100,
        &[charge(77, 1.0, "praxis-llm-s1-deadbeef")],
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    )
    .unwrap();
    assert_eq!(result.rows[0].estimated_usd, None);
    close(result.provider_usd, 1.0);
}

#[test]
fn actual_usage_floors_are_idempotent_durable_and_carry_unbilled_usage_without_changing_caps() {
    let t = TestApp::new();
    t.insert(11, "2026-01-01T00:00:00Z");
    let start = "2026-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
    t.app
        .db
        .meter_until(start + chrono::Duration::hours(1), chrono_tz::UTC)
        .unwrap();
    close(t.app.db.spent_today("2026-01-01"), 1.1);
    let observed = vec![
        ("2026-01-01".into(), 11, 2.0, 1.1),
        ("2026-01".into(), 11, 2.0, 1.1),
    ];
    t.app.db.record_provider_usage("{}", &observed).unwrap();
    t.app.db.record_provider_usage("{}", &observed).unwrap();
    close(t.app.db.spent_today("2026-01-01"), 2.0);
    close(t.app.db.spent_month("2026-01"), 2.0);
    t.app
        .db
        .meter_until(start + chrono::Duration::hours(2), chrono_tz::UTC)
        .unwrap();
    close(t.app.db.spent_today("2026-01-01"), 3.1);
    // Lagging provider data cannot erase already accrued post-snapshot usage.
    t.app
        .db
        .record_provider_usage("{}", &[("2026-01-01".into(), 11, 2.0, 2.2)])
        .unwrap();
    close(t.app.db.spent_today("2026-01-01"), 3.1);
    t.app.db.mark_destroyed(11).unwrap();
    let reopened = crate::db::Db::open(&t.dir.join("test.sqlite")).unwrap();
    close(reopened.spent_today("2026-01-01"), 3.1);
    assert_eq!(t.app.cfg().budget.daily_hard_eur, 2000.0);
    t.app.db.record_credit("2026-01-01", 1000.0).unwrap();
    t.app.db.record_credit("2026-01-01", 1.0).unwrap();
    close(t.app.db.spent_today("2026-01-01"), 3.1); // Balance movement is not usage.
}

#[test]
fn legacy_unattributed_spending_and_bandwidth_are_not_double_counted() {
    let t = TestApp::new();
    t.app
        .db
        .meter_instance_traffic("2026-01-01", 11, 1.0)
        .unwrap();
    t.app
        .db
        .record_provider_usage("{}", &[("2026-01-01".into(), 11, 1.0, 1.0)])
        .unwrap();
    close(t.app.db.spent_today("2026-01-01"), 1.0);
    let connection = rusqlite::Connection::open(t.dir.join("test.sqlite")).unwrap();
    connection
        .execute(
            "UPDATE budget_days SET metered_usd=5.0 WHERE date='2026-01-01'",
            [],
        )
        .unwrap();
    t.app
        .db
        .record_provider_usage("{}", &[("2026-01-01".into(), 11, 6.0, 1.0)])
        .unwrap();
    close(t.app.db.spent_today("2026-01-01"), 6.0); // Not 11: the old $5 was already paid.
    t.app
        .db
        .meter_instance_traffic("2026-01-01", 11, 1.0)
        .unwrap();
    close(t.app.db.spent_today("2026-01-01"), 7.0);
}

#[test]
fn period_boundaries_use_router_timezone_and_dst() {
    let first = midnight(
        chrono::NaiveDate::from_ymd_opt(2026, 3, 29).unwrap(),
        chrono_tz::Europe::Berlin,
    )
    .unwrap();
    let next = midnight(
        chrono::NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
        chrono_tz::Europe::Berlin,
    )
    .unwrap();
    assert_eq!((next - first).num_hours(), 23);
}

#[tokio::test]
async fn complete_charge_pagination_uses_usage_endpoint_and_server_filters() {
    let server=LocalServer::new(axum::Router::new().fallback(|req:Request|async move {
        assert_eq!(req.method(),axum::http::Method::GET);
        assert_eq!(req.uri().path(),"/api/v0/charges");
        let url=url::Url::parse(&format!("http://test{}",req.uri())).unwrap();
        let params:HashMap<_,_>=url.query_pairs().into_owned().collect();
        let filters:Value=serde_json::from_str(&params["select_filters"]).unwrap();
        assert_eq!(filters,json!({"day":{"gte":100,"lte":200},"type":{"in":["instance"]}}));
        assert_eq!(params["format"],"table");
        let (id,next)=if params.contains_key("after_token") {(12,Value::Null)} else {(11,json!("second"))};
        Json(json!({"success":true,"count":1,"total":2,"next_token":next,"results":[{"type":"instance","source":format!("instance-{id}"),"amount":1.0}]}))
    })).await;
    let vast = praxis_vast::Vast::with_api_root("fake", &server.url()).unwrap();
    assert_eq!(vast.charges(100, 200).await.unwrap().len(), 2);
}

#[tokio::test]
async fn missing_rows_missing_cursor_and_negative_usage_fail_closed() {
    let server = MockProvider::new().await;
    let vast = praxis_vast::Vast::with_api_root("fake", &server.server.url()).unwrap();
    for response in [
        json!({"success":true,"count":0,"total":1,"next_token":null,"results":[]}),
        json!({"success":true,"count":0,"total":0,"results":[]}),
        json!({"success":false,"count":0,"total":0,"next_token":null,"results":[]}),
        json!({"success":true,"count":1,"total":1,"next_token":null,"results":[{"type":"instance","source":"instance-1","amount":-1}]}),
    ] {
        server.respond(200, &response.to_string());
        assert!(vast.charges(100, 200).await.is_err());
    }
}

#[tokio::test]
async fn reconciliation_reads_only_and_keeps_previous_snapshot_on_provider_failure() {
    let t = TestApp::new();
    let endpoint=LocalServer::new(axum::Router::new().fallback(|req:Request|async move {
        assert_eq!(req.method(),axum::http::Method::GET);
        if req.uri().path()=="/api/v1/instances/" {
            Json(json!({"instances":[{"id":12,"actual_status":"stopped","label":"praxis-llm-s1-deadbeef"}]}))
        } else {
            assert_eq!(req.uri().path(),"/api/v0/charges");
            Json(json!({"success":true,"count":2,"total":2,"next_token":null,"results":[
                {"type":"instance","source":"instance-12","amount":0.4,"metadata":{"label":"praxis-llm-s1-deadbeef"}},
                {"type":"instance","source":"instance-999","amount":999.0,"metadata":{"label":"personal-unrelated"}}
            ]}))
        }
    })).await;
    *t.app.vast.lock().unwrap() =
        Some(praxis_vast::Vast::with_api_root("fake", &endpoint.url()).unwrap());
    let result = reconcile(&t.app).await.unwrap();
    close(result.day.provider_usd, 0.4);
    assert_eq!(result.day.rows[0].state, "stopped");
    close(t.app.db.spent_today(&crate::node::local_date(&t.app)), 0.4);
    let before = t.app.db.setting("vast_billing_snapshot").unwrap();
    let broken = MockProvider::new().await;
    broken.respond(503, "{}");
    broken.attach(&t.app);
    assert!(reconcile(&t.app).await.is_err());
    assert_eq!(t.app.db.setting("vast_billing_snapshot").unwrap(), before);
    close(t.app.db.spent_today(&crate::node::local_date(&t.app)), 0.4);
    assert!(t.app.db.instances(true).is_empty());
}
