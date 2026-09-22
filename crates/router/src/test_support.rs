//! Offline fixtures: ephemeral SQLite files and loopback-only HTTP providers.
use crate::{config::Config, db::{Db, InstanceRow}, state::{App, SharedApp}};
use std::sync::{Arc, Mutex, RwLock};
use axum::{body::Body, extract::Request, response::Response};

pub struct TestApp {
    pub app: SharedApp,
    pub dir: std::path::PathBuf,
}

impl TestApp {
    pub fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("pgpu-test-{}-{:016x}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = Config::load_str(r#"
            [router]
            token = "test-token-not-a-real-secret-12345678"
            tz = "UTC"
            [budget]
            daily_soft_eur = 1000.0
            daily_hard_eur = 2000.0
            monthly_eur = 10000.0
            usd_per_eur = 1.0
            [limits]
            max_total_rate_usd_h = 10.0
            [[slots]]
            id = 1
            name = "test"
            role = "llm"
            [slots.bid]
            ceiling_usd_h = 2.0
        "#).unwrap();
        cfg.router.data_dir = dir.clone();
        // Even if real environment credentials exist, tests cannot reach NetBird.
        cfg.netbird.api_url = "http://127.0.0.1:1".into();
        cfg.netbird.setup_key = "test".into();
        let db = Db::open(&dir.join("test.sqlite")).unwrap();
        db.init_slots(&cfg).unwrap();
        let app = Arc::new(App {
            cfg: RwLock::new(Arc::new(cfg)), config_path: dir.join("config.toml").to_string_lossy().into(),
            db, management: tokio::sync::Mutex::new(()), proxy_client: crate::proxy::http_client(),
            last_reconcile: Default::default(), shutting_down: Default::default(),
            events: crate::events::EventBus::new(32), traffic: Default::default(), jobs: Default::default(),
            targets: Default::default(), pool_routes: Default::default(), hub: Default::default(),
            vast: Arc::new(Mutex::new(None)), reconcile_now: Default::default(), stt_sessions: Default::default(),
        });
        Self { app, dir }
    }

    pub fn insert(&self, id: i64, created: &str) {
        self.app.db.insert_instance(&InstanceRow {
            vast_id: id, slot_id: 1, role: praxis_common::Role::Llm, node_token: format!("{id:08x}{id:040x}"),
            offer_id: 1, machine_id: 1, gpu_name: "test".into(), nb_ip: None, image: "test".into(),
            mode: praxis_common::Mode::Interruptible, lifecycle: "{\"kind\":\"auto\"}".into(), pinned: false,
            actual_status: "running".into(), intended_status: "running".into(), state: "healthy".into(),
            healthy: true, ever_healthy: true, busy: false, busy_reason: String::new(), min_bid: 1.0, bid_usd_h: 1.0, dph_total: 1.0,
            storage_usd_h: 0.1, created_at: created.into(), healthy_since: Some(created.into()),
            stopped_since: None, destroyed_at: None, label: "test".into(),
        }).unwrap();
        self.app.db.set_instance_state(id, "healthy").unwrap();
        self.app.db.set_active_instance(1, Some(id)).unwrap();
    }

    pub fn request(&self, body: &str) -> Request {
        Request::builder().header("authorization", format!("Bearer {}", self.app.cfg().router_token()))
            .body(Body::from(body.to_owned())).unwrap()
    }
}

impl Drop for TestApp {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.dir); }
}

pub struct LocalServer {
    pub addr: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}
impl LocalServer {
    pub async fn new(router: axum::Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });
        Self { addr, task }
    }
    pub fn url(&self) -> String { format!("http://{}", self.addr) }
}
impl Drop for LocalServer {
    fn drop(&mut self) { self.task.abort(); }
}

type Calls = Arc<Mutex<Vec<(String, String, serde_json::Value)>>>;
pub struct MockProvider {
    pub server: LocalServer,
    pub calls: Calls,
    pub reply: Arc<Mutex<(u16, String)>>,
}
impl MockProvider {
    pub async fn new() -> Self {
        let calls: Calls = Default::default();
        let reply = Arc::new(Mutex::new((200, r#"{"success":true}"#.to_string())));
        let c = calls.clone();
        let r = reply.clone();
        let router = axum::Router::new().fallback(move |req: Request| {
            let c = c.clone(); let r = r.clone();
            async move {
                let (parts, body) = req.into_parts();
                let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
                c.lock().unwrap().push((parts.method.to_string(), parts.uri.to_string(), serde_json::from_slice(&bytes).unwrap_or_default()));
                let (status, body) = r.lock().unwrap().clone();
                Response::builder().status(status).header("content-type", "application/json").body(Body::from(body)).unwrap()
            }
        });
        Self { server: LocalServer::new(router).await, calls, reply }
    }
    pub fn attach(&self, app: &SharedApp) {
        *app.vast.lock().unwrap() = Some(praxis_vast::Vast::with_api_root("fake-key", &self.server.url()).unwrap());
    }
    pub fn respond(&self, status: u16, body: &str) { *self.reply.lock().unwrap() = (status, body.into()); }
}
