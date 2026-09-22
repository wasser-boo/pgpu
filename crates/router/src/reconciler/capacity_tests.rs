use super::*;
use crate::{hub::HeartbeatData,test_support::{MockProvider,TestApp}};
use chrono::{Duration,Utc};
use serde_json::json;

fn provider(actual:&str,cur:&str)->praxis_vast::Instance {
    serde_json::from_value(json!({"id":11,"actual_status":actual,"cur_state":cur,
        "intended_status":"running","dph_base":1.0,"storage_total_cost":0.1})).unwrap()
}
fn stopped(t:&TestApp) {
    t.insert(11,&crate::db::now_iso());
    t.app.db.complete_stop(11).unwrap();
    t.app.db.update_instance_vast(11,"stopped",1.0,1.0,1,"test",0.1).unwrap();
}
async fn observe(t:&TestApp,v:&praxis_vast::Instance) {
    sync_vast(&t.app,std::slice::from_ref(v),Utc::now()).await.unwrap();
}

#[tokio::test]
async fn resumed_contract_waits_without_preemption_duplicate_start_or_stale_health() {
    let t=TestApp::new();let p=MockProvider::new().await;p.attach(&t.app);stopped(&t);
    let _rx=t.app.hub.register(11,1,Some("127.0.0.1".into()),Default::default());
    t.app.hub.record_heartbeat(11,HeartbeatData {health_json:json!("healthy"),..Default::default()},None);
    crate::operations::start_instance(&t.app,11,"test resume").await.unwrap();
    assert_eq!(t.app.db.instance(11).unwrap().state,"start_requested");
    assert!(t.app.hub.heartbeat(11).is_none());
    let epoch=t.app.db.boot_started_at(11).unwrap().unwrap();
    for v in [provider("stopped","stopped"),provider("scheduling","stopped"),provider("running","scheduling")] {
        observe(&t,&v).await;
        assert_eq!(t.app.db.instance(11).unwrap().state,"scheduling");
        t.app.db.update_instance_agent(11,None,true,"healthy").unwrap();
        assert!(!t.app.db.instance(11).unwrap().healthy);
        crate::operations::start_automatic_instance(&t.app,11,"stale start").await.unwrap();
        crate::operations::start_instance(&t.app,11,"repeated start").await.unwrap();
        refresh_slot_routes(&t.app,1).unwrap();
        assert!(t.app.pool_routes.healthy(1).is_empty());
    }
    assert_eq!(p.calls.lock().unwrap().len(),1);
    assert!(t.app.db.machine_stat(1).is_none());
    assert_eq!(t.app.db.instance(11).unwrap().state,"scheduling");
    let reopened=crate::db::Db::open(&t.dir.join("test.sqlite")).unwrap();
    assert_eq!(reopened.instance(11).unwrap().state,"scheduling");
    observe(&t,&provider("running","running")).await;
    assert_eq!(t.app.db.instance(11).unwrap().state,"booting");
    assert!(t.app.db.boot_started_at(11).unwrap().unwrap()>epoch);
    assert!(!t.app.db.instance(11).unwrap().healthy);
    let allocated=t.app.db.boot_started_at(11).unwrap();
    observe(&t,&provider("running","running")).await;
    assert_eq!(t.app.db.boot_started_at(11).unwrap(),allocated);
    t.app.db.update_instance_agent(11,None,true,"healthy").unwrap();
    assert!(t.app.db.instance(11).unwrap().healthy);
    assert!(t.app.db.events(100,None).iter().all(|e|e.kind!="PREEMPTED" && e.kind!="machine_fail"));
}

#[tokio::test]
async fn old_inventory_cannot_confirm_preempt_or_erase_a_new_start() {
    for list in [vec![provider("running","running")],vec![provider("stopped","stopped")],vec![]] {
        let t=TestApp::new();let p=MockProvider::new().await;p.attach(&t.app);stopped(&t);
        let fetched_before_start=Utc::now();
        crate::operations::start_instance(&t.app,11,"resume").await.unwrap();
        sync_vast(&t.app,&list,fetched_before_start).await.unwrap();
        let row=t.app.db.instance(11).unwrap();
        assert_eq!(row.state,"start_requested");assert!(row.destroyed_at.is_none());
        assert!(t.app.db.machine_stat(1).is_none());
    }
}

#[tokio::test]
async fn legacy_false_preemption_recovers_only_on_positive_queue_or_allocation_evidence() {
    let t=TestApp::new();stopped(&t);
    t.app.db.set_instance_intended(11,"running").unwrap();
    t.app.db.set_instance_state(11,"preempted").unwrap();
    observe(&t,&provider("stopped","stopped")).await;
    assert_eq!(t.app.db.instance(11).unwrap().state,"preempted");
    observe(&t,&provider("scheduling","stopped")).await;
    assert_eq!(t.app.db.instance(11).unwrap().state,"scheduling");
    observe(&t,&provider("running","running")).await;
    assert_eq!(t.app.db.instance(11).unwrap().state,"booting");
    t.app.db.set_instance_state(11,"preempted").unwrap();
    observe(&t,&provider("running","running")).await;
    assert_eq!(t.app.db.instance(11).unwrap().state,"booting");
}

#[tokio::test]
async fn real_failure_after_allocation_and_explicit_stop_intent_remain_distinct() {
    let t=TestApp::new();stopped(&t);
    t.app.db.request_instance_start(11).unwrap();
    observe(&t,&provider("running","running")).await;
    observe(&t,&provider("stopped","stopped")).await;
    assert_eq!(t.app.db.instance(11).unwrap().state,"preempted");
    t.app.db.complete_stop(11).unwrap();
    observe(&t,&provider("scheduling","stopped")).await;
    assert_eq!(t.app.db.instance(11).unwrap().state,"stopped");
    assert_eq!(t.app.db.pending_operations().unwrap()[0].1,"stop");
    t.app.db.queue_operation(11,"destroy","explicit").unwrap();
    observe(&t,&provider("scheduling","stopped")).await;
    assert_eq!(t.app.db.instance(11).unwrap().intended_status,"deleted");
}

#[tokio::test]
async fn pending_capacity_reserves_hourly_budget_and_pool_without_metering_compute_as_running() {
    let t=TestApp::new();stopped(&t);
    t.app.db.request_instance_start(11).unwrap();
    observe(&t,&provider("scheduling","stopped")).await;
    let row=t.app.db.instance(11).unwrap();
    let rate=crate::costs::hourly([&row]).unwrap();
    assert!((rate.compute-1.0).abs()<1e-8);assert!((rate.storage-0.1).abs()<1e-8);
    let mut cfg=(*t.app.cfg()).clone();cfg.limits.max_total_rate_usd_h=1.2;t.app.cfg_swap(cfg);
    assert!(crate::operations::check_admission(&t.app,1,0.2,0.0,None).is_err());
    t.app.db.set_slot_desired_audited(1,true,"test wake").unwrap();
    let snapshot=build_snapshot(&t.app).await.unwrap();
    let mut future=snapshot.clone();future.now+=Duration::hours(24);
    let actions=praxis_policy::decide(&future,&effective_policy(&t.app));
    assert!(!actions.iter().any(|a|matches!(a,Action::Create{..}|Action::Start{..}|Action::Destroy{..}|Action::SwapOut{..}|Action::Stop{..})),"{actions:?}");
    let db=rusqlite::Connection::open(t.dir.join("test.sqlite")).unwrap();
    let at=Utc::now();db.execute("UPDATE instances SET last_metered_at=?1 WHERE vast_id=11",[at.to_rfc3339()]).unwrap();
    let before=t.app.db.instance_costs(&at.format("%Y-%m-%d").to_string()).unwrap().get(&11).copied().unwrap_or(0.0);
    t.app.db.meter_until(at+Duration::hours(1),chrono_tz::UTC).unwrap();
    // May cross a calendar day; sum all metered records instead of assuming one date.
    let compute:f64=db.query_row("SELECT COALESCE(SUM(metered_usd),0) FROM instance_meter WHERE instance_id=11",[],|r|r.get(0)).unwrap();
    assert!(compute<=before+0.001,"capacity queue is not an hour of allocated compute: {compute}");
    assert!(t.app.db.instance(11).unwrap().destroyed_at.is_none());
}

#[tokio::test]
async fn full_reconcile_keeps_a_long_capacity_wait_even_with_cached_healthy_agent_data() {
    let t=TestApp::new();let p=MockProvider::new().await;p.attach(&t.app);stopped(&t);
    t.app.db.request_instance_start(11).unwrap();observe(&t,&provider("scheduling","stopped")).await;
    t.app.db.set_slot_desired_audited(1,true,"test wake").unwrap();
    let conn=rusqlite::Connection::open(t.dir.join("test.sqlite")).unwrap();
    conn.execute("UPDATE instances SET boot_started_at=?1 WHERE vast_id=11",[(Utc::now()-Duration::hours(8)).to_rfc3339()]).unwrap();
    let _rx=t.app.hub.register(11,1,Some("127.0.0.1".into()),Default::default());
    t.app.hub.record_heartbeat(11,HeartbeatData{health_json:json!("healthy"),..Default::default()},None);
    p.respond(200,&json!({"success":true,"instances":[{"id":11,"actual_status":"scheduling","cur_state":"stopped","intended_status":"running","dph_base":1.0,"storage_total_cost":0.1}],"offers":[]}).to_string());
    for _ in 0..3 {tick(&t.app).await.unwrap();}
    let row=t.app.db.instance(11).unwrap();assert_eq!(row.state,"scheduling");assert!(!row.healthy);assert!(row.destroyed_at.is_none());
    assert!(t.app.db.machine_stat(1).is_none());assert!(t.app.pool_routes.healthy(1).is_empty());
    assert!(p.calls.lock().unwrap().iter().all(|(method,path,_)|method!="DELETE" && !path.contains("/asks/") && !path.contains("/instances/11/")));
}

#[tokio::test]
async fn queued_slot_can_be_cancelled_without_an_active_route_and_stale_timeout_cannot_delete_it() {
    let t=TestApp::new();let p=MockProvider::new().await;p.attach(&t.app);stopped(&t);
    t.app.db.request_instance_start(11).unwrap();observe(&t,&provider("scheduling","stopped")).await;
    t.app.db.clear_active_instance(1,11).unwrap();
    assert!(apply_action(&t.app,&Action::Destroy{instance_id:11,reason:"swap_failed: warmup timeout".into()}).await.is_err());
    assert!(p.calls.lock().unwrap().is_empty());assert!(t.app.db.machine_stat(1).is_none());
    let r=crate::api::slot_action(crate::state::AppCtx(t.app.clone()),axum::extract::Path((1,"stop".into())),t.request("{}")).await;
    assert!(r.status().is_success());assert_eq!(t.app.db.instance(11).unwrap().state,"stopped");
    let calls=p.calls.lock().unwrap();assert_eq!(calls.len(),1);assert_eq!(calls[0].2,json!({"state":"stopped"}));
}

#[tokio::test]
async fn resume_does_not_apply_changed_template_env_or_run_a_gpu_command() {
    let t=TestApp::new();let p=MockProvider::new().await;p.attach(&t.app);stopped(&t);
    let mut cfg=(*t.app.cfg()).clone();cfg.slots[0].env.insert("LLAMA_CONTEXT_SIZE".into(),"16384".into());t.app.cfg_swap(cfg);
    crate::operations::start_instance(&t.app,11,"resume").await.unwrap();
    let calls=p.calls.lock().unwrap();assert_eq!(calls.len(),1);
    assert_eq!(calls[0].0,"PUT");assert_eq!(calls[0].2,json!({"state":"running"}));
    assert_eq!(t.app.db.instance(11).unwrap().image,"test");
}

#[tokio::test]
async fn provider_start_error_retains_disk_blocks_automatic_churn_and_can_be_retried_explicitly() {
    let t=TestApp::new();let p=MockProvider::new().await;p.attach(&t.app);stopped(&t);
    t.app.db.request_instance_start(11).unwrap();
    observe(&t,&provider("error","stopped")).await;
    assert_eq!(t.app.db.instance(11).unwrap().state,"start_failed");
    t.app.db.update_instance_agent(11,None,true,"healthy").unwrap();assert!(!t.app.db.instance(11).unwrap().healthy);
    crate::operations::start_automatic_instance(&t.app,11,"stale auto start").await.unwrap();
    assert!(p.calls.lock().unwrap().is_empty());assert!(t.app.db.machine_stat(1).is_none());
    crate::operations::start_instance(&t.app,11,"explicit retry").await.unwrap();
    assert_eq!(t.app.db.instance(11).unwrap().state,"start_requested");assert_eq!(p.calls.lock().unwrap().len(),1);
    observe(&t,&provider("running","running")).await;
    assert_eq!(t.app.db.instance(11).unwrap().state,"booting");
    // A cleanup planned before this new allocation cannot time out the new boot.
    for reason in ["swap_failed: warmup timeout","preempted vor healthy — aufräumen","destroy_after_stopped: old deadline"] {
        assert!(crate::operations::automatic_request(&t.app,11,"destroy",reason).await.is_err());
    }
    assert_eq!(p.calls.lock().unwrap().len(),1);
}

#[test]
fn provider_queue_signals_do_not_confuse_desired_running_with_allocated_capacity() {
    let mut v=provider("stopped","stopped");v.next_state=Some("running".into());
    assert!(!v.allocation_running());assert!(!v.waiting_for_capacity());
    v.status=Some("scheduling".into());assert!(v.waiting_for_capacity());assert!(!v.allocation_running());
    v=provider("running","stopped");assert!(!v.allocation_running());
    v=provider("running","running");assert!(v.allocation_running());
}

#[tokio::test]
async fn fresh_provisioning_download_progress_remains_visible_without_readiness() {
    let t=TestApp::new();stopped(&t);t.app.db.request_instance_start(11).unwrap();
    observe(&t,&provider("loading","loading")).await;
    assert_eq!(t.app.db.instance(11).unwrap().state,"provisioning");
    let _rx=t.app.hub.register(11,1,Some("127.0.0.1".into()),Default::default());
    t.app.hub.record_heartbeat(11,HeartbeatData{health_json:json!("downloading"),progress_json:json!({"pct":42.0}),..Default::default()},None);
    t.app.db.update_instance_agent(11,None,false,"booting").unwrap();
    let state=crate::dashboard::state_json(&t.app);
    assert_eq!(state["slots"][0]["progress"]["pct"],42.0);
    assert_eq!(state["slots"][0]["healthy"],false);
    assert_eq!(t.app.db.instance(11).unwrap().state,"provisioning");
}

#[test]
fn old_agent_liveness_is_not_a_new_boot_timeout() {
    let now=Utc::now();
    assert_eq!(capacity::silence_since_boot(Some(now),Some(now.timestamp()-86400),now.timestamp()),0);
    assert_eq!(capacity::silence_since_boot(Some(now-Duration::seconds(300)),Some(now.timestamp()-20),now.timestamp()),20);
}
