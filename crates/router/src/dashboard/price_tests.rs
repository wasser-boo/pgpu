use super::*;
use crate::test_support::TestApp;

#[tokio::test]
async fn rendered_instance_prices_use_current_on_demand_compute_and_storage_not_old_bid() {
    let t = TestApp::new();
    t.insert(11, &crate::db::now_iso());
    let conn = rusqlite::Connection::open(t.dir.join("test.sqlite")).unwrap();
    conn.execute("UPDATE instances SET mode='on_demand',bid_usd_h=0.144444,dph_total=0.457333,storage_usd_h=0.011111 WHERE vast_id=11",[]).unwrap();
    let row = t.app.db.instance(11).unwrap();
    let view = inst_view(&t.app, &row);
    assert!((view.bid_usd_h - 0.457333).abs() < 1e-8);
    let response = instance_page(
        AppCtx(t.app.clone()),
        Path(11),
        axum::http::Method::GET,
        t.request("").headers().clone(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    for expected in [
        "0.4573",
        "0.0111",
        "0.4684",
        "auch im Stop",
        "kein On-demand-Preis",
    ] {
        assert!(body.contains(expected), "missing {expected}");
    }
    assert!(!body.contains("name=\"price\""));
    let budget = budget_view(&t.app);
    assert!(budget.current_rate.contains("0.4684"));
}

#[tokio::test]
async fn settings_hide_legacy_provider_html_and_explain_charges_and_four_hour_digests() {
    let t = TestApp::new();
    t.app.db.set_setting("vast_billing_status",&serde_json::json!({"state":"error","at":"2026-09-22T12:00:00Z","message":"vast /charges: 301 Moved Permanently\n<!doctype html><h1>REMOTE-HTML-MARKER</h1><script>secret</script>"}).to_string()).unwrap();
    let view = billing_view(&t.app);
    assert!(view.failed);
    assert!(view.status.contains("301"));
    assert!(!view.status.contains("<"));
    let response = settings_page(
        AppCtx(t.app.clone()),
        Query(Default::default()),
        t.request(""),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    for expected in [
        "/api/v0/charges/",
        "Tatsächliche Vast-Nutzung",
        "Lokale Slot-Schätzung",
        "301 Moved Permanently",
        "14400",
        "state_changes",
        "kein API-Request-Limit",
    ] {
        assert!(body.contains(expected), "missing {expected}");
    }
    assert!(!body.contains("REMOTE-HTML-MARKER"));
}
