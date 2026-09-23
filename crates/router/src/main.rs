//! pgpu — Praxis GPU Router (Bauplan v2).
//!
//! Slots statt Instanzen: Slot 1 = llm (fork-llama), Slot 2 = media
//! (fork-comfyui). Passthrough-Ports + Pfad-Routing auf dem Dashboard,
//! Reconciler mit Policy, Agent-Hub mit Assets.

mod api;
mod catalog;
mod performance;
mod assets;
mod billing;
mod connector;
mod config;
mod costs;
mod dashboard;
mod db;
mod events;
mod free_router;
mod hub;
mod node;
mod netbird;
mod notifications;
mod operations;
mod webhook;

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod regressions;
#[cfg(test)]
mod performance_tests;
mod proxy;
mod reconciler;
mod schedule_time;
mod selection;
mod selection_ui;
mod slot_labels;
mod state;

use anyhow::{Context, Result};
use axum::routing::{get, post};
use state::{SharedApp, Traffic};
use std::sync::{Arc, Mutex, RwLock};

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
    if args.iter().any(|a|a=="--version") {
        println!("praxis-router {}",env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let config_path = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|p| args.get(p + 1))
        .cloned()
        .or_else(|| std::env::var("PGPU_CONFIG").ok())
        .unwrap_or_else(|| "config.toml".to_string());
    let mut cfg = config::Config::load(&config_path)
        .with_context(|| format!("config {config_path} laden"))?;
    if args.iter().any(|a|a=="--check-config") {
        println!("configuration valid ({} slots; schema only, no provider calls)",cfg.slots.len());
        return Ok(());
    }
    cfg.validate_auth()?;
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
    // Never admit a rental from legacy/cross-version offer pricing. Fresh
    // mode-specific quotes are required after every process start.
    db.invalidate_offers()?;

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
        cfg: RwLock::new(Arc::new(cfg.clone())),
        config_path: config_path.clone(),
        db: db.clone(),
        management: tokio::sync::Mutex::new(()),
        proxy_client: proxy::http_client(),
        free_router: free_router::Router::new()?,
        last_reconcile: Default::default(),
        shutting_down: Default::default(),
        events: event_bus,
        traffic: Traffic::default(),
        jobs: Default::default(),
        targets: Default::default(),
        pool_routes: Default::default(),
        hub: Default::default(),
        vast: Arc::new(Mutex::new(vast)),
        reconcile_now: tokio::sync::Notify::new(),
        stt_sessions: Default::default(),
    });

    app.db.initialize_notifications(chrono::Utc::now().timestamp())?;
    app.events.emit(
        &app.db,
        "router_started",
        None,
        None,
        &format!("pgpu {} — {} Slots", env!("CARGO_PKG_VERSION"), app.cfg().slots.len()),
        &serde_json::json!({"config": config_path}),
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    // Keep handles: a dead control loop must fail the process, not silently
    // leave only the dashboard alive while GPUs continue costing money.
    let notifications_task = tokio::spawn(notifications::run(app.clone()));
    let mut connector_task = {
        let app = app.clone();
        tokio::spawn(async move { connector::run(app).await })
    };

    let mut reconciler_task = {
        let app = app.clone();
        tokio::spawn(async move {
            // Erster Tick sofort: Angebote vorwärmen.
            for slot in app.cfg().slots.clone() {
                let _ = reconciler::search_slot_offers(&app, slot.id, true).await;
            }
            reconciler::run(app).await;
        })
    };

    // Dashboard + API.
    let dash = axum::Router::<SharedApp>::new()
        .route("/", get(dashboard::index))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(api::ready))
        .route("/login", get(dashboard::login_page).post(dashboard::login_submit))
        .route("/offers", get(dashboard::offers_page))
        .route("/performance", get(dashboard::performance_page))
        .route("/instances/:id", get(dashboard::instance_page).post(dashboard::instance_page))
        .route("/term/:id", get(dashboard::terminal_page))
        .route("/schedules", get(dashboard::schedules_page))
        .route("/assets", get(dashboard::assets_page))
        .route("/settings", get(dashboard::settings_page))
        .route("/do/config", post(dashboard::do_config_save))
        .route("/do/config/reload", post(dashboard::do_config_reload))
        .route("/do/webhook/test", post(dashboard::do_webhook_test))
        .route("/do/selection", post(selection_ui::save))
        .route("/api/v1/selection/preview", post(selection_ui::preview))
        .route("/do/slots/:id/:action", post(dashboard::do_slot_action))
        .route("/do/auto_rent", post(dashboard::do_auto_rent))
        .route("/do/rent", post(dashboard::do_rent))
        .route("/do/instances/:id/:action", post(dashboard::do_instance_action))
        .route("/do/machines/:id/:action", post(dashboard::do_machine_action))
        .route("/do/assets/:scope/upload", post(dashboard::do_asset_upload))
        .route("/api/v1/state", get(api::state))
        .route("/api/v1/settings/auto_rent", post(api::auto_rent_set))
        .route("/api/v1/config", get(api::config_get).put(api::config_put))
        .route("/api/v1/free-router", get(free_router::ui::get).put(free_router::ui::save))
        .route("/api/v1/alerts/test", post(api::webhook_test))
        .route("/api/v1/destroy_all", post(api::destroy_all))
        .route("/api/v1/sleep_all", post(api::sleep_all))
        .route("/do/destroy_all", post(dashboard::do_destroy_all))
        .route("/api/v1/budget", get(api::budget))
        .route("/api/v1/events", get(api::events_json))
        .route("/api/v1/events/stream", get(api::events_sse))
        .route("/api/v1/offers", get(api::offers))
        .route("/api/v1/machines", get(api::machines))
        .route("/api/v1/performance", get(api::performance_history))
        .route("/api/v1/performance/summary", get(api::performance_summary))
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
        // nest_service (NICHT route_service): nest stript das /static-Präfix,
        // route_service tut es NICHT → ServeDir suchte static/static/<file>
        // → alles unter /static/* war 404 (xterm.js, htmx …), das Terminal-
        // Widget renderte nie. Erste live bemerkt 21.09. (Terminal-Seite ohne
        // xterm). Local reproduziert und verifiziert.
        .nest_service("/static", tower_http::services::ServeDir::new(static_dir()))
        .fallback(free_router::endpoint)
        .with_state(app.clone());

    let bind_ip = app.cfg().router.bind_ip.clone();
    let dash_port = app.cfg().router.dashboard_port;

    // Passthrough-Listener: pro Slot-Service-Port ein Socket.
    let mut passthrough_binds: Vec<(u16, i64, String)> = Vec::new();
    for slot in &app.cfg().slots {
        for (port, service) in slot.passthrough_ports() {
            passthrough_binds.push((port, slot.id, service));
        }
    }
    let mut tasks = tokio::task::JoinSet::new();
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
        let shutdown = shutdown_rx.clone();
        tasks.spawn(async move {
            axum::serve(listener, router).with_graceful_shutdown(wait_shutdown(shutdown)).await
        });
    }

    // STT-Port bei lokalem Mode gehört dem Router (Sidecar-Proxy).
    if app.cfg().stt.mode == "local" {
        let stt_app = app.clone();
        let stt_port = app.cfg().stt.listen_port;
        let stt_url = app.cfg().stt.url.clone();
        let stt_router = axum::Router::new()
            .fallback(move |req| {
                let app = stt_app.clone();
                async move { proxy::passthrough(app, 0, "stt".to_string(), req).await }
            })
            .with_state(app.clone());
        let addr = format!("{bind_ip}:{stt_port}");
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        tracing::info!("stt local {addr} → sidecar {stt_url}");
        let shutdown = shutdown_rx.clone();
        tasks.spawn(async move {
            axum::serve(listener, stt_router).with_graceful_shutdown(wait_shutdown(shutdown)).await
        });
    }

    let app2 = app.clone();
    let addr = format!("{bind_ip}:{dash_port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("dashboard {addr} (Slots: {})", app2.cfg().slots.len());
    tasks.spawn(async move {
        axum::serve(listener, dash).with_graceful_shutdown(wait_shutdown(shutdown_rx)).await
    });
    let unexpected = tokio::select! {
        result = shutdown_signal() => { result?; None },
        result = tasks.join_next() => Some(format!("listener exited: {result:?}")),
        result = &mut connector_task => Some(format!("connector exited: {result:?}")),
        result = &mut reconciler_task => Some(format!("reconciler exited: {result:?}")),
    };
    app.shutting_down.store(true, std::sync::atomic::Ordering::Relaxed);
    app.reconcile_now.notify_one();
    let _ = shutdown_tx.send(true);
    connector_task.abort();
    notifications_task.abort();
    // Allow in-progress requests/control actions to finish, but never hang
    // indefinitely on SSE/WS. A service manager can safely restart afterwards.
    let graceful = async {
        while tasks.join_next().await.is_some() {}
        if unexpected.is_none() { let _ = (&mut reconciler_task).await; }
    };
    if tokio::time::timeout(std::time::Duration::from_secs(30), graceful).await.is_err() {
        tracing::warn!("shutdown drain timed out after 30 seconds");
    }
    tasks.abort_all();
    reconciler_task.abort();
    if let Some(reason) = unexpected { anyhow::bail!(reason); }
    Ok(())
}

async fn wait_shutdown(mut rx: tokio::sync::watch::Receiver<bool>) {
    if !*rx.borrow() { let _ = rx.changed().await; }
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { r = tokio::signal::ctrl_c() => r, _ = term.recv() => Ok(()) }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
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
