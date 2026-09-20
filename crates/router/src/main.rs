//! pgpu — Praxis GPU Router (Bauplan v2).
//!
//! Slots statt Instanzen: Slot 1 = llm (fork-llama), Slot 2 = media
//! (fork-comfyui). Passthrough-Ports + Pfad-Routing auf dem Dashboard,
//! Reconciler mit Policy, Agent-Hub mit Assets.

mod api;
mod assets;
mod connector;
mod config;
mod dashboard;
mod db;
mod events;
mod hub;
mod node;
mod proxy;
mod reconciler;
mod state;

use anyhow::{Context, Result};
use axum::routing::{get, post};
use state::{SharedApp, Traffic};
use std::sync::{Arc, Mutex};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?;
    rt.block_on(async_main())
}

async fn async_main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let config_path = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|p| args.get(p + 1))
        .cloned()
        .or_else(|| std::env::var("PGPU_CONFIG").ok())
        .unwrap_or_else(|| "config.toml".to_string());
    let mut cfg = config::Config::load(&config_path)
        .with_context(|| format!("config {config_path} laden"))?;
    // bind_ip = "auto": erste IPv4 im NetBird-Range (100.64.0.0/10) am
    // wt0/anderen Interfaces erkennen — VPS-Deploy ohne hartcodierte IP
    // (NetBird-IP steht erst nach Enroll fest). Gleiches gilt für
    // router_nb_ip = "auto" (Call-home-URL für Agents).
    let auto_ip = detect_netbird_ip();
    if cfg.router.bind_ip == "auto" {
        let ip = auto_ip.clone().unwrap_or_else(|| {
            tracing::warn!("bind_ip=auto: keine NetBird-IP gefunden — fallback 127.0.0.1");
            "127.0.0.1".to_string()
        });
        tracing::info!(%ip, "bind_ip=auto aufgelöst");
        cfg.router.bind_ip = ip;
    }
    if cfg.netbird.router_nb_ip == "auto" {
        cfg.netbird.router_nb_ip = auto_ip.unwrap_or_else(|| cfg.router.bind_ip.clone());
    }
    let cfg = &cfg;
    let data_dir = cfg.router.data_dir.clone();
    let db = db::Db::open(&data_dir.join("pgpu.sqlite"))?;
    db.init_slots(&cfg)?;

    let vast = if !cfg.vast_api_key().is_empty() {
        match praxis_vast::Vast::new(cfg.vast_api_key()) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(%e, "vast client init fehlgeschlagen — Router läuft ohne Vast-API");
                None
            }
        }
    } else {
        tracing::warn!("VAST_API_KEY nicht gesetzt — nur Proxy/Dashboard aktiv");
        None
    };

    let event_bus = events::EventBus::new(256);
    // Db broadcastet ab jetzt jedes add_event in den SSE-Live-Feed.
    db.set_sse_sender(event_bus.sender());

    let app: SharedApp = Arc::new(state::App {
        cfg: cfg.clone(),
        db: db.clone(),
        events: event_bus,
        traffic: Traffic::default(),
        jobs: Default::default(),
        targets: Default::default(),
        hub: Default::default(),
        vast: Arc::new(Mutex::new(vast)),
        reconcile_now: tokio::sync::Notify::new(),
        stt_sessions: Default::default(),
    });

    app.events.emit(
        &app.db,
        "router_started",
        None,
        None,
        &format!("pgpu {} — {} Slots", env!("CARGO_PKG_VERSION"), app.cfg.slots.len()),
        &serde_json::json!({"config": config_path}),
    );

    // Agent-Connector: Router waehlt sich in die gpu-agents ein (:9100).
    {
        let app = app.clone();
        tokio::spawn(async move {
            connector::run(app).await;
        });
    }

    // Reconciler.
    {
        let app = app.clone();
        tokio::spawn(async move {
            // Erster Tick sofort: Angebote vorwärmen.
            for slot in app.cfg.slots.clone() {
                let _ = reconciler::search_slot_offers(&app, slot.id, true).await;
            }
            reconciler::run(app).await;
        });
    }

    // Dashboard + API.
    let dash = axum::Router::<SharedApp>::new()
        .route("/", get(dashboard::index))
        .route("/login", get(dashboard::login_page).post(dashboard::login_submit))
        .route("/offers", get(dashboard::offers_page))
        .route("/instances/:id", get(dashboard::instance_page).post(dashboard::instance_page))
        .route("/term/:id", get(dashboard::terminal_page))
        .route("/schedules", get(dashboard::schedules_page))
        .route("/assets", get(dashboard::assets_page))
        .route("/settings", get(dashboard::settings_page))
        .route("/do/slots/:id/:action", post(dashboard::do_slot_action))
        .route("/do/auto_rent", post(dashboard::do_auto_rent))
        .route("/do/rent", post(dashboard::do_rent))
        .route("/do/instances/:id/:action", post(dashboard::do_instance_action))
        .route("/do/machines/:id/:action", post(dashboard::do_machine_action))
        .route("/do/assets/:scope/upload", post(dashboard::do_asset_upload))
        .route("/api/v1/state", get(api::state))
        .route("/api/v1/settings/auto_rent", post(api::auto_rent_set))
        .route("/api/v1/budget", get(api::budget))
        .route("/api/v1/events", get(api::events_json))
        .route("/api/v1/events/stream", get(api::events_sse))
        .route("/api/v1/offers", get(api::offers))
        .route("/api/v1/machines", get(api::machines))
        .route("/api/v1/machines/:id/:action", post(api::machine_action))
        .route("/api/v1/slots/:id/:action", post(api::slot_action))
        .route("/api/v1/instances", post(api::instance_create))
        .route("/api/v1/instances/:id/:action", post(api::instance_action))
        .route("/api/v1/instances/:id/logs", get(api::instance_logs))
        .route("/api/v1/instances/:id/cmd/:cmd", post(api::instance_cmd))
        .route("/api/v1/instances/:id/ws/term", get(api::term_ws))
        .route("/gpu/:slot/:svc/*path", any_of_proxy())
        .route("/gpu/:slot/:svc", any_of_proxy())
        .route("/inst/:vast_id/:svc/*path", get(proxy::inst_path))
        .route("/inst/:vast_id/:svc", get(proxy::inst_path))
        .route("/api/v1/node", get(node::node_ws))
        .route("/api/v1/node/assets/manifest", get(node::manifest))
        .route("/api/v1/node/assets/:id", get(node::asset))
        .route("/api/v1/node/reports", post(node::report))
        .route("/api/v1/node/events", post(node::node_event))
        .route_service("/static/*path", tower_http::services::ServeDir::new(static_dir()))
        .with_state(app.clone());

    let bind_ip = app.cfg.router.bind_ip.clone();
    let dash_port = app.cfg.router.dashboard_port;

    // Passthrough-Listener: pro Slot-Service-Port ein Socket.
    let mut passthrough_binds: Vec<(u16, i64, String)> = Vec::new();
    for slot in &app.cfg.slots {
        for (port, service) in slot.passthrough_ports() {
            passthrough_binds.push((port, slot.id, service));
        }
    }
    let mut tasks = Vec::new();
    for (port, slot_id, service) in passthrough_binds {
        let outer_app = app.clone();
        let service2 = service.clone();
        let slot2 = slot_id;
        let router = axum::Router::new()
            .fallback(move |req| {
                let app = outer_app.clone();
                let service = service2.clone();
                async move { proxy::passthrough(app, slot2, service, req).await }
            })
            .with_state(app.clone());
        let addr = format!("{bind_ip}:{port}");
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        tracing::info!("passthrough {addr} → slot {slot2}/{service}");
        tasks.push(tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!(%e, "passthrough listener died");
            }
        }));
    }

    // STT-Port bei lokalem Mode gehört dem Router (Sidecar-Proxy).
    if app.cfg.stt.mode == "local" {
        let stt_app = app.clone();
        let stt_port = app.cfg.stt.listen_port;
        let stt_url = app.cfg.stt.url.clone();
        let stt_router = axum::Router::new()
            .fallback(move |req| {
                let app = stt_app.clone();
                async move { proxy::passthrough(app, 0, "stt".to_string(), req).await }
            })
            .with_state(app.clone());
        let addr = format!("{bind_ip}:{stt_port}");
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        tracing::info!("stt local {addr} → sidecar {stt_url}");
        tasks.push(tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, stt_router).await {
                tracing::error!(%e, "stt listener died");
            }
        }));
    }

    let app2 = app.clone();
    let addr = format!("{bind_ip}:{dash_port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("dashboard {addr} (Slots: {})", app2.cfg.slots.len());
    axum::serve(listener, dash).await?;
    for t in tasks {
        let _ = t.await;
    }
    Ok(())
}

fn static_dir() -> std::path::PathBuf {
    // Cargo-CWD = Crate-Root (crates/router), im Docker /opt/praxis/static.
    let p = std::path::PathBuf::from("static");
    if p.is_dir() { p } else { "/opt/praxis/static".into() }
}

fn any_of_proxy() -> axum::routing::MethodRouter<SharedApp> {
    use axum::routing::any;
    any(move |axum::extract::State(app): axum::extract::State<SharedApp>, axum::extract::Path((slot, service)): axum::extract::Path<(i64, String)>, req| async move {
        proxy::gpu_path(state::AppCtx(app), axum::extract::Path((slot, service)), req).await
    })
}

/// Erste IPv4 im NetBird-CGNAT-Range (100.64.0.0/10) über alle Interfaces.
/// Für `bind_ip = "auto"` (VPS: IP steht erst nach NetBird-Enroll fest).
fn detect_netbird_ip() -> Option<String> {
    let interfaces = if_addrs::get_if_addrs().ok()?;
    for iface in interfaces {
        let ip = iface.ip();
        if let std::net::IpAddr::V4(v4) = ip {
            let o = v4.octets();
            if o[0] == 100 && (64..=127).contains(&o[1]) {
                return Some(v4.to_string());
            }
        }
    }
    None
}
