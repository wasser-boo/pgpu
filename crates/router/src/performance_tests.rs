//! Offline-only feature regressions. No real agent/provider/GPU is contacted.
use crate::{catalog, performance, state::AppCtx, test_support::TestApp};
use praxis_common::{Mode, performance::Metrics};
use praxis_policy::OfferSnapshot;
use std::sync::Arc;

fn configure(t: &TestApp, f: impl FnOnce(&mut crate::config::Config)) {
    let mut c=(*t.app.cfg()).clone(); f(&mut c); *t.app.cfg.write().unwrap()=Arc::new(c);
}
fn offer() -> OfferSnapshot {
    OfferSnapshot {id:10,machine_id:42,gpu_name:"RTX 5090".into(),num_gpus:1,gpu_ram_gb:32.0,cpu_ram_gb:128.0,cpu_cores:Some(16.0),
        min_bid:0.2,dph_total:0.4,disk_gb:120.0,inet_down:1000.0,disk_bw:2000.0,reliability2:0.99,cuda_max_good:Some(13.0),..Default::default()}
}
fn eligible(t: &TestApp, o: &OfferSnapshot) -> bool {
    catalog::assess(&t.app,&t.app.cfg().slots[0],o,Mode::Interruptible,0.3,60).eligible
}
fn enable(t: &TestApp) {
    configure(t,|c| {let p=&mut c.slots[0].performance;p.enabled=true;p.collect_usage=true;p.profile="test-q4-ctx8k".into();p.benchmark.spec.model="test-model".into();});
}

#[test]
fn all_blacklist_whitelist_switch_combinations_preserve_entries() {
    let t=TestApp::new(); let o=offer();
    assert!(eligible(&t,&o));
    t.app.db.set_machine_blacklist(o.machine_id,true,"test").unwrap();
    t.app.db.set_machine_whitelist(o.machine_id,true).unwrap();
    for blacklist in [false,true] {for whitelist in [false,true] {
        configure(&t,|c| {c.vast.activate_blacklist=blacklist;c.vast.activate_whitelist=whitelist;});
        assert_eq!(eligible(&t,&o),!blacklist);
        let mut unknown=o.clone();unknown.machine_id=99;
        assert_eq!(eligible(&t,&unknown),!whitelist);
    }}
    let reopened=crate::db::Db::open(&t.dir.join("test.sqlite")).unwrap();
    let state=reopened.machine_stat(o.machine_id).unwrap();
    assert!(state.whitelisted && state.blacklisted);
}

#[test]
fn whitelist_never_bypasses_model_hardware_or_cost_limits() {
    let t=TestApp::new();let mut o=offer();
    t.app.db.set_machine_whitelist(o.machine_id,true).unwrap();
    configure(&t,|c| {c.vast.activate_whitelist=true;let r=&mut c.slots[0].requirements;
        r.gpu_names=vec!["NVIDIA RTX_5090".into()];r.min_gpu_ram_gb=32.0;r.min_cpu_ram_gb=64.0;r.min_cuda=12.9;r.num_gpus=Some(1);
        r.gpu_price_ceiling_usd_h.insert("RTX 5090".into(),0.35);});
    assert!(eligible(&t,&o));
    o.gpu_name="RTX 5090 D".into();assert!(!eligible(&t,&o));o=offer();
    o.gpu_ram_gb=24.0;assert!(!eligible(&t,&o));o=offer();
    o.cuda_max_good=None;assert!(!eligible(&t,&o));o=offer();
    o.disk_gb=40.0;assert!(!eligible(&t,&o));o=offer();
    o.min_bid=0.5;assert!(!eligible(&t,&o));
    o=offer();
    let c=t.app.cfg();
    assert!(!catalog::assess(&t.app,&c.slots[0],&o,Mode::Interruptible,0.36,60).eligible);
    assert!(!catalog::assess(&t.app,&c.slots[0],&o,Mode::OnDemand,0.01,60).eligible); // cannot fake on-demand price
}

#[tokio::test]
async fn rejected_create_and_resume_never_contact_provider() {
    let t=TestApp::new();configure(&t,|c|c.vast.activate_whitelist=true);
    let provider=crate::test_support::MockProvider::new().await;provider.attach(&t.app);
    let err=crate::reconciler::create_instance(&t.app,1,&offer(),Mode::Interruptible,0.3,60,Default::default(),false,"test").await.unwrap_err();
    assert!(err.to_string().contains("Whitelist"));
    t.insert(1,&crate::db::now_iso());
    assert!(crate::operations::start_instance(&t.app,1,"test").await.is_err());
    assert!(provider.calls.lock().unwrap().is_empty());
}

#[test]
fn startup_traffic_counts_in_addition_to_existing_rate_projection() {
    let t=TestApp::new();t.insert(1,&crate::db::now_iso());
    configure(&t,|c|c.budget.daily_soft_eur=1.2);
    assert!(crate::operations::check_admission(&t.app,1,1.0,0.0,None).is_ok());
    assert!(crate::operations::check_admission_with_startup(&t.app,1,1.0,0.0,None,0.2).is_err());
}

#[test]
fn benchmarks_exclude_proxy_traffic_atomically_and_release_on_drop() {
    let t=TestApp::new();let request=t.app.traffic.try_begin(1).unwrap();
    assert!(t.app.traffic.try_benchmark(1).is_none());drop(request);
    let lease=t.app.traffic.try_benchmark(1).unwrap();
    assert!(t.app.traffic.try_begin(1).is_none());
    assert!(t.app.traffic.try_benchmark(1).is_none());
    assert_eq!(t.app.traffic.snapshot(1).in_flight,1);drop(lease);
    assert!(t.app.traffic.try_begin(1).is_some());
}

#[test]
fn passive_sse_records_metrics_not_user_text_and_handles_split_frames() {
    let t=TestApp::new();enable(&t);t.insert(1,&crate::db::now_iso());
    let mut observer=performance::UsageObserver::new(&t.app,1,"/v1/chat/completions",&axum::http::Method::POST).unwrap();
    let mut headers=axum::http::HeaderMap::new();headers.insert("content-type","text/event-stream".parse().unwrap());
    observer.response(axum::http::StatusCode::OK,&headers);
    let data=b"data: {\"model\":\"test-model\",\"choices\":[{\"delta\":{\"content\":\"PRIVATE GENERATED TEXT\"}}]}\n\ndata: {\"usage\":{\"completion_tokens\":80,\"prompt_tokens\":900},\"timings\":{\"predicted_per_second\":45.0,\"prompt_per_second\":800.0}}\n\ndata: [DONE]\n\n";
    for piece in data.chunks(7) {observer.feed(piece);}observer.finish(true);
    let rows=t.app.db.performance_samples(None,None,None,10).unwrap();assert_eq!(rows.len(),1);
    let m=&rows[0].metrics;
    assert!(rows[0].success);assert_eq!(m.decode_tps,Some(45.0));assert_eq!(m.output_tokens,Some(80));assert!(m.ttft_ms.is_some());
    assert!(!serde_json::to_string(&rows).unwrap().contains("PRIVATE"));
    assert!(!serde_json::to_string(&t.app.db.performance_catalogue().unwrap()).unwrap().contains("PRIVATE"));
    assert!(performance::host_scores(&t.app,&t.app.cfg().slots[0]).unwrap().is_empty());
}

#[test]
fn passive_json_abort_error_and_oversized_events_are_safe() {
    let t=TestApp::new();enable(&t);t.insert(1,&crate::db::now_iso());
    let mut o=performance::UsageObserver::new(&t.app,1,"/v1/chat/completions",&axum::http::Method::POST).unwrap();
    o.response(axum::http::StatusCode::OK,&Default::default());
    o.feed(br#"{"model":"test-model","usage":{"completion_tokens":10},"timings":{"predicted_per_second":20},"choices":[{"message":{"content":"SECRET"}}]}"#);
    o.finish(true);
    let mut o=performance::UsageObserver::new(&t.app,1,"/completion",&axum::http::Method::POST).unwrap();
    o.response(axum::http::StatusCode::OK,&Default::default());o.feed(&vec![b'x';300000]);o.finish(false);
    let rows=t.app.db.performance_samples(None,None,None,10).unwrap();
    assert_eq!(rows.len(),2);assert!(!rows[0].success);assert!(rows[1].success);assert_eq!(rows[1].metrics.ttft_ms,None);
    assert!(!serde_json::to_string(&rows).unwrap().contains("SECRET"));
    configure(&t,|c|c.slots[0].performance.enabled=false);
    assert!(performance::UsageObserver::new(&t.app,1,"/completion",&axum::http::Method::POST).is_none());
}

fn sample(t:&TestApp) -> crate::db::PerformanceRow {
    let slot=&t.app.cfg().slots[0];
    crate::db::PerformanceRow {id:0,ts:chrono::Utc::now().timestamp(),slot_id:1,instance_id:1,machine_id:42,gpu_name:"RTX 5090".into(),
        profile:slot.performance.profile.clone(),workload_key:performance::workload_key(slot,&slot.image),allocation:performance::allocation_key(&offer(),slot.disk_gb),
        image:slot.image.clone(),source:"benchmark".into(),success:true,price_usd_h:0.3,
        metrics:Metrics {model:"test-model".into(),elapsed_ms:3000.0,ttft_ms:Some(500.0),decode_tps:Some(80.0),prefill_tps:Some(1000.0),..Default::default()} }
}

#[test]
fn history_survives_restart_and_raw_retention_keeps_lifetime_catalogue() {
    let t=TestApp::new();enable(&t);let row=sample(&t);
    for _ in 0..3 {t.app.db.record_performance(&row,90,2).unwrap();}
    let db=crate::db::Db::open(&t.dir.join("test.sqlite")).unwrap();
    assert_eq!(db.performance_samples(None,None,None,100).unwrap().len(),2);
    let cat=db.performance_catalogue().unwrap();assert_eq!(cat.len(),1);assert_eq!(cat[0].samples,3);
    assert_eq!(cat[0].means()["decode_tps"],80.0);
    let mut changed=row.clone();changed.metrics.model="different-model".into();
    db.record_performance(&changed,90,2).unwrap();assert_eq!(db.performance_catalogue().unwrap().len(),2);
}

#[test]
fn scores_require_matching_workload_model_allocation_sample_count_and_age() {
    let t=TestApp::new();enable(&t);let mut row=sample(&t);
    for _ in 0..2 {t.app.db.record_performance(&row,90,1000).unwrap();}
    assert!(performance::host_scores(&t.app,&t.app.cfg().slots[0]).unwrap().is_empty());
    t.app.db.record_performance(&row,90,1000).unwrap();
    let score=performance::host_scores(&t.app,&t.app.cfg().slots[0]).unwrap();
    assert!(score[&(42,performance::allocation_key(&offer(),60))]>60.0);
    let mut smaller=offer();smaller.gpu_ram_gb=16.0;
    assert!(!score.contains_key(&(42,performance::allocation_key(&smaller,60))));
    configure(&t,|c|c.slots[0].performance.profile="different-quant".into());
    assert!(performance::host_scores(&t.app,&t.app.cfg().slots[0]).unwrap().is_empty());
    row.workload_key=performance::workload_key(&t.app.cfg().slots[0],&t.app.cfg().slots[0].image);
    row.ts-=40*86400;
    for _ in 0..3 {t.app.db.record_performance(&row,90,1000).unwrap();}
    assert!(performance::host_scores(&t.app,&t.app.cfg().slots[0]).unwrap().is_empty());
}

#[tokio::test]
async fn benchmark_is_opt_in_idle_exclusive_and_persists_results_and_cooldown() {
    let t=TestApp::new();t.insert(1,&crate::db::now_iso());
    assert!(performance::start_benchmark(&t.app,1).await.is_err());enable(&t);
    configure(&t,|c| {c.slots[0].performance.benchmark.enabled=true;c.slots[0].performance.benchmark.idle_s=0;});
    let mut rx=t.app.hub.register(1,1,None,Default::default());
    t.app.hub.record_heartbeat(1,crate::hub::HeartbeatData {health_json:serde_json::json!("healthy"),..Default::default()},None);
    performance::start_benchmark(&t.app,1).await.unwrap();
    assert!(t.app.traffic.snapshot(1).benchmarking);
    assert!(performance::start_benchmark(&t.app,1).await.is_err());
    let command=rx.recv().await.unwrap();
    let praxis_common::node::RouterCommand::Cmd {id,command:praxis_common::node::Command::Benchmark {spec}}=command else {panic!("wrong command")};
    let metrics=sample(&t).metrics;
    t.app.hub.resolve(1,id,Ok(serde_json::json!({"schema":1,"samples":vec![metrics;spec.requests as usize],"disk_status":"disabled"})));
    tokio::time::timeout(std::time::Duration::from_secs(3),async {while t.app.traffic.snapshot(1).benchmarking {tokio::task::yield_now().await;}}).await.unwrap();
    assert_eq!(t.app.db.performance_samples(None,None,None,10).unwrap().len(),3);
    assert!(performance::start_benchmark(&t.app,1).await.unwrap_err().to_string().contains("cooldown"));
    let response=crate::api::instance_action(AppCtx(t.app.clone()),axum::extract::Path((1,"benchmark".into())),t.request("{}")).await;
    assert_eq!(response.status(),axum::http::StatusCode::CONFLICT);
}

#[test]
fn measured_preference_is_opt_in_and_never_overrides_trust_or_price() {
    let t=TestApp::new();enable(&t);
    let cheap=offer();let mut fast=offer();fast.id=11;fast.machine_id=43;fast.min_bid=0.21;
    t.app.db.cache_offers(1,"",&serde_json::to_string(&vec![cheap.clone(),fast.clone()]).unwrap()).unwrap();
    let mut row=sample(&t);row.machine_id=43;
    for _ in 0..3 {t.app.db.record_performance(&row,90,1000).unwrap();}
    assert_eq!(crate::reconciler::best_candidate(&t.app,1).unwrap().id,cheap.id);
    configure(&t,|c|c.slots[0].performance.score.enabled=true);
    assert_eq!(crate::reconciler::best_candidate(&t.app,1).unwrap().id,fast.id);
    t.app.db.set_machine_blacklist(43,true,"test").unwrap();
    assert_eq!(crate::reconciler::best_candidate(&t.app,1).unwrap().id,cheap.id);
    configure(&t,|c|c.vast.activate_blacklist=false);
    assert_eq!(crate::reconciler::best_candidate(&t.app,1).unwrap().id,fast.id);
    configure(&t,|c|c.slots[0].bid.ceiling_usd_h=0.205);
    assert_eq!(crate::reconciler::best_candidate(&t.app,1).unwrap().id,cheap.id);
    configure(&t,|c|c.vast.activate_whitelist=true);
    assert!(crate::reconciler::best_candidate(&t.app,1).is_none());
}

#[test]
fn config_reload_cannot_relabel_an_existing_instances_runtime() {
    let t=TestApp::new();enable(&t);t.insert(1,&crate::db::now_iso());
    configure(&t,|c| {c.slots[0].image="test".into();c.slots[0].env.insert("LLAMA_KV_TYPE".into(),"f16".into());});
    let slot=t.app.cfg().slots[0].clone();
    let old_key=performance::workload_key(&slot,"test");
    t.app.db.save_rental_facts(1,&offer(),&performance::runtime_key(&slot,"test")).unwrap();
    configure(&t,|c| {c.slots[0].env.insert("LLAMA_KV_TYPE".into(),"q8_0".into());});
    let mut o=performance::UsageObserver::new(&t.app,1,"/completion",&axum::http::Method::POST).unwrap();
    o.response(axum::http::StatusCode::OK,&Default::default());o.feed(br#"{"model":"test-model"}"#);o.finish(true);
    let rows=t.app.db.performance_samples(None,None,None,10).unwrap();
    assert_eq!(rows[0].workload_key,old_key);
    assert_ne!(rows[0].workload_key,performance::workload_key(&t.app.cfg().slots[0],"test"));
}

#[test]
fn feature_config_defaults_and_invalid_benchmark_controls() {
    let raw=include_str!("../../../config.example.toml");
    let c=crate::config::Config::load_str(raw).unwrap();
    assert!(c.vast.activate_blacklist);assert!(!c.vast.activate_whitelist);
    assert!(!c.slots[0].performance.enabled && !c.slots[0].performance.benchmark.enabled && !c.slots[0].performance.score.enabled);
    assert!(crate::config::Config::load_str(&raw.replace("preference_weight = 0.25","preference_weight = nan")).is_err());
    assert!(crate::config::Config::load_str(&raw.replace("min_interval_s = 86400","min_interval_s = 0")).is_err());
    assert!(crate::config::Config::load_str(&raw.replace("activate_blacklist = true","activate_blacklist = 'false'")).is_err());
}

#[test]
fn incomplete_sse_is_failure_even_with_clean_http_eof() {
    let t=TestApp::new();enable(&t);t.insert(1,&crate::db::now_iso());
    let mut o=performance::UsageObserver::new(&t.app,1,"/v1/chat/completions",&axum::http::Method::POST).unwrap();
    let mut h=axum::http::HeaderMap::new();h.insert("content-type","text/event-stream".parse().unwrap());
    o.response(axum::http::StatusCode::OK,&h);o.feed(b"data: {\"model\":\"test-model\"}\n\n");o.finish(true);
    assert!(!t.app.db.performance_samples(None,None,None,1).unwrap()[0].success);
}

#[tokio::test]
async fn disconnected_benchmark_releases_lease_and_records_failed_attempt() {
    let t=TestApp::new();t.insert(1,&crate::db::now_iso());enable(&t);
    configure(&t,|c| {c.slots[0].performance.benchmark.enabled=true;c.slots[0].performance.benchmark.idle_s=0;});
    let _rx=t.app.hub.register(1,1,None,Default::default());
    t.app.hub.record_heartbeat(1,crate::hub::HeartbeatData {health_json:serde_json::json!("healthy"),..Default::default()},None);
    performance::start_benchmark(&t.app,1).await.unwrap();t.app.hub.unregister(1);
    tokio::time::timeout(std::time::Duration::from_secs(3),async {while t.app.traffic.snapshot(1).benchmarking {tokio::task::yield_now().await;}}).await.unwrap();
    let rows=t.app.db.performance_samples(None,None,None,10).unwrap();
    assert_eq!(rows.len(),1);assert!(!rows[0].success);
}

#[tokio::test]
async fn performance_endpoints_require_authentication_and_validate_limits() {
    let t=TestApp::new();
    let req=axum::extract::Request::builder().body(axum::body::Body::empty()).unwrap();
    assert_eq!(crate::api::performance_summary(AppCtx(t.app.clone()),req).await.status(),axum::http::StatusCode::UNAUTHORIZED);
    let q=[("limit".into(),"1000000000".into())].into_iter().collect();
    assert_eq!(crate::api::performance_history(AppCtx(t.app.clone()),axum::extract::Query(q),t.request("")).await.status(),axum::http::StatusCode::BAD_REQUEST);
}
