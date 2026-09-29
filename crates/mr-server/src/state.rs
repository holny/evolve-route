use mr_core::catalog::Catalog;
use mr_core::config::FileConfig;
use mr_core::engine::Engine;
use mr_decision::DecisionBackend;
use mr_memory::{EventLog, Flywheel, HealthRegistry, QuotaLedger, SessionStore};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

pub struct Inner {
    pub config: FileConfig,
    pub key_cursor: std::sync::Mutex<HashMap<String, usize>>,
    pub engine: Engine,
    pub sessions: SessionStore,
    pub events: EventLog,
    pub health: HealthRegistry,
    pub quota: QuotaLedger,
    pub flywheel: Flywheel,
    pub bus: broadcast::Sender<serde_json::Value>,
    pub http: reqwest::Client,
}

pub type AppState = Arc<Inner>;

pub fn build_state(config: FileConfig) -> AppState {
    let mut discovered = mr_discovery::discover(&config.discovery.agents);
    if config.catalog.modelsdev_reference {
        if let Some(api) = mr_discovery::modelsdev::load_fresh(&config.data.dir) {
            mr_discovery::modelsdev::enrich(&mut discovered, &api);
        }
        mr_discovery::modelsdev::spawn_refresh(config.data.dir.clone());
    }
    let catalog = Catalog::build_with_discovered(&config, discovered);
    let policy = config.policy.clone();
    let backend = DecisionBackend::build(&config.decision.backend);
    let engine = Engine::new(catalog, policy, backend);
    let events = EventLog::open(&config.data.dir);
    let flywheel = Flywheel::open(&config.data.dir);
    let (bus, _) = broadcast::channel(256);
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
        .expect("http client");

    Arc::new(Inner {
        config,
        key_cursor: Mutex::new(HashMap::new()),
        engine,
        sessions: SessionStore::new(),
        events,
        health: HealthRegistry::new(),
        quota: QuotaLedger::new(),
        flywheel,
        bus,
        http,
    })
}

impl Inner {
    /// Round-robin starting index for a model's key pool (load spread).
    pub fn key_start(&self, model: &str, len: usize) -> usize {
        if len <= 1 {
            return 0;
        }
        let Ok(mut m) = self.key_cursor.lock() else { return 0 };
        let e = m.entry(model.to_string()).or_insert(0);
        let v = *e;
        *e = (*e + 1) % len;
        v
    }

    /// Must be called inside the tokio runtime: periodic flywheel persistence.
    pub fn start_background(&self) {
        let flywheel = self.flywheel.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                flywheel.save();
            }
        });
    }
}

pub fn build_router(state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/v1/chat/completions", axum::routing::post(crate::relay::chat_completions))
        .route("/v1/models", axum::routing::get(crate::meta::list_models))
        .route("/healthz", axum::routing::get(crate::meta::healthz))
        .route("/api/health", axum::routing::get(crate::meta::api_health))
        .route("/api/stats", axum::routing::get(crate::meta::api_stats))
        .route("/api/feedback", axum::routing::post(crate::meta::api_feedback))
        .route("/api/quota", axum::routing::get(crate::meta::api_quota))
        .route("/api/stream", axum::routing::get(crate::meta::api_stream))
        .route("/", axum::routing::get(crate::meta::dashboard))
        .route("/v1/messages", axum::routing::post(crate::anthropic::messages))
        .route("/v1/messages/count_tokens", axum::routing::post(crate::anthropic::count_tokens))
        .with_state(state)
}
