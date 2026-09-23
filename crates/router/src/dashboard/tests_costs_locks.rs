use super::*;
use crate::{billing,db::Db,test_support::{MockProvider,TestApp}};
use chrono::{Duration,TimeZone,Utc};

fn near(a:f64,b:f64) {assert!((a-b).abs()<1e-7,"{a} != {b}");}
fn headers(t:&TestApp)->axum::http::HeaderMap {t.request("{}").headers().clone()}
fn snapshot(t:&TestApp,now:chrono::DateTime<Utc>,amount:f64)->billing::Snapshot {
    let date=now.with_timezone(&t.app.cfg().router.tz.parse::<chrono_tz::Tz>().unwrap()).format("%Y-%m-%d").to_string();
    let day=billing::Window {period:date.clone(),from_unix:now.timestamp()-100,through_unix:now.timestamp(),provider_usd:amount,
        estimated_usd:0.0,ignored_contracts:0,rows:vec![billing::Row{instance_id:11,slot_id:1,label:"praxis-llm-s1-deadbeef".into(),state:"stopped".into(),provider_usd:Some(amount),estimated_usd:None}]};
    billing::Snapshot {scope:crate::slot_labels::scope(&t.app.cfg()),timezone:t.app.cfg().router.tz.clone(),synced_at:now.to_rfc3339(),
        month:billing::Window{period:date[..7].into(),..day.clone()},day}
}
fn save(t:&TestApp,s:&billing::Snapshot) {t.app.db.record_provider_usage(&serde_json::to_string(s).unwrap(),&[]).unwrap();}

#[tokio::test]
async fn dashboard_costs_include_stopped_destroyed_traffic_and_provider_only_history_without_double_counting() {
    let t=TestApp::new();
    let mut cfg=(*t.app.cfg()).clone();let mut second=cfg.slots[0].clone();second.id=2;second.role=praxis_common::Role::Media;cfg.slots.push(second);
    t.app.db.init_slots(&cfg).unwrap();t.app.cfg_swap(cfg);
    let now=Utc::now();let day=now.date_naive().and_hms_opt(0,0,0).unwrap().and_utc();let date=day.format("%Y-%m-%d").to_string();
    for id in [11,12,13,21] {t.insert(id,&day.to_rfc3339());}
    let conn=rusqlite::Connection::open(t.dir.join("test.sqlite")).unwrap();
    conn.execute("UPDATE instances SET slot_id=2,role='media' WHERE vast_id=21",[]).unwrap();
    t.app.db.set_active_instance(1,Some(11)).unwrap();t.app.db.set_active_instance(2,Some(21)).unwrap();
    t.app.db.meter_until(day+Duration::hours(1),chrono_tz::UTC).unwrap();
    t.app.db.complete_stop(12).unwrap();t.app.db.update_instance_vast(12,"stopped",1.0,1.0,1,"test",0.1).unwrap();
    t.app.db.meter_until(day+Duration::hours(2),chrono_tz::UTC).unwrap();t.app.db.mark_destroyed(13).unwrap();
    t.app.db.meter_until(day+Duration::hours(3),chrono_tz::UTC).unwrap();
    t.app.db.meter_instance_traffic(&date,11,0.4).unwrap();
    // A different day must not bleed into the day card.
    conn.execute("INSERT INTO instance_meter(date,instance_id,metered_usd,last_metered_at) VALUES(?1,11,99,?2)",rusqlite::params![(day-Duration::days(1)).format("%Y-%m-%d").to_string(),day.to_rfc3339()]).unwrap();
    let mut s=snapshot(&t,now,4.65);
    s.day.rows=[(11,1,3.0),(12,1,0.25),(13,1,0.2),(21,2,0.5),(888,1,0.7)].into_iter().map(|(id,slot,amount)|billing::Row {
        instance_id:id,slot_id:slot,label:format!("praxis-llm-s{slot}-deadbeef"),state:"historical".into(),provider_usd:Some(amount),estimated_usd:None,
    }).collect();save(&t,&s);
    let before=t.app.db.budget_totals(&date).unwrap();
    let view=billing::today(&t.app,now);
    near(view.local.unwrap(),10.5);near(view.local_slots[&1],7.2);near(view.local_slots[&2],3.3);
    near(view.provider.unwrap(),4.65);near(view.provider_slots[&1],4.15);near(view.provider_slots[&2],0.5);
    assert!(view.unassigned.is_none());
    let response=index(crate::state::AppCtx(t.app.clone()),t.request("{}")).await;
    assert!(response.status().is_success());
    let html=String::from_utf8(axum::body::to_bytes(response.into_body(),1<<20).await.unwrap().to_vec()).unwrap();
    for value in ["7.2000 USD","3.3000 USD","4.1500 USD","4.6500 USD","10.5000 USD","Vast bisher gemeldet","Schätzung","30000","Bisher verbraucht (heute): 10.5000 USD","aria-label=\"Bisheriger Tagesverbrauch\""] {assert!(html.contains(value),"missing {value}");}
    assert_eq!(t.app.db.budget_totals(&date).unwrap(),before,"rendering cannot alter ledgers");
    assert_eq!(t.app.cfg().budget.daily_hard_eur,2000.0);
}

#[test]
fn today_is_timezone_scoped_unknown_without_a_matching_window_and_honest_about_stale_data() {
    let t=TestApp::new();let mut cfg=(*t.app.cfg()).clone();cfg.router.tz="Europe/Berlin".into();t.app.cfg_swap(cfg);
    let now=Utc.with_ymd_and_hms(2026,1,1,23,30,0).unwrap();
    assert_eq!(billing::today(&t.app,now).date,"2026-01-02");assert!(billing::today(&t.app,now).provider.is_none());
    let mut s=snapshot(&t,now,0.125);save(&t,&s);
    near(billing::today(&t.app,now).provider.unwrap(),0.125);
    assert!(!billing::today(&t.app,now).stale);
    t.app.db.set_setting("vast_billing_status",r#"{"state":"error","message":"hidden provider HTML"}"#).unwrap();
    assert!(billing::today(&t.app,now).stale);
    t.app.db.set_setting("vast_billing_status",r#"{"state":"ok"}"#).unwrap();
    s.day.through_unix-=7201;save(&t,&s);assert!(billing::today(&t.app,now).stale);
    s.day.period="2026-01-01".into();save(&t,&s);assert!(billing::today(&t.app,now).provider.is_none());
    s=snapshot(&t,now,0.125);s.scope.clear();save(&t,&s);assert!(billing::today(&t.app,now).provider.is_none());
    s=snapshot(&t,now,0.125);s.timezone="UTC".into();save(&t,&s);assert!(billing::today(&t.app,now).provider.is_none());
}

#[test]
fn legacy_lock_and_pending_intent_migration_is_safe_and_persistent_after_contract_loss() {
    let t=TestApp::new();t.insert(11,&crate::db::now_iso());
    t.app.db.set_slot_pin(1,Some(11)).unwrap();t.app.db.update_instance_pinned(11,true).unwrap();
    t.app.db.queue_operation(11,"stop","legacy unknown origin").unwrap();
    let conn=rusqlite::Connection::open(t.dir.join("test.sqlite")).unwrap();
    conn.execute_batch("DROP TRIGGER notification_slot_lock; ALTER TABLE slots DROP COLUMN locked; ALTER TABLE pending_operations DROP COLUMN respect_lock;").unwrap();
    let migrated=Db::open(&t.dir.join("test.sqlite")).unwrap();
    assert!(migrated.slot_locks().unwrap().contains(&1));assert!(migrated.pending_respects_lock(11).unwrap());
    migrated.mark_destroyed(11).unwrap();assert!(migrated.slot_locks().unwrap().contains(&1));
    migrated.set_slot_locked(1,false).unwrap();assert!(migrated.slot_locks().unwrap().is_empty());
}

#[tokio::test]
async fn scheduled_lock_and_unlock_use_the_same_empty_slot_lock() {
    let t=TestApp::new();let mut cfg=(*t.app.cfg()).clone();
    cfg.schedule=vec![serde_json::from_value(serde_json::json!({"time":Utc::now().format("%H:%M").to_string(),"action":" LOCK "})).unwrap()];
    t.app.cfg_swap(cfg.clone());crate::reconciler::run_schedules(&t.app).await;
    assert!(t.app.db.slot_locks().unwrap().contains(&1));
    cfg.schedule[0].action="unlock".into();t.app.cfg_swap(cfg);crate::reconciler::run_schedules(&t.app).await;
    assert!(t.app.db.slot_locks().unwrap().is_empty());
}

#[tokio::test]
async fn unavailable_local_cost_ledger_never_renders_as_zero_spend() {
    let t=TestApp::new();let conn=rusqlite::Connection::open(t.dir.join("test.sqlite")).unwrap();
    conn.execute_batch("DROP TABLE budget_days;").unwrap();
    let view=billing::today(&t.app,Utc::now());assert!(view.local.is_none());
    assert!(!budget_view(&t.app).costs_known);
    let response=index(crate::state::AppCtx(t.app.clone()),t.request("{}")).await;
    let html=String::from_utf8(axum::body::to_bytes(response.into_body(),1<<20).await.unwrap().to_vec()).unwrap();
    assert!(html.contains("Budget-Verbrauch nicht verfügbar"));
    assert!(html.contains("Lokal erfasst (Schätzung): nicht verfügbar"));
    assert!(!html.contains("0.0000 USD für das Budget"));
    assert!(html.contains("Bisher verbraucht (heute): nicht verfügbar"));
    assert!(!html.contains("aria-label=\"Bisheriger Tagesverbrauch\""));
}

#[test]
fn daily_budget_and_consumption_bar_reset_at_local_midnight_without_erasing_month() {
    let t=TestApp::new();
    let mut cfg=(*t.app.cfg()).clone();
    cfg.router.tz="Europe/Berlin".into();
    cfg.budget.daily_soft_eur=10.0;cfg.budget.daily_hard_eur=20.0;cfg.budget.usd_per_eur=1.0;
    t.app.cfg_swap(cfg);
    // Midnight in Berlin is still the previous UTC date.
    let midnight=Utc.with_ymd_and_hms(2026,1,2,23,0,0).unwrap();
    t.insert(11,&(midnight-Duration::hours(1)).to_rfc3339());
    t.app.db.meter_until(midnight,chrono_tz::Europe::Berlin).unwrap();
    t.app.db.record_provider_usage("{}",&[
        ("2026-01-02".into(),11,15.0,1.1),
        ("2026-01".into(),11,15.0,1.1),
    ]).unwrap();
    let before=budget_view_at(&t.app,midnight-Duration::seconds(1));
    near(before.spent_usd,15.0);near(before.spent_pct,100.0);
    assert_eq!(before.spent_bar_class,"warn");
    let after=budget_view_at(&t.app,midnight);
    near(after.spent_usd,0.0);near(after.spent_pct,0.0);near(after.spent_month_usd,15.0);
    assert_eq!(after.spent_bar_class,"");
    assert!(after.projected_usd>0.0,"future running costs are not consumption");
    t.app.db.meter_until(midnight+Duration::hours(1),chrono_tz::Europe::Berlin).unwrap();
    let later=budget_view_at(&t.app,midnight+Duration::hours(1));
    near(later.spent_usd,1.1);near(later.spent_pct,11.0);near(later.spent_month_usd,16.1);
    near(t.app.db.spent_today("2026-01-02"),15.0);
}

#[test]
fn budget_projection_uses_real_calendar_midnight_on_both_dst_changes() {
    let tz=chrono_tz::Europe::Berlin;
    assert_eq!(billing::seconds_to_day_end(Utc.with_ymd_and_hms(2026,3,28,23,0,0).unwrap(),tz).unwrap(),23*3600);
    assert_eq!(billing::seconds_to_day_end(Utc.with_ymd_and_hms(2026,10,24,22,0,0).unwrap(),tz).unwrap(),25*3600);
}

#[tokio::test]
async fn lock_button_works_for_empty_slots_persists_and_never_contacts_the_provider() {
    let t=TestApp::new();let p=MockProvider::new().await;p.attach(&t.app);
    let r=do_slot_action(crate::state::AppCtx(t.app.clone()),Path((1,"lock".into())),headers(&t)).await;
    assert_eq!(r.status(),StatusCode::SEE_OTHER);assert!(t.app.db.slot_locks().unwrap().contains(&1));
    assert_eq!(state_json(&t.app)["slots"][0]["locked"],true);
    assert!(Db::open(&t.dir.join("test.sqlite")).unwrap().slot_locks().unwrap().contains(&1));
    let r=index(crate::state::AppCtx(t.app.clone()),t.request("{}")).await;
    let html=String::from_utf8(axum::body::to_bytes(r.into_body(),1<<20).await.unwrap().to_vec()).unwrap();
    assert!(html.contains("LOCKED"));assert!(html.contains("/do/slots/1/unlock"));assert!(html.contains("Vast bisher gemeldet: nicht verfügbar"));
    let r=do_slot_action(crate::state::AppCtx(t.app.clone()),Path((1,"unlock".into())),headers(&t)).await;
    assert_eq!(r.status(),StatusCode::SEE_OTHER);assert!(t.app.db.slot_locks().unwrap().is_empty());
    assert!(p.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn slot_lock_revalidates_all_backers_and_blocks_stale_automatic_and_bulk_mutations() {
    let t=TestApp::new();let p=MockProvider::new().await;p.attach(&t.app);
    t.insert(11,&crate::db::now_iso());t.insert(12,&crate::db::now_iso());t.app.db.complete_stop(11).unwrap();
    crate::operations::set_slot_lock(&t.app,1,true,"test").await.unwrap();
    assert!(crate::operations::start_automatic_instance(&t.app,11,"stale start").await.is_err());
    assert!(crate::operations::change_automatic_bid(&t.app,12,1.1,"stale bid").await.is_err());
    for id in [11,12] {for op in ["stop","destroy"] {assert!(crate::operations::automatic_request(&t.app,id,op,"stale policy").await.is_err());}}
    assert!(crate::operations::flip_slot(&t.app,1,12,11,"stale flip").await.is_err());
    assert!(crate::reconciler::create_instance(&t.app,1,&Default::default(),praxis_common::Mode::Interruptible,1.0,60,Default::default(),true,"stale create").await.is_err());
    assert_eq!(crate::reconciler::destroy_all_instances(&t.app,None,"bulk").await.unwrap(),0);
    crate::reconciler::set_auto_rent(&t.app,false,"bulk sleep").await.unwrap();
    assert!(p.calls.lock().unwrap().is_empty());
    assert!(t.app.db.pending_operations().unwrap().is_empty());
}

#[tokio::test]
async fn lock_during_offer_search_prevents_the_already_planned_rental() {
    use std::sync::{Arc,atomic::{AtomicBool,AtomicUsize,Ordering}};
    let t=TestApp::new();
    let entered=Arc::new(tokio::sync::Notify::new());let release=Arc::new(tokio::sync::Notify::new());
    let first=Arc::new(AtomicBool::new(true));let mutations=Arc::new(AtomicUsize::new(0));
    let (e,r,f,m)=(entered.clone(),release.clone(),first.clone(),mutations.clone());
    let server=crate::test_support::LocalServer::new(axum::Router::new().fallback(move |axum::Json(q):axum::Json<serde_json::Value>| {
        let (e,r,f,m)=(e.clone(),r.clone(),f.clone(),m.clone());
        async move {
            if q.get("type").is_some() {
                if f.swap(false,Ordering::SeqCst) {e.notify_one();r.notified().await;}
                axum::Json(serde_json::json!({"offers":[{"id":7,"machine_id":7,"gpu_name":"RTX 3090","gpu_ram":24576,"cpu_ram":65536,"num_gpus":1,"disk_space":120,"inet_down":1000,"reliability2":0.99,"geolocation":"DE","min_bid":0.1,"dph_base":0.2,"dph_total":0.21,"storage_total_cost":0.01,"storage_cost":0.12}]}))
            } else {m.fetch_add(1,Ordering::SeqCst);axum::Json(serde_json::json!({"success":true,"new_contract":999}))}
        }
    })).await;
    *t.app.vast.lock().unwrap()=Some(praxis_vast::Vast::with_api_root("fake-key",&server.url()).unwrap());
    let app=t.app.clone();
    let task=tokio::spawn(async move {crate::reconciler::apply_action(&app,&praxis_common::Action::Create {
        slot_id:1,offer_id:7,mode:praxis_common::Mode::Interruptible,price_usd_h:Some(0.1),disk_gb:Some(60),reason:"pool fill".into(),
    }).await});
    tokio::time::timeout(std::time::Duration::from_secs(5),entered.notified()).await.unwrap();
    crate::operations::set_slot_lock(&t.app,1,true,"button while search in flight").await.unwrap();release.notify_one();
    let error=task.await.unwrap().unwrap_err();assert!(error.to_string().contains("pin/lock"),"{error}");
    assert_eq!(mutations.load(Ordering::SeqCst),0);assert!(t.app.db.instances(true).is_empty());
}

#[tokio::test]
async fn lock_suspends_failed_automatic_retries_but_explicit_instance_stop_can_override_it() {
    let t=TestApp::new();let p=MockProvider::new().await;p.attach(&t.app);t.insert(11,&crate::db::now_iso());
    p.respond(503,"temporary failure");
    assert!(crate::operations::automatic_request(&t.app,11,"stop","idle").await.is_err());
    assert!(t.app.db.pending_respects_lock(11).unwrap());
    crate::operations::set_slot_lock(&t.app,1,true,"test").await.unwrap();p.respond(200,r#"{"success":true}"#);
    crate::operations::retry_pending(&t.app).await.unwrap();assert_eq!(p.calls.lock().unwrap().len(),1);
    crate::operations::stop_instance(&t.app,11,"explicit user stop").await.unwrap();
    assert_eq!(p.calls.lock().unwrap().len(),2);assert_eq!(t.app.db.instance(11).unwrap().state,"stopped");
    assert!(t.app.db.slot_locks().unwrap().contains(&1));
}

#[tokio::test]
async fn unlock_preserves_independent_pins_and_unpin_does_not_unlock_the_slot() {
    let t=TestApp::new();t.insert(11,&crate::db::now_iso());t.insert(12,&crate::db::now_iso());
    t.app.db.update_instance_pinned(11,true).unwrap();
    for action in ["lock","unlock"] {
        let r=crate::api::instance_action(crate::state::AppCtx(t.app.clone()),Path((12,action.into())),t.request("{}")).await;
        assert!(r.status().is_success());assert!(t.app.db.instance(11).unwrap().pinned);
    }
    crate::operations::set_slot_lock(&t.app,1,true,"test").await.unwrap();
    let r=crate::api::instance_action(crate::state::AppCtx(t.app.clone()),Path((11,"unpin".into())),t.request("{}")).await;
    assert!(r.status().is_success());assert!(!t.app.db.instance(11).unwrap().pinned);assert!(t.app.db.slot_locks().unwrap().contains(&1));
}

#[tokio::test]
async fn failed_lock_persistence_and_cross_origin_requests_are_not_silent_success() {
    let t=TestApp::new();let p=MockProvider::new().await;p.attach(&t.app);
    let conn=rusqlite::Connection::open(t.dir.join("test.sqlite")).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_lock BEFORE UPDATE OF locked ON slots BEGIN SELECT RAISE(ABORT,'simulated disk failure'); END;").unwrap();
    let r=do_slot_action(crate::state::AppCtx(t.app.clone()),Path((1,"lock".into())),headers(&t)).await;
    assert_eq!(r.status(),StatusCode::INTERNAL_SERVER_ERROR);assert!(t.app.db.slot_locks().unwrap().is_empty());
    let mut h=axum::http::HeaderMap::new();
    h.insert("cookie",format!("pgpu_session={}",t.app.cfg().router_token()).parse().unwrap());
    h.insert("sec-fetch-site","cross-site".parse().unwrap());
    let r=do_slot_action(crate::state::AppCtx(t.app.clone()),Path((1,"lock".into())),h).await;
    assert_eq!(r.status(),StatusCode::SEE_OTHER);assert_eq!(r.headers()["location"],"/login");
    assert!(p.calls.lock().unwrap().is_empty());
}
