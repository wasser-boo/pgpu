//! Dashboard: askama + HTMX, Cookie-Session (Token), Form-POSTs → API-Logik.

use crate::state::AppCtx;
use crate::state::SharedApp;
use askama::Template;
use axum::body::Body;
use axum::extract::multipart::Multipart;
use axum::extract::{Form, Path, Query, Request};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use chrono::Timelike;
use std::collections::HashMap;

pub mod filters {
    pub fn fmt2(v: &f64) -> askama::Result<String> {
        Ok(format!("{v:.2}"))
    }
    pub fn fmt1(v: &f64) -> askama::Result<String> {
        Ok(format!("{v:.1}"))
    }
    pub fn fmt4(v: &f64) -> askama::Result<String> {
        Ok(format!("{v:.4}"))
    }
    pub fn fmt3(v: &f64) -> askama::Result<String> {
        Ok(format!("{v:.3}"))
    }
    pub fn fmt0(v: &f64) -> askama::Result<String> {
        Ok(format!("{v:.0}"))
    }
}

// ---------------------------------------------------------------- Auth

fn token_of(app: &SharedApp) -> String {
    app.cfg.router_token()
}

pub fn session_ok(app: &SharedApp, headers: &axum::http::HeaderMap) -> bool {
    let token = token_of(app);
    if token.is_empty() {
        return true;
    }
    let cookies = headers.get_all(axum::http::header::COOKIE);
    for c in cookies.iter() {
        if let Ok(s) = c.to_str() {
            for part in s.split(';') {
                let part = part.trim();
                if let Some(v) = part.strip_prefix("pgpu_session=") {
                    if v == token {
                        return true;
                    }
                }
            }
        }
    }
    // Bearer-Alternative für API-Clients:
    if let Some(t) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        if t == token {
            return true;
        }
    }
    false
}

pub async fn login_page() -> Response {
    Html(r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>pgpu Login</title>
    <style>body{background:#101418;color:#dbe2ea;font:14px system-ui;display:flex;align-items:center;justify-content:center;height:100vh;margin:0}
    form{background:#1a2027;padding:24px;border-radius:10px;border:1px solid #2a323c;display:flex;flex-direction:column;gap:10px}
    input{background:#0d1116;color:#dbe2ea;border:1px solid #2a323c;border-radius:6px;padding:8px}
    button{background:#22303c;color:#dbe2ea;border:1px solid #2a323c;border-radius:6px;padding:8px;cursor:pointer}</style></head>
    <body><form method="post" action="/login"><b>pgpu — Router-Token</b><input name="token" type="password" autofocus><button>Login</button></form></body></html>"#)
        .into_response()
}

pub async fn login_submit(Form(form): Form<HashMap<String, String>>) -> Response {
    let token = form.get("token").cloned().unwrap_or_default();
    let resp = Redirect::to("/").into_response();
    if token.is_empty() {
        return resp;
    }
    let mut r = Response::from(resp);
    let cookie = format!("pgpu_session={token}; Path=/; HttpOnly; SameSite=Lax");
    r.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_str(&cookie).unwrap(),
    );
    r
}

trait GuardHeaders {
    fn headers(&self) -> &axum::http::HeaderMap;
}
impl GuardHeaders for Request {
    fn headers(&self) -> &axum::http::HeaderMap {
        Request::headers(self)
    }
}
impl GuardHeaders for axum::http::HeaderMap {
    fn headers(&self) -> &axum::http::HeaderMap {
        self
    }
}
struct ReqOf<'a>(&'a axum::http::HeaderMap);
impl GuardHeaders for ReqOf<'_> {
    fn headers(&self) -> &axum::http::HeaderMap {
        self.0
    }
}

macro_rules! page_guard {
    ($app:expr, $guard_arg:expr) => {
        if !session_ok(&$app.0, $guard_arg.headers()) {
            return Redirect::to("/login").into_response();
        }
    };
}

// ---------------------------------------------------------------- Modelle

pub struct SlotView {
    pub id: i64,
    pub name: String,
    pub vast_id: i64,
    pub gpu_name: String,
    pub state: Option<StateView>,
    pub healthy: bool,
    pub pct: f64,
    pub bid: f64,
    pub min_bid: f64,
    pub in_flight: u32,
    pub busy: bool,
    pub busy_reason: String,
    pub cost_today: f64,
    pub pinned: bool,
}

pub struct StateView {
    pub label: String,
    pub class: &'static str,
}

pub struct BudgetView {
    pub spent_usd: f64,
    pub projected_usd: f64,
    pub spent_month_usd: f64,
    pub soft_usd: f64,
    pub hard_usd: f64,
    pub monthly_usd: f64,
    pub pct: f64,
    pub bar_class: &'static str,
    pub daily_soft_eur: f64,
    pub daily_hard_eur: f64,
    pub monthly_eur: f64,
    pub usd_per_eur: f64,
    pub tz: String,
}

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTpl {
    pub slots: Vec<SlotView>,
    pub budget: BudgetView,
    pub events: Vec<EventView>,
    pub stt_sessions: u32,
}

pub struct EventView {
    pub kind: String,
    pub ts: String,
    pub reason: String,
}

#[derive(Template)]
#[template(path = "offers.html")]
pub struct OffersTpl {
    pub slot_names: Vec<(i64, String)>,
    pub slot_id: i64,
    pub mode: String,
    pub offers: Vec<OfferRow>,
    pub default_disk_gb: i64,
    pub machines: Vec<crate::db::MachineStatRow>,
    pub error: Option<String>,
}

pub struct OfferRow {
    pub id: i64,
    pub machine_id: i64,
    pub machine_fails: i64,
    pub blacklisted: bool,
    pub gpu_name: String,
    pub cpu_ram_gb: f64,
    pub disk_gb: f64,
    pub inet_down: f64,
    pub reliability2: f64,
    pub min_bid: f64,
    pub dph_total: f64,
    pub storage_cost: f64,
}

#[derive(Template)]
#[template(path = "instance.html")]
pub struct InstanceTpl {
    pub inst: InstView,
    pub state: String,
    pub state_class: String,
    pub events: Vec<EventView>,
    pub logs: Option<String>,
    pub cmd_output: Option<String>,
}

pub struct InstView {
    pub vast_id: i64,
    pub slot_id: i64,
    pub role: String,
    pub gpu_name: String,
    pub image: String,
    pub nb_ip: String,
    pub mode: String,
    pub actual_status: String,
    pub intended_status: String,
    pub bid_usd_h: f64,
    pub min_bid: f64,
    pub busy: bool,
    pub busy_reason: String,
    pub lifecycle: String,
    pub pinned: bool,
}

#[derive(Template)]
#[template(path = "terminal.html")]
pub struct TerminalTpl {
    pub vast_id: i64,
}

#[derive(Template)]
#[template(path = "schedules.html")]
pub struct SchedulesTpl {
    pub instances: Vec<SchedView>,
}

pub struct SchedView {
    pub vast_id: i64,
    pub slot_id: i64,
    pub role: String,
    pub mode: String,
    pub lifecycle: String,
    pub warm_hours: String,
    pub stop_after_s: i64,
    pub destroy_after_s: i64,
    pub state: String,
}

#[derive(Template)]
#[template(path = "assets.html")]
pub struct AssetsTpl {
    pub groups: Vec<(String, Vec<AssetRow>)>,
}

pub struct AssetRow {
    pub id: String,
    pub target: String,
    pub size: u64,
}

#[derive(Template)]
#[template(path = "settings.html")]
pub struct SettingsTpl {
    pub budget: BudgetView,
    pub limits: praxis_policy::LimitsConfig,
    pub slots: Vec<SlotSettingView>,
    pub routing: RoutingView,
}

pub struct SlotSettingView {
    pub id: i64,
    pub ceiling: f64,
    pub margin: f64,
    pub stop_after_s: i64,
    pub destroy_after_s: i64,
    pub on_preempt: bool,
    pub on_bid_pressure: bool,
    pub optimize_cost: bool,
}

pub struct RoutingView {
    pub bind_ip: String,
    pub dashboard_port: u16,
    pub passthrough: Vec<(u16, i64, String)>,
    pub stt: String,
    pub vast_ok: bool,
}

// ---------------------------------------------------------------- State-JSON (API)

pub fn state_json(app: &SharedApp) -> serde_json::Value {
    let mut slots = Vec::new();
    for s in &app.cfg.slots {
        let active = app.db.active_instance(s.id);
        // Ohne aktive Instanz: jüngste lebende Box des Slots zeigen
        // (Warmup/Download-Progress), sonst bleibt state/progress null.
        let fallback = app
            .db
            .instances(false)
            .into_iter()
            .filter(|r| r.slot_id == s.id)
            .max_by_key(|r| r.vast_id);
        let shown = active.or_else(|| fallback.map(|r| r.vast_id));
        let inst = shown.and_then(|v| app.db.instance(v));
        let traffic = app.traffic.snapshot(s.id);
        let hb = shown.and_then(|v| app.hub.heartbeat(v));
        slots.push(serde_json::json!({
            "id": s.id,
            "role": s.role,
            "name": s.name,
            "desired_running": app.db.slot_desired(s.id),
            "active_instance": active,
            "healthy": inst.as_ref().map(|i| i.healthy).unwrap_or(false),
            "state": inst.as_ref().map(|i| i.state.clone()),
            "busy": inst.as_ref().map(|i| i.busy).unwrap_or(false),
            "in_flight": traffic.in_flight,
            "progress": hb.map(|h| h.progress_json.clone()),
        }));
    }
    serde_json::json!({
        "slots": slots,
        "budget": crate::api::budget_json(app),
        "stt_sessions": app.stt_sessions.load(std::sync::atomic::Ordering::Relaxed),
    })
}

fn budget_view(app: &SharedApp) -> BudgetView {
    let soft = app.cfg.budget.daily_soft_eur * app.cfg.budget.usd_per_eur;
    let hard = app.cfg.budget.daily_hard_eur * app.cfg.budget.usd_per_eur;
    let monthly = app.cfg.budget.monthly_eur * app.cfg.budget.usd_per_eur;
    let date = crate::node::local_date(app);
    let spent = app.db.spent_today(&date);
    // Projektion: verbraucht + aktuelle Rates bis Tagesende.
    let mut rate = 0.0;
    for i in app.db.instances(false) {
        if i.actual_status == "running" {
            rate += match i.mode {
                praxis_common::Mode::Interruptible => i.bid_usd_h,
                _ => i.dph_total,
            };
        }
        rate += i.storage_usd_h;
    }
    let now_local = chrono::Utc::now().with_timezone(&tz_of(app));
    let hours = 24.0 - now_local.time().hour() as f64 - now_local.time().minute() as f64 / 60.0;
    let projected = spent + rate * hours;
    let pct = (projected / soft.max(0.01) * 100.0).min(100.0);
    BudgetView {
        spent_usd: spent,
        projected_usd: projected,
        spent_month_usd: app.db.spent_month(&date[..7]),
        soft_usd: soft,
        hard_usd: hard,
        monthly_usd: monthly,
        pct,
        bar_class: if projected >= hard { "hard" } else if projected >= soft { "warn" } else { "" },
        daily_soft_eur: app.cfg.budget.daily_soft_eur,
        daily_hard_eur: app.cfg.budget.daily_hard_eur,
        monthly_eur: app.cfg.budget.monthly_eur,
        usd_per_eur: app.cfg.budget.usd_per_eur,
        tz: app.cfg.router.tz.clone(),
    }
}

fn tz_of(app: &SharedApp) -> chrono_tz::Tz {
    app.cfg.router.tz.parse().unwrap_or(chrono_tz::Europe::Berlin)
}

// ---------------------------------------------------------------- Seiten

pub async fn index(app: AppCtx, req: Request) -> Response {
    page_guard!(app, req);
    let mut slots = Vec::new();
    for s in &app.cfg.slots {
        let active = app.db.active_instance(s.id);
        let inst = active.and_then(|v| app.db.instance(v));
        let traffic = app.traffic.snapshot(s.id);
        let hb = active.and_then(|v| app.hub.heartbeat(v));
        let pct = hb
            .as_ref()
            .and_then(|h| h.progress_json.get("pct").and_then(|p| p.as_f64()))
            .unwrap_or(-1.0);
        let pinned = app.db.slot_pins().into_iter().any(|(sid, p)| sid == s.id && p.is_some());
        let state_view = inst.as_ref().map(|i| StateView {
            label: i.state.clone(),
            class: match i.state.as_str() {
                "healthy" => "b-ok",
                "preempted" | "unreachable" | "failed" => "b-bad",
                "stopped" => "b-mut",
                _ => "b-info",
            },
        });
        slots.push(SlotView {
            id: s.id,
            name: s.name.clone(),
            vast_id: active.unwrap_or(0),
            gpu_name: inst.as_ref().map(|i| i.gpu_name.clone()).unwrap_or_default(),
            state: state_view,
            healthy: inst.as_ref().map(|i| i.healthy).unwrap_or(false),
            pct,
            bid: inst.as_ref().map(|i| i.bid_usd_h).unwrap_or(0.0),
            min_bid: inst.as_ref().map(|i| i.min_bid).unwrap_or(0.0),
            in_flight: traffic.in_flight,
            busy: inst.as_ref().map(|i| i.busy).unwrap_or(false),
            busy_reason: inst.as_ref().map(|i| i.busy_reason.clone()).unwrap_or_default(),
            cost_today: 0.0,
            pinned,
        });
    }
    let events = app
        .db
        .events(40, None)
        .into_iter()
        .map(|e| EventView { kind: e.kind, ts: e.ts, reason: e.reason })
        .collect();
    let tpl = IndexTpl {
        slots,
        budget: budget_view(&app.0),
        events,
        stt_sessions: app.stt_sessions.load(std::sync::atomic::Ordering::Relaxed),
    };
    Html(tpl.render().unwrap_or_default()).into_response()
}

pub async fn offers_page(app: AppCtx, Query(q): Query<HashMap<String, String>>, req: Request) -> Response {
    page_guard!(app, req);
    let slot_id = q.get("slot").and_then(|s| s.parse::<i64>().ok()).unwrap_or_else(|| app.cfg.slots.first().map(|s| s.id).unwrap_or(1));
    let mode = q.get("mode").cloned().unwrap_or_else(|| "interruptible".into());
    let (offers, error) = match crate::reconciler::search_slot_offers(&app.0, slot_id, true).await {
        Ok(o) => (o, None),
        Err(e) => (vec![], Some(format!("{e}"))),
    };
    let machines = app.db.machine_stats();
    let stats: std::collections::HashMap<i64, crate::db::MachineStatRow> =
        machines.iter().map(|m| (m.machine_id, m.clone())).collect();
    let rows: Vec<OfferRow> = offers
        .iter()
        .take(30)
        .map(|o| {
            let stat = stats.get(&o.machine_id);
            OfferRow {
                id: o.id,
                machine_id: o.machine_id,
                machine_fails: stat.map(|s| s.fails).unwrap_or(0),
                blacklisted: stat.map(|s| s.blacklisted).unwrap_or(false),
                gpu_name: o.gpu_name.clone(),
                cpu_ram_gb: o.cpu_ram_gb,
                disk_gb: o.disk_gb,
                inet_down: o.inet_down,
                reliability2: o.reliability2,
                min_bid: o.min_bid,
                dph_total: o.dph_total,
                storage_cost: o.storage_cost,
            }
        })
        .collect();
    let slot_names = app.cfg.slots.iter().map(|s| (s.id, s.name.clone())).collect();
    let tpl = OffersTpl {
        slot_names,
        slot_id,
        mode,
        offers: rows,
        default_disk_gb: app.cfg.slot(slot_id).map(|s| s.disk_gb).unwrap_or(60),
        machines: machines.into_iter().take(12).collect(),
        error,
    };
    Html(tpl.render().unwrap_or_default()).into_response()
}

/// Blacklist-Knopf aus dem Dashboard: `/do/machines/:id/:action`.
pub async fn do_machine_action(app: AppCtx, Path((machine_id, action)): Path<(i64, String)>, headers: axum::http::HeaderMap) -> Response {
    page_guard!(app, ReqOf(&headers));
    let (set, note) = match action.as_str() {
        "blacklist" => (true, "manuell blacklisted (Dashboard)"),
        "unblacklist" => (false, ""),
        _ => return (StatusCode::BAD_REQUEST, "unknown action").into_response(),
    };
    if app.db.machine_stat(machine_id).is_none() && !set {
        return (StatusCode::NOT_FOUND, "Maschine unbekannt").into_response();
    }
    let _ = app.db.set_machine_blacklist(machine_id, set, note);
    app.events.emit(
        &app.0.db,
        if set { "machine_blacklisted" } else { "machine_unblacklisted" },
        None,
        None,
        &format!("Host {machine_id} {} (Dashboard)", if set { "blacklisted" } else { "von Blacklist entfernt" }),
        &serde_json::json!({"machine_id": machine_id}),
    );
    Redirect::to("/offers").into_response()
}

fn inst_view(app: &SharedApp, row: &crate::db::InstanceRow) -> InstView {
    InstView {
        vast_id: row.vast_id,
        slot_id: row.slot_id,
        role: crate::api::role_str(row.role).to_string(),
        gpu_name: row.gpu_name.clone(),
        image: row.image.clone(),
        nb_ip: row.nb_ip.clone().unwrap_or_else(|| app.hub.nb_ip(row.vast_id).unwrap_or_default()),
        mode: row.mode.to_string(),
        actual_status: row.actual_status.clone(),
        intended_status: row.intended_status.clone(),
        bid_usd_h: row.bid_usd_h,
        min_bid: row.min_bid,
        busy: row.busy,
        busy_reason: row.busy_reason.clone(),
        lifecycle: row.lifecycle.clone(),
        pinned: row.pinned,
    }
}

pub async fn instance_page(app: AppCtx, Path(vast_id): Path<i64>, method: axum::http::Method, headers: axum::http::HeaderMap) -> Response {
    page_guard!(app, ReqOf(&headers));
    let Some(row) = app.db.instance(vast_id) else {
        return (StatusCode::NOT_FOUND, "Instanz unbekannt").into_response();
    };
    let fetch_logs = method == axum::http::Method::POST;
    let logs = if fetch_logs {
        let r = crate::api::instance_logs(AppCtx(app.0.clone()), axum::extract::Path(vast_id), Request::builder().method(axum::http::Method::GET).body(Body::empty()).unwrap()).await;
        let body = r.into_body();
        match axum::body::to_bytes(body, 4 * 1024 * 1024).await {
            Ok(bytes) => Some(String::from_utf8_lossy(&bytes).to_string()),
            Err(_) => None,
        }
    } else {
        None
    };
    let events = app
        .db
        .events(30, Some(vast_id))
        .into_iter()
        .map(|e| EventView { kind: e.kind, ts: e.ts, reason: e.reason })
        .collect();
    let tpl = InstanceTpl {
        inst: inst_view(&app.0, &row),
        state: row.state.clone(),
        state_class: match row.state.as_str() {
            "healthy" => "b-ok".into(),
            "preempted" | "unreachable" | "failed" => "b-bad".into(),
            "stopped" | "destroyed" => "b-mut".into(),
            _ => "b-info".into(),
        },
        events,
        logs,
        cmd_output: None,
    };
    Html(tpl.render().unwrap_or_default()).into_response()
}

pub async fn terminal_page(app: AppCtx, Path(vast_id): Path<i64>, req: Request) -> Response {
    page_guard!(app, req);
    Html(TerminalTpl { vast_id }.render().unwrap_or_default()).into_response()
}

pub async fn schedules_page(app: AppCtx, req: Request) -> Response {
    page_guard!(app, req);
    let mut instances = Vec::new();
    for row in app.db.instances(true) {
        let slot = app.cfg.slot(row.slot_id);
        instances.push(SchedView {
            vast_id: row.vast_id,
            slot_id: row.slot_id,
            role: crate::api::role_str(row.role).to_string(),
            mode: row.mode.to_string(),
            lifecycle: row.lifecycle.clone(),
            warm_hours: slot.and_then(|s| s.warm_hours.clone()).unwrap_or_else(|| "—".into()),
            stop_after_s: slot.map(|s| s.idle_cfg(row.role).stop_after_s).unwrap_or(0),
            destroy_after_s: slot.map(|s| s.idle_cfg(row.role).destroy_after_stopped_s).unwrap_or(0),
            state: row.state.clone(),
        });
    }
    Html(SchedulesTpl { instances }.render().unwrap_or_default()).into_response()
}

pub async fn assets_page(app: AppCtx, req: Request) -> Response {
    page_guard!(app, req);
    let mut groups = Vec::new();
    for role in ["all", "llm", "media"] {
        let m = crate::assets::manifest_for_role(&app.0, role);
        groups.push((
            role.to_string(),
            m.assets
                .into_iter()
                .map(|a| AssetRow { id: a.id, target: a.target, size: a.size })
                .collect(),
        ));
    }
    Html(AssetsTpl { groups }.render().unwrap_or_default()).into_response()
}

pub async fn settings_page(app: AppCtx, req: Request) -> Response {
    page_guard!(app, req);
    let slots = app
        .cfg
        .slots
        .iter()
        .map(|s| SlotSettingView {
            id: s.id,
            ceiling: s.bid.ceiling_usd_h,
            margin: s.bid.margin,
            stop_after_s: s.idle_cfg(s.role).stop_after_s,
            destroy_after_s: s.idle_cfg(s.role).destroy_after_stopped_s,
            on_preempt: s.swap.on_preempt,
            on_bid_pressure: s.swap.on_bid_pressure,
            optimize_cost: s.swap.optimize_cost,
        })
        .collect();
    let mut passthrough = Vec::new();
    for s in &app.cfg.slots {
        for (port, service) in s.passthrough_ports() {
            passthrough.push((port, s.id, service));
        }
    }
    passthrough.sort_by_key(|(p, _, _)| *p);
    let tpl = SettingsTpl {
        budget: budget_view(&app.0),
        limits: app.cfg.limits.clone(),
        slots,
        routing: RoutingView {
            bind_ip: app.cfg.router.bind_ip.clone(),
            dashboard_port: app.cfg.router.dashboard_port,
            passthrough,
            stt: format!("{} ({})", app.cfg.stt.mode, app.cfg.stt.url),
            vast_ok: !app.cfg.vast_api_key().is_empty(),
        },
    };
    Html(tpl.render().unwrap_or_default()).into_response()
}

// ---------------------------------------------------------------- Form-Actions

async fn api_slot(app: &SharedApp, slot_id: i64, action: &str) {
    let req = Request::builder()
        .method(axum::http::Method::POST)
        .uri("/")
        .body(Body::empty())
        .unwrap();
    let _ = crate::api::slot_action(AppCtx(app.clone()), axum::extract::Path((slot_id, action.to_string())), req).await;
}

pub async fn do_slot_action(app: AppCtx, Path((slot_id, action)): Path<(i64, String)>, headers: axum::http::HeaderMap) -> Response {
    page_guard!(app, ReqOf(&headers));
    api_slot(&app.0, slot_id, &action).await;
    Redirect::to("/").into_response()
}

pub async fn do_rent(app: AppCtx, headers: axum::http::HeaderMap, Form(form): Form<HashMap<String, String>>) -> Response {
    page_guard!(app, ReqOf(&headers));
    let offer_id = form.get("offer_id").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let slot_id = form.get("slot_id").and_then(|v| v.parse::<i64>().ok()).unwrap_or(1);
    let mode = match form.get("mode").map(|s| s.as_str()) {
        Some("on_demand") => praxis_common::Mode::OnDemand,
        _ => praxis_common::Mode::Interruptible,
    };
    let disk_gb = form.get("disk_gb").and_then(|v| v.parse::<i64>().ok());
    let body = serde_json::json!({
        "offer_id": offer_id,
        "slot_id": slot_id,
        "mode": mode,
        "disk_gb": disk_gb,
    });
    let api_req = Request::builder()
        .method(axum::http::Method::POST)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {}", token_of(&app.0)))
        .uri("/api/v1/instances")
        .body(Body::from(body.to_string()))
        .unwrap();
    match crate::api::instance_create(AppCtx(app.0.clone()), api_req).await {
        resp => {
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap_or_default();
            if status.is_success() {
                let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
                if let Some(vast_id) = v.get("vast_id").and_then(|i| i.as_i64()) {
                    return Redirect::to(&format!("/instances/{vast_id}")).into_response();
                }
            }
            let _ = app.db.add_event(
                "rent_failed",
                Some(slot_id),
                None,
                &String::from_utf8_lossy(&bytes),
                &serde_json::json!({"status": status.as_u16()}),
            );
            Redirect::to("/").into_response()
        }
    }
}

pub async fn do_instance_action(
    app: AppCtx,
    Path((vast_id, action)): Path<(i64, String)>,
    headers: axum::http::HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    page_guard!(app, ReqOf(&headers));
    let back = format!("/instances/{vast_id}");
    let mut payload = serde_json::json!({});
    match action.as_str() {
        "bid" => {
            let price = form.get("price").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
            payload = serde_json::json!({"price": price});
        }
        "mode" => {
            payload = serde_json::json!({"mode": form.get("mode").cloned().unwrap_or_default()});
        }
        "lifecycle" => {
            let kind = form.get("kind").cloned().unwrap_or_else(|| "auto".into());
            let value = form.get("value").cloned().unwrap_or_default();
            let destroy = form.get("destroy").map(|v| v == "1" || v == "on").unwrap_or(false);
            let lc = match kind.as_str() {
                "ttl" => praxis_common::Lifecycle::Ttl { ttl_s: value.parse().unwrap_or(3600), destroy },
                "until" => praxis_common::Lifecycle::Until { until: value, destroy },
                "schedule" => praxis_common::Lifecycle::Schedule {
                    spec: value,
                    prewarm_s: 1200,
                    destroy,
                },
                "sleep" => praxis_common::Lifecycle::Sleep {
                    stop_after_idle_s: 1200,
                    resume_at: if value.is_empty() { None } else { Some(value) },
                    resume_after_s: None,
                },
                _ => praxis_common::Lifecycle::Auto,
            };
            payload = serde_json::json!({"lifecycle": lc});
        }
        "cmd" => {
            let command = form.get("command").cloned().unwrap_or_default();
            let service = form.get("service").cloned().unwrap_or_else(|| "llama-chat".into());
            let cmd_req = Request::builder()
                .method(axum::http::Method::POST)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .uri(format!("/api/v1/instances/{vast_id}/cmd"))
                .body(Body::from(serde_json::json!({"service": service}).to_string()))
                .unwrap();
            let result = crate::api::instance_cmd(
                AppCtx(app.0.clone()),
                axum::extract::Path((vast_id, command.clone())),
                cmd_req,
            )
            .await;
            // Seite mit Output neu rendern:
            let Some(row) = app.db.instance(vast_id) else {
                return Redirect::to(&back).into_response();
            };
            let events = app
                .db
                .events(30, Some(vast_id))
                .into_iter()
                .map(|e| EventView { kind: e.kind, ts: e.ts, reason: e.reason })
                .collect();
            let output = match result {
                r => {
                    let status = r.status();
                    let bytes = axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap_or_default();
                    Some(format!("[HTTP {}]\n{}", status.as_u16(), String::from_utf8_lossy(&bytes)))
                }
            };
            let tpl = InstanceTpl {
                inst: inst_view(&app.0, &row),
                state: row.state.clone(),
                state_class: "b-info".into(),
                events,
                logs: None,
                cmd_output: output,
            };
            return Html(tpl.render().unwrap_or_default()).into_response();
        }
        _ => {}
    }
    let body_str = payload.to_string();
    let api_req = Request::builder()
        .method(axum::http::Method::POST)
        .uri(format!("/api/v1/instances/{vast_id}/{action}"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body_str))
        .unwrap();
    let _ = crate::api::instance_action(AppCtx(app.0.clone()), axum::extract::Path((vast_id, action.clone())), api_req).await;
    Redirect::to(&back).into_response()
}

pub async fn do_asset_upload(
    app: AppCtx,
    Path(scope): Path<String>,
    headers: axum::http::HeaderMap,
    mut multipart: Multipart,
) -> Response {
    page_guard!(app, ReqOf(&headers));
    while let Some(field) = multipart.next_field().await.ok().flatten() {
        let name = field.file_name().unwrap_or("upload.bin").to_string();
        let data = field.bytes().await.unwrap_or_default();
        let up_req = Request::builder()
            .method(axum::http::Method::POST)
            .uri("/")
            .body(Body::from(data))
            .unwrap();
        let r = crate::assets::upload(app.0.clone(), scope.clone(), name.clone(), up_req).await;
        if r.status() != StatusCode::OK {
            return r;
        }
    }
    Redirect::to("/assets").into_response()
}