//! Dashboard: askama + HTMX, Cookie-Session (Token), Form-POSTs → API-Logik.

use crate::state::AppCtx;
use crate::state::SharedApp;
use askama::Template;
use axum::body::Body;
use axum::extract::multipart::Multipart;
use axum::extract::{Form, Path, Query, Request};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
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
    app.cfg().router_token()
}

pub fn session_ok(app: &SharedApp, headers: &axum::http::HeaderMap) -> bool {
    let token = token_of(app);
    if token.is_empty() {
        return false;
    }
    if crate::node::bearer(headers).as_deref() == Some(token.as_str()) {
        return true;
    }
    // Cookie-authenticated browser requests (including terminal WS) must not
    // originate from another site, even a sibling host inside the same VPN.
    if !same_origin(headers) { return false; }
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

fn same_origin(headers: &axum::http::HeaderMap) -> bool {
    if headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) == Some("cross-site") {
        return false;
    }
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else { return true; };
    let Some(host) = headers.get(axum::http::header::HOST).and_then(|v| v.to_str().ok()) else { return false; };
    let Ok(origin) = origin.to_str().unwrap_or("").parse::<url::Url>() else { return false; };
    let Ok(expected) = format!("{}://{host}", origin.scheme()).parse::<url::Url>() else { return false; };
    matches!(origin.scheme(), "http" | "https") && origin.origin() == expected.origin()
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

pub async fn login_submit(app: AppCtx, headers: axum::http::HeaderMap, Form(form): Form<HashMap<String, String>>) -> Response {
    if !same_origin(&headers) { return (StatusCode::FORBIDDEN, "cross-origin login denied").into_response(); }
    let token = form.get("token").cloned().unwrap_or_default();
    // Falscher Token: kein Cookie setzen — sonst endloser Login-Loop ohne
    // Fehlermeldung (Guard bounced ohnehin zurück).
    if token.is_empty() || token != token_of(&app.0) {
        return Html(r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>pgpu Login</title>
    <style>body{background:#101418;color:#dbe2ea;font:14px system-ui;display:flex;align-items:center;justify-content:center;height:100vh;margin:0}
    form{background:#1a2027;padding:24px;border-radius:10px;border:1px solid #2a323c;display:flex;flex-direction:column;gap:10px}
    input{background:#0d1116;color:#dbe2ea;border:1px solid #2a323c;border-radius:6px;padding:8px}
    button{background:#22303c;color:#dbe2ea;border:1px solid #2a323c;border-radius:6px;padding:8px;cursor:pointer}
    .err{color:#e07a5f}</style></head>
    <body><form method="post" action="/login"><b>pgpu — Router-Token</b><span class="err">Token falsch — nochmal versuchen.</span><input name="token" type="password" autofocus><button>Login</button></form></body></html>"#)
            .into_response();
    }
    let mut r = Response::from(Redirect::to("/").into_response());
    let cookie = format!("pgpu_session={token}; Path=/; HttpOnly; SameSite=Strict");
    let Ok(cookie) = axum::http::HeaderValue::from_str(&cookie) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "invalid token configuration").into_response();
    };
    r.headers_mut().insert(axum::http::header::SET_COOKIE, cookie);
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
    pub cost_today: Option<f64>,
    pub provider_cost_today: Option<f64>,
    pub pinned: bool,
    /// Ersatz-Box, die parallel zum aktiven Box wärmt (Hot-Swap).
    pub warming_id: i64,
    pub warming_state: String,
    /// Pool-Anzeige: "Pool: 2/3 warm · 1 kalt" (nur bei total > 1).
    pub pool_line: String,
}

pub struct StateView {
    pub label: String,
    pub class: &'static str,
}

pub struct BudgetView {
    pub costs_known: bool,
    pub spent_usd: f64,
    pub projected_usd: f64,
    pub spent_pct: f64,
    pub spent_bar_class: &'static str,
    pub rate_known: bool,
    pub current_rate: String,
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
    pub today: crate::billing::TodayCosts,
    pub slots: Vec<SlotView>,
    pub budget: BudgetView,
    pub events: Vec<EventView>,
    pub stt_sessions: u32,
    /// Auto-Miete-Schalter: aus = Router mietet/startet nie automatisch.
    pub auto_rent: bool,
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
    pub activate_blacklist: bool,
    pub activate_whitelist: bool,
    pub error: Option<String>,
}

pub struct OfferRow {
    pub id: i64,
    pub machine_id: i64,
    pub machine_fails: i64,
    pub blacklisted: bool,
    pub whitelisted: bool,
    pub eligible: bool,
    pub rejection: String,
    pub score: String,
    pub gpu_ram_gb: f64,
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
    pub storage_usd_h: f64,
    pub total_usd_h: f64,
    pub min_bid: f64,
    pub busy: bool,
    pub busy_reason: String,
    pub lifecycle: String,
    pub pinned: bool,
    pub slot_locked: bool,
    /// Letzter Agent-Health ("healthy" oder Degraded-Grund, z. B.
    /// "11434 → 503") — sagt, warum die Box (noch) nicht serviert.
    pub agent_health: String,
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
    /// Upload-Feedback (?ok=<name> nach do_asset_upload)
    pub ok: Option<String>,
}

pub struct AssetRow {
    pub id: String,
    pub target: String,
    pub size: u64,
    pub mode: String,
    pub restart: String,
    pub required: bool,
}

#[derive(Template)]
#[template(path = "settings.html")]
pub struct SettingsTpl {
    pub budget: BudgetView,
    pub limits: praxis_policy::LimitsConfig,
    pub slots: Vec<SlotSettingView>,
    pub routing: RoutingView,
    pub webhook: WebhookView,
    pub billing: BillingView,
    /// Rohtext der config.toml — editierbar (Hot-Reload, kein Neustart).
    pub config_raw: String,
    /// Status-Message (Query-Param nach Save/Reload).
    pub config_msg: String,
}

pub struct BillingRowView {
    pub id: i64,
    pub slot: i64,
    pub state: String,
    pub provider: String,
    pub estimate: String,
}
pub struct BillingView {
    pub scope: String,
    pub status: String,
    pub failed: bool,
    pub timezone: String,
    pub synced: String,
    pub attempted: String,
    pub day: String,
    pub month: String,
    pub day_provider: String,
    pub day_estimate: String,
    pub month_provider: String,
    pub month_estimate: String,
    pub rows: Vec<BillingRowView>,
}
fn billing_view(app: &SharedApp) -> BillingView {
    let status=crate::billing::status(app);
    let snapshot=crate::billing::snapshot(app).ok().flatten();
    let money=|v:Option<f64>|v.map(|v|format!("{v:.4} USD")).unwrap_or_else(||"noch unbekannt".into());
    let error=status.get("error").or_else(||status["last_attempt"].get("message")).and_then(|v|v.as_str());
    // Also sanitize old persisted 0.26 errors until the first successful sync.
    let brief=error.map(|s|s.lines().next().unwrap_or(s).split('<').next().unwrap_or("").chars().take(240).collect::<String>());
    BillingView {
        scope:crate::slot_labels::scope(&app.cfg()).join(", "),
        status:brief.map(|e|format!("Abgleich fehlgeschlagen: {}. Letzte gültige Werte und Budgethistorie bleiben erhalten.",e.trim())).unwrap_or_else(||
            if snapshot.is_some() { "Charges erfolgreich eingelesen. Vast kann verzögert abrechnen.".into() } else { "Noch kein Abgleich; nach Start und anschließend stündlich im Hintergrund.".into() }),
        failed:error.is_some(),
        timezone:app.cfg().router.tz.clone(),
        attempted:status["last_attempt"]["at"].as_str().unwrap_or("—").into(),
        synced:snapshot.as_ref().map(|s|s.synced_at.clone()).unwrap_or_else(||"—".into()),
        day:snapshot.as_ref().map(|s|s.day.period.clone()).unwrap_or_else(||crate::node::local_date(app)),
        month:snapshot.as_ref().map(|s|s.month.period.clone()).unwrap_or_else(||crate::node::local_date(app)[..7].into()),
        day_provider:money(snapshot.as_ref().map(|s|s.day.provider_usd)),
        day_estimate:money(snapshot.as_ref().map(|s|s.day.estimated_usd)),
        month_provider:money(snapshot.as_ref().map(|s|s.month.provider_usd)),
        month_estimate:money(snapshot.as_ref().map(|s|s.month.estimated_usd)),
        rows:snapshot.as_ref().map(|s|s.month.rows.iter().map(|row|BillingRowView {
            id:row.instance_id,slot:row.slot_id,state:row.state.clone(),provider:money(row.provider_usd),estimate:money(row.estimated_usd),
        }).collect()).unwrap_or_default(),
    }
}

pub struct WebhookView {
    pub configured: bool,
    pub targets: Vec<WebhookTargetView>,
    pub last_event: String,
}
pub struct WebhookTargetView {
    pub number: usize,
    pub host: String,
    pub format: String,
}

pub struct SlotSettingView {
    pub id: i64,
    pub name: String,
    pub countries: String,
    pub excluded_countries: String,
    pub origin_country: String,
    pub radius_km: String,
    pub latitude: String,
    pub longitude: String,
    pub gpu_names: String,
    pub raw_query: String,
    pub raw_on_demand: String,
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
    let locks=app.db.slot_locks();
    let mut slots = Vec::new();
    for s in &app.cfg().slots {
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
        let hb = shown.filter(|_|!inst.as_ref().is_some_and(|i|matches!(i.state.as_str(),"start_requested"|"start_failed"|"scheduling"))).and_then(|v| app.hub.heartbeat(v));
        // Pool-Sicht: alle lebenden Instanzen des Slots (Multi-Instanz-
        // Slots, Badges/Routing-Transparenz für Praxis).
        let pool_cfg = app.cfg().slot(s.id).map(|c| c.pool).unwrap_or_default();
        let instances: Vec<serde_json::Value> = app
            .db
            .instances(false)
            .into_iter()
            .filter(|r| r.slot_id == s.id)
            .map(|r| {
                serde_json::json!({
                    "vast_id": r.vast_id,
                    "state": r.state,
                    "healthy": r.healthy,
                    "busy": r.busy,
                    "mode": r.mode.to_string(),
                })
            })
            .collect();
        let healthy_count = instances.iter().filter(|i| i["healthy"] == true).count();
        slots.push(serde_json::json!({
            "id": s.id,
            "role": s.role,
            "name": s.name,
            "desired_running": app.db.slot_desired(s.id),
            "locked": locks.as_ref().ok().map(|locks|locks.contains(&s.id)),
            "lock_status_known": locks.is_ok(),
            "active_instance": active,
            "healthy": inst.as_ref().map(|i| i.healthy).unwrap_or(false),
            "state": inst.as_ref().map(|i| i.state.clone()),
            "busy": inst.as_ref().map(|i| i.busy).unwrap_or(false),
            "in_flight": traffic.in_flight,
            "progress": hb.map(|h| h.progress_json.clone()),
            "pool": { "warm": pool_cfg.warm, "total": pool_cfg.total, "healthy_now": healthy_count, "live_now": instances.len() },
            "instances": instances,
        }));
    }
    serde_json::json!({
        "slots": slots,
        "budget": crate::api::budget_json(app),
        "stt_sessions": app.stt_sessions.load(std::sync::atomic::Ordering::Relaxed),
        "auto_rent": app.db.auto_rent_enabled(),
    })
}

#[cfg(test)]
mod price_tests;
#[cfg(test)]
mod tests_costs_locks;

fn budget_view(app: &SharedApp) -> BudgetView {budget_view_at(app,chrono::Utc::now())}

fn budget_view_at(app:&SharedApp,now:chrono::DateTime<chrono::Utc>)->BudgetView {
    let policy=crate::reconciler::effective_policy(app);
    let soft = policy.budget.daily_soft_eur * policy.budget.usd_per_eur;
    let hard = policy.budget.daily_hard_eur * policy.budget.usd_per_eur;
    let monthly = policy.budget.monthly_eur * policy.budget.usd_per_eur;
    let date = now.with_timezone(&tz_of(app)).format("%Y-%m-%d").to_string();
    let totals=app.db.budget_totals(&date);
    let costs_known=totals.is_ok();
    let (spent,spent_month)=totals.unwrap_or((0.0,0.0));
    // Same active/reserved compute + retained storage as admission and digests.
    let costs=app.db.try_instances(false).and_then(|rows|crate::costs::hourly(&rows));
    let rate_known=costs.is_ok() && costs_known;
    let current_rate=costs.as_ref().map(|c|format!("{:.4} USD/h (Miete {:.4} + Speicher {:.4})",c.total(),c.compute,c.storage)).unwrap_or_else(|_|"nicht verfügbar".into());
    let rate=costs.map(|c|c.total()).unwrap_or(0.0);
    let hours = crate::billing::seconds_to_day_end(now,tz_of(app)).unwrap_or(0) as f64 /3600.0;
    let projected = spent + rate * hours;
    let pct = (projected / soft.max(0.01) * 100.0).min(100.0);
    BudgetView {
        costs_known,
        spent_usd: spent,
        projected_usd: projected,
        spent_pct: (spent / soft.max(0.01) * 100.0).clamp(0.0, 100.0),
        spent_bar_class: if spent >= hard { "hard" } else if spent >= soft { "warn" } else { "" },
        rate_known,
        current_rate,
        spent_month_usd: spent_month,
        soft_usd: soft,
        hard_usd: hard,
        monthly_usd: monthly,
        pct,
        bar_class: if !rate_known { "warn" } else if projected >= hard { "hard" } else if projected >= soft { "warn" } else { "" },
        daily_soft_eur: crate::reconciler::effective_policy(app).budget.daily_soft_eur,
        daily_hard_eur: crate::reconciler::effective_policy(app).budget.daily_hard_eur,
        monthly_eur: app.cfg().budget.monthly_eur,
        usd_per_eur: app.cfg().budget.usd_per_eur,
        tz: app.cfg().router.tz.clone(),
    }
}

fn tz_of(app: &SharedApp) -> chrono_tz::Tz {
    app.cfg().router.tz.parse().unwrap_or(chrono_tz::Europe::Berlin)
}

// ---------------------------------------------------------------- Seiten

pub async fn index(app: AppCtx, req: Request) -> Response {
    page_guard!(app, req);
    let now=chrono::Utc::now();
    let today=crate::billing::today(&app.0,now);
    let locks=match app.db.slot_locks() {Ok(locks)=>locks,Err(_)=>return (StatusCode::INTERNAL_SERVER_ERROR,"Lock-Status nicht verfügbar").into_response()};
    let mut slots = Vec::new();
    for s in &app.cfg().slots {
        let active = app.db.active_instance(s.id);
        // Live-Boxen des Slots, die nicht die aktive sind: neueste = wärmende
        // Ersatz-Box (Hot-Swap) — bzw. ohne aktive Instanz die Anzeige-Box
        // (Warmup/Download-Progress), damit die Karte nicht blind „cold“ zeigt.
        let live_others: Vec<_> = app
            .db
            .instances(false)
            .into_iter()
            .filter(|r| r.slot_id == s.id && Some(r.vast_id) != active)
            .collect();
        let newest_other = live_others.iter().max_by_key(|r| r.vast_id);
        let shown = active.or_else(|| newest_other.map(|r| r.vast_id));
        let inst = shown.and_then(|v| app.db.instance(v));
        let traffic = app.traffic.snapshot(s.id);
        let hb = shown.filter(|_|!inst.as_ref().is_some_and(|i|matches!(i.state.as_str(),"start_requested"|"start_failed"|"scheduling"))).and_then(|v| app.hub.heartbeat(v));
        let pct = hb
            .as_ref()
            .and_then(|h| h.progress_json.get("pct").and_then(|p| p.as_f64()))
            .unwrap_or(-1.0);
        let pinned = locks.contains(&s.id);
        let state_view = inst.as_ref().map(|i| StateView {
            label: i.state.clone(),
            class: match i.state.as_str() {
                "healthy" => "b-ok",
                "preempted" | "unreachable" | "failed" | "start_failed" => "b-bad",
                "stopped" => "b-mut",
                _ => "b-info",
            },
        });
        slots.push(SlotView {
            id: s.id,
            name: s.name.clone(),
            vast_id: shown.unwrap_or(0),
            gpu_name: inst.as_ref().map(|i| i.gpu_name.clone()).unwrap_or_default(),
            state: state_view,
            healthy: inst.as_ref().map(|i| i.healthy).unwrap_or(false),
            pct,
            bid: inst.as_ref().map(|i| i.compute_usd_h()).unwrap_or(0.0),
            min_bid: inst.as_ref().map(|i| i.min_bid).unwrap_or(0.0),
            in_flight: traffic.in_flight,
            busy: inst.as_ref().map(|i| i.busy).unwrap_or(false),
            busy_reason: inst.as_ref().map(|i| i.busy_reason.clone()).unwrap_or_default(),
            cost_today: today.local.map(|_|today.local_slots.get(&s.id).copied().unwrap_or(0.0)),
            provider_cost_today: today.provider.map(|_|today.provider_slots.get(&s.id).copied().unwrap_or(0.0)),
            pinned,
            warming_id: if active.is_some() { newest_other.map(|r| r.vast_id).unwrap_or(0) } else { 0 },
            warming_state: if active.is_some() { newest_other.map(|r| r.state.clone()).unwrap_or_default() } else { String::new() },
            pool_line: {
                let pool = s.pool;
                if pool.total > 1 {
                    let live = live_others.len() + usize::from(active.is_some());
                    let healthy = app
                        .db
                        .instances(false)
                        .into_iter()
                        .filter(|r| r.slot_id == s.id && r.healthy && r.state == "healthy")
                        .count();
                    let cold = live.saturating_sub(healthy);
                    format!("Pool: {healthy}/{} warm · {cold} kalt · {live}/{} live", pool.warm, pool.total)
                } else {
                    String::new()
                }
            },
        });
    }
    let events = app
        .db
        .events(40, None)
        .into_iter()
        .map(|e| EventView { kind: e.kind, ts: e.ts, reason: e.reason })
        .collect();
    let tpl = IndexTpl {
        today,
        slots,
        budget: budget_view_at(&app.0,now),
        events,
        stt_sessions: app.stt_sessions.load(std::sync::atomic::Ordering::Relaxed),
        auto_rent: app.db.auto_rent_enabled(),
    };
    Html(tpl.render().unwrap_or_default()).into_response()
}

pub async fn offers_page(app: AppCtx, Query(q): Query<HashMap<String, String>>, req: Request) -> Response {
    page_guard!(app, req);
    let slot_id = q.get("slot").and_then(|s| s.parse::<i64>().ok()).unwrap_or_else(|| app.cfg().slots.first().map(|s| s.id).unwrap_or(1));
    let mode = q.get("mode").cloned().unwrap_or_else(|| "interruptible".into());
    let (offers, error) = match crate::reconciler::search_slot_offers(&app.0, slot_id, true).await {
        Ok(o) => (o, None),
        Err(e) => (vec![], Some(format!("{e}"))),
    };
    let machines = app.db.machine_stats();
    let stats: std::collections::HashMap<i64, crate::db::MachineStatRow> =
        machines.iter().map(|m| (m.machine_id, m.clone())).collect();
    let slot = app.cfg().slot(slot_id).cloned();
    let offer_mode = if mode == "on_demand" {praxis_common::Mode::OnDemand} else {praxis_common::Mode::Interruptible};
    let scores = slot.as_ref().and_then(|s| crate::performance::host_scores(&app.0, s).ok()).unwrap_or_default();
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
                whitelisted: stat.map(|s| s.whitelisted).unwrap_or(false),
                eligible: slot.as_ref().is_some_and(|s| crate::catalog::assess(&app.0,s,o,offer_mode,praxis_policy::eligibility::rental_price(o,offer_mode,&s.bid),s.disk_gb).eligible),
                rejection: slot.as_ref().map(|s| crate::catalog::assess(&app.0,s,o,offer_mode,praxis_policy::eligibility::rental_price(o,offer_mode,&s.bid),s.disk_gb).reasons.join("; ")).unwrap_or_default(),
                score: scores.get(&(o.machine_id,crate::performance::allocation_key(o,slot.as_ref().map(|s|s.disk_gb).unwrap_or(0)))).map(|s|format!("{s:.1}/100")).unwrap_or_else(||"—".into()),
                gpu_ram_gb: o.gpu_ram_gb,
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
    let slot_names = app.cfg().slots.iter().map(|s| (s.id, s.name.clone())).collect();
    let tpl = OffersTpl {
        slot_names,
        slot_id,
        mode,
        offers: rows,
        default_disk_gb: app.cfg().slot(slot_id).map(|s| s.disk_gb).unwrap_or(60),
        machines: machines.into_iter().take(200).collect(),
        activate_blacklist: app.cfg().vast.activate_blacklist,
        activate_whitelist: app.cfg().vast.activate_whitelist,
        error,
    };
    Html(tpl.render().unwrap_or_default()).into_response()
}

/// Blacklist-Knopf aus dem Dashboard: `/do/machines/:id/:action`.
pub async fn do_machine_action(app: AppCtx, Path((machine_id, action)): Path<(i64, String)>, headers: axum::http::HeaderMap) -> Response {
    page_guard!(app, ReqOf(&headers));
    if let Err(e) = crate::catalog::machine_action(&app.0, machine_id, &action).await {
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    Redirect::to("/offers").into_response()
}

#[derive(Template)]
#[template(path = "performance.html")]
struct PerformanceTpl { rows: Vec<PerformanceView> }
struct PerformanceView {
    machine: i64, gpu: String, profile: String, model: String, source: String,
    workload: String, allocation: String, image: String,
    samples: u64, successful: u64, decode: String, prefill: String, ttft: String,
    elapsed: String, read_mb: String, disk_speed: String, vram: String, score: String,
}
pub async fn performance_page(app: AppCtx, req: Request) -> Response {
    page_guard!(app, req);
    let catalogue = match app.db.performance_catalogue() {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR,e.to_string()).into_response(),
    };
    let cfg = app.cfg();
    let scores: std::collections::HashMap<_,_> = cfg.slots.iter().map(|s|(s.id,crate::performance::host_scores(&app.0,s).unwrap_or_default())).collect();
    let rows=catalogue.into_iter().take(500).map(|r| {
        let means=r.means();
        let fmt=|name: &str,scale:f64| means.get(name).map(|v|format!("{:.1}",v/scale)).unwrap_or_else(||"—".into());
        let score=cfg.slot(r.slot_id).filter(|s| r.source=="benchmark" && r.workload_key==crate::performance::workload_key(s,&s.image) && r.model==s.performance.benchmark.spec.model)
            .and_then(|_|scores.get(&r.slot_id)).and_then(|m|m.get(&(r.machine_id,r.allocation.clone())))
            .map(|s|format!("{s:.1}")).unwrap_or_else(||"—".into());
        PerformanceView { machine:r.machine_id,gpu:r.gpu_name,profile:r.profile,model:r.model,source:r.source,
            workload:r.workload_key.chars().take(12).collect(),allocation:r.allocation,image:r.image,samples:r.samples,successful:r.successful,
            decode:fmt("decode_tps",1.0),prefill:fmt("prefill_tps",1.0),ttft:fmt("ttft_ms",1.0),elapsed:fmt("elapsed_ms",1.0),
            read_mb:fmt("disk_read_bytes",1e6),disk_speed:fmt("disk_read_mbps",1.0),vram:fmt("vram_used_mb",1024.0),score }
    }).collect();
    match (PerformanceTpl {rows}).render() {
        Ok(html)=>Html(html).into_response(),
        Err(e)=>(StatusCode::INTERNAL_SERVER_ERROR,e.to_string()).into_response(),
    }
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
        bid_usd_h: row.compute_usd_h(),
        storage_usd_h: row.storage_usd_h,
        total_usd_h: row.compute_usd_h()+row.storage_usd_h,
        min_bid: row.min_bid,
        busy: row.busy,
        busy_reason: row.busy_reason.clone(),
        lifecycle: row.lifecycle.clone(),
        pinned: row.pinned,
        slot_locked: app.db.slot_locks().map(|locks|locks.contains(&row.slot_id)).unwrap_or(true),
        agent_health: if row.phase().awaiting_allocation() {"GPU-Zuweisung ausstehend; kein gültiger Ready-Status".into()} else {app
            .hub
            .heartbeat(row.vast_id)
            .map(|hb| hb.health_json.to_string())
            .unwrap_or_else(|| "—".into())},
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
            "preempted" | "unreachable" | "failed" | "start_failed" => "b-bad".into(),
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
    if app.db.instance(vast_id).is_none() {
        return (StatusCode::NOT_FOUND, "Instanz unbekannt").into_response();
    }
    Html(TerminalTpl { vast_id }.render().unwrap_or_default()).into_response()
}

pub async fn schedules_page(app: AppCtx, req: Request) -> Response {
    page_guard!(app, req);
    let mut instances = Vec::new();
    for row in app.db.instances(true) {
        let slot = app.cfg().slot(row.slot_id).cloned();
        instances.push(SchedView {
            vast_id: row.vast_id,
            slot_id: row.slot_id,
            role: crate::api::role_str(row.role).to_string(),
            mode: row.mode.to_string(),
            lifecycle: row.lifecycle.clone(),
            warm_hours: slot.as_ref().and_then(|s| s.warm_hours.clone()).unwrap_or_else(|| "—".into()),
            stop_after_s: slot.as_ref().map(|s| s.idle_cfg(row.role).stop_after_s).unwrap_or(0),
            destroy_after_s: slot.as_ref().map(|s| s.idle_cfg(row.role).destroy_after_stopped_s).unwrap_or(0),
            state: row.state.clone(),
        });
    }
    Html(SchedulesTpl { instances }.render().unwrap_or_default()).into_response()
}

pub async fn assets_page(app: AppCtx, req: Request) -> Response {
    page_guard!(app, req);
    // Upload-Feedback per Query-Param (?ok=<name> — do_asset_upload redirectet
    // hierher; Name ist auf [A-Za-z0-9.-_/] begrenzt, '/' wird als %2F encodiert).
    let ok = req
        .uri()
        .query()
        .and_then(|q| {
            q.split('&').find(|p| p.starts_with("ok=")).map(|p| {
                p[3..].replace("%2F", "/").replace("%2f", "/").replace('+', " ")
            })
        });
    let mut groups = Vec::new();
    for role in ["all", "llm", "media"] {
        let m = crate::assets::manifest_for_role(&app.0, role);
        groups.push((
            role.to_string(),
            m.assets
                .into_iter()
                .map(|a| AssetRow {
                    id: a.id,
                    target: a.target,
                    size: a.size,
                    mode: a.mode,
                    restart: a.restart,
                    required: a.required,
                })
                .collect(),
        ));
    }
    Html(AssetsTpl { groups, ok }.render().unwrap_or_default()).into_response()
}

pub async fn settings_page(app: AppCtx, Query(q): Query<HashMap<String, String>>, req: Request) -> Response {
    page_guard!(app, req);
    let config_msg = q.get("msg").cloned().unwrap_or_default();
    let slots = app
        .cfg()
        .slots
        .iter()
        .map(|s| SlotSettingView {
            id: s.id,
            name: s.name.clone(),
            countries: s.location.countries.join(", "),
            excluded_countries: s.location.excluded_countries.join(", "),
            origin_country: s.location.origin_country.clone(),
            radius_km: s.location.radius_km.map(|v|v.to_string()).unwrap_or_default(),
            latitude: s.location.latitude.map(|v|v.to_string()).unwrap_or_default(),
            longitude: s.location.longitude.map(|v|v.to_string()).unwrap_or_default(),
            gpu_names: s.requirements.gpu_names.join("\n"),
            raw_query: s.search_query.clone(),
            raw_on_demand: s.search_query_on_demand.clone().unwrap_or_else(||s.search_query.clone()),
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
    for s in &app.cfg().slots {
        for (port, service) in s.passthrough_ports() {
            passthrough.push((port, s.id, service));
        }
    }
    passthrough.sort_by_key(|(p, _, _)| *p);
    let webhook_targets = crate::webhook::targets(&app.cfg().alerts).unwrap_or_default();
    let tpl = SettingsTpl {
        budget: budget_view(&app.0),
        limits: app.cfg().limits.clone(),
        slots,
        routing: RoutingView {
            bind_ip: app.cfg().router.bind_ip.clone(),
            dashboard_port: app.cfg().router.dashboard_port,
            passthrough,
            stt: format!("{} ({})", app.cfg().stt.mode, app.cfg().stt.url),
            vast_ok: !app.cfg().vast_api_key().is_empty(),
        },
        webhook: WebhookView {
            configured: !webhook_targets.is_empty(),
            targets: webhook_targets.iter().map(|target| WebhookTargetView {
                number: target.number, host: target.host(), format: target.format.label().into(),
            }).collect(),
            last_event: app.db.events(200, None).into_iter().find(|e| e.kind.starts_with("webhook_"))
                .map(|e| format!("{} · {}", e.ts, e.reason)).unwrap_or_else(|| "Noch kein Zustellversuch in den letzten Events.".into()),
        },
        billing: billing_view(&app.0),
        config_raw: app.cfg_raw(),
        config_msg,
    };
    let mut response = Html(tpl.render().unwrap_or_default()).into_response();
    response.headers_mut().insert(axum::http::header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

/// Dashboard-Button „Alle Instanzen zerstören“ — gleiches Primitiv wie
/// /api/v1/destroy_all (alle nicht gepinnten, über alle Slots).
pub async fn do_destroy_all(app: AppCtx, headers: axum::http::HeaderMap) -> Response {
    page_guard!(app, ReqOf(&headers));
    match crate::reconciler::destroy_all_instances(&app.0, None, "dashboard: destroy_all").await {
        Ok(n) => Redirect::to(&format!("/?msg=destroyed%20{n}")).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

/// Save aus dem Dashboard-Editor: validiert + schreibt + tauscht live
/// (gleicher Pfad wie PUT /api/v1/config). Fehler → Message statt Reload.
pub async fn do_config_save(app: AppCtx, headers: axum::http::HeaderMap, Form(form): Form<HashMap<String, String>>) -> Response {
    page_guard!(app, ReqOf(&headers));
    let raw = form.get("raw").cloned().unwrap_or_default();
    match crate::api::apply_config(&app.0, &raw).await {
        Ok(msg) => Redirect::to(&format!("/settings?msg=gespeichert%3A%20{}", urlencode(&msg))).into_response(),
        Err(e) => Redirect::to(&format!("/settings?msg=fehler%3A%20{}", urlencode(&format!("{e}")))).into_response(),
    }
}

/// The same authenticated test as the API, without exposing the webhook URL.
pub async fn do_webhook_test(app: AppCtx, headers: axum::http::HeaderMap) -> Response {
    page_guard!(app, ReqOf(&headers));
    let message = match crate::notifications::test(&app.0).await {
        Ok(report) => format!("{} — {report}. Bitte in den Zielkanälen prüfen.",
            if report.ok { "Webhook-Test erfolgreich" } else { "Webhook-Test mit Fehlern" }),
        Err(error) => format!("Webhook-Test fehlgeschlagen: {error}"),
    };
    Redirect::to(&format!("/settings?msg={}", urlencode(&message))).into_response()
}

/// Reload von Disk (Datei außerhalb geändert — z. B. per SSH/Editor).
pub async fn do_config_reload(app: AppCtx, headers: axum::http::HeaderMap) -> Response {
    page_guard!(app, ReqOf(&headers));
    let raw = app.cfg_raw();
    match crate::api::apply_config(&app.0, &raw).await {
        Ok(msg) => Redirect::to(&format!("/settings?msg=neu%20geladen%3A%20{}", urlencode(&msg))).into_response(),
        Err(e) => Redirect::to(&format!("/settings?msg=fehler%3A%20{}", urlencode(&format!("{e}")))).into_response(),
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b' ' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------- Form-Actions

async fn api_slot(app: &SharedApp, slot_id: i64, action: &str) -> Response {
    let req = Request::builder()
        .method(axum::http::Method::POST)
        // Bearer mitgeben: slot_action ist per guarded! geschützt — ohne
        // Header war JEDE Dashboard-Slot-Aktion (wake/stop/pin/…) ein
        // stilles 401-No-Op (gefunden im Browser-Klicktest 20.09.).
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {}", token_of(app)))
        .uri("/")
        .body(Body::empty())
        .unwrap();
    crate::api::slot_action(AppCtx(app.clone()), axum::extract::Path((slot_id, action.to_string())), req).await
}

pub async fn do_slot_action(app: AppCtx, Path((slot_id, action)): Path<(i64, String)>, headers: axum::http::HeaderMap) -> Response {
    page_guard!(app, ReqOf(&headers));
    let response = api_slot(&app.0, slot_id, &action).await;
    if !response.status().is_success() { return response; }
    Redirect::to("/").into_response()
}

/// Dashboard-Toggle „Auto-Miete AN/AUS“ (Overview-Karte): aus = keine
/// automatischen Mieten/Starts, laufende Boxen stoppen (s. set_auto_rent).
pub async fn do_auto_rent(app: AppCtx, headers: axum::http::HeaderMap) -> Response {
    page_guard!(app, ReqOf(&headers));
    let enabled = !app.db.auto_rent_enabled();
    if let Err(e) = crate::reconciler::set_auto_rent(&app.0, enabled, "dashboard").await {
        return (StatusCode::BAD_GATEWAY, e.to_string()).into_response();
    }
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
                .header(axum::http::header::AUTHORIZATION, format!("Bearer {}", token_of(&app.0)))
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
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {}", token_of(&app.0)))
        .body(Body::from(body_str))
        .unwrap();
    let response = crate::api::instance_action(AppCtx(app.0.clone()), axum::extract::Path((vast_id, action.clone())), api_req).await;
    if !response.status().is_success() { return response; }
    Redirect::to(&back).into_response()
}

pub async fn do_asset_upload(
    app: AppCtx,
    Path(scope): Path<String>,
    headers: axum::http::HeaderMap,
    mut multipart: Multipart,
) -> Response {
    page_guard!(app, ReqOf(&headers));
    let mut uploaded: Vec<String> = Vec::new();
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
        uploaded.push(name);
    }
    // Feedback: Name im Query-Param (safe-Zeichensatz, '/' → %2F) — die
    // Assets-Seite zeigt danach das aufgeloeste Ziel/Restart/Mode (Meta!
    // Erfolg sichtbar statt stiller Redirect, 21.09. Nutzer-Wunsch).
    let ok = uploaded.join(", ").replace('/', "%2F");
    Redirect::to(&format!("/assets?ok={ok}")).into_response()
}