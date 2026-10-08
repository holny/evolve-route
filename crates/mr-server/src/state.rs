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
    pub key_cursor: Mutex<HashMap<String, usize>>,
    pub engine: Engine,
    pub sessions: SessionStore,
    pub events: EventLog,
    pub health: HealthRegistry,
    pub quota: QuotaLedger,
    pub flywheel: Flywheel,
    pub bus: broadcast::Sender<serde_json::Value>,
    pub http: reqwest::Client,
    /// 订阅方案预算（plan_key → 5h 积分额度），面板档位驱动
    pub plan_budgets: Mutex<HashMap<String, f64>>,
}

pub type AppState = Arc<Inner>;

pub fn build_state(config: FileConfig) -> AppState {
    let mut discovered = mr_discovery::discover(&config.discovery.agents);

    // remote /models discovery FIRST (provider union), THEN models.dev
    // enrichment can fill unknown windows for remote entries too
    if config.catalog.remote_fetch {
        let mut base_records = config.model_records();
        base_records.extend(discovered.iter().cloned());
        let self_origin = format!("{}:{}", config.server.host, config.server.port);
        discovered.extend(mr_discovery::remote::discover_remote_blocking(
            &base_records,
            &config.data.dir,
            Some(&self_origin),
        ));
    }

    if config.catalog.modelsdev_reference {
        if let Some(api) = mr_discovery::modelsdev::load_fresh(&config.data.dir) {
            mr_discovery::modelsdev::enrich(&mut discovered, &api);
        }
        mr_discovery::modelsdev::spawn_refresh(config.data.dir.clone());
    }

    // remote entries without a usable window are dead weight — drop them
    discovered.retain(|m| m.source != mr_core::types::Source::Remote || m.context_window.is_some());

    let catalog = Catalog::build_with_discovered(&config, discovered);
    let policy = config.policy.clone();
    let backend = DecisionBackend::build(&config.decision.backend);
    let engine = Engine::new(catalog, policy, backend);

    // 面板调控覆盖（overrides.json，重启重放）：模型权重 + 公式权重
    let ov = load_overrides(&config.data.dir);
    for (id, w) in &ov.models {
        engine.set_weight_override(id, Some(*w));
    }
    if let Some(w) = ov.weights {
        engine.set_weights_override(Some(mr_core::types::PolicyWeights {
            quality: w[0], speed: w[1], cost: w[2], stability: w[3], headroom: w[4],
        }));
    }

    // 订阅方案预算：档位→额度（registry），显式 allowance 覆盖优先
    let mut plan_budgets: HashMap<String, f64> = HashMap::new();
    for (key, po) in &ov.plans {
        if let Some(tier) = &po.tier
            && let Some(a) = mr_core::plans::tier_allowance_by_key(key, tier)
        {
            plan_budgets.insert(key.clone(), a);
        }
    }

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
        plan_budgets: Mutex::new(plan_budgets),
    })
}

impl Inner {
    /// 订阅方案预算压力（plan_key → 消耗占比）：额度内积分消耗 / 档位额度。
    /// 无额度配置（未声明档位）的方案无压力。
    pub fn plan_pressure_map(&self) -> HashMap<String, f32> {
        let mut out = HashMap::new();
        let Ok(budgets) = self.plan_budgets.lock() else { return out };
        if budgets.is_empty() {
            return out;
        }
        let now = mr_memory::quota::now_ms();
        let sums = self.quota.plan_usage_sums(now, 5 * 3600 * 1000);
        // 模型 → plan_key 映射（按 catalog base_url 匹配注册表）
        for m in &self.engine.catalog.models {
            if let Some(key) = mr_core::plans::plan_key_for(&m.base_url)
                && let Some(allowance) = budgets.get(key)
                && let Some((i, c, o)) = sums.get(&m.id)
            {
                // 同方案多模型共享账户额度——SUM 而非 MAX（MAX 会漏计并发燃烧）
                let credits = mr_core::plans::plan_credits_used(&m.base_url, &m.id, *i, *c, *o);
                let p = (credits / allowance).clamp(0.0, 1.5) as f32;
                out.entry(key.to_string()).and_modify(|e| *e = (*e + p).min(1.5)).or_insert(p);
            }
        }
        out
    }

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

    /// Must be called inside the tokio runtime.
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
        .route("/api/weight", axum::routing::post(crate::meta::api_weight))
        .route(
            "/api/policy-weights",
            axum::routing::get(crate::meta::api_policy_weights_get)
                .post(crate::meta::api_policy_weights_set),
        )
        .route("/api/trends", axum::routing::get(crate::meta::api_trends))
        .route(
            "/api/plans",
            axum::routing::get(crate::meta::api_plans).post(crate::meta::api_plans_set),
        )
        .route("/v1/chat/completions", axum::routing::post(crate::relay::chat_completions))
        .route("/v1/models", axum::routing::get(crate::meta::list_models))
        .route("/healthz", axum::routing::get(crate::meta::healthz))
        .route("/api/health", axum::routing::get(crate::meta::api_health))
        .route("/api/stats", axum::routing::get(crate::meta::api_stats_query))
        .route("/api/feedback", axum::routing::post(crate::meta::api_feedback))
        .route("/api/quota", axum::routing::get(crate::meta::api_quota))
        .route("/api/events", axum::routing::get(crate::meta::api_events))
        .route("/api/benchmarks", axum::routing::get(crate::meta::api_benchmarks))
        .route("/api/stream", axum::routing::get(crate::meta::api_stream))
        .route("/", axum::routing::get(crate::meta::dashboard))
        .route("/v1/messages", axum::routing::post(crate::anthropic::messages))
        .route("/v1/messages/count_tokens", axum::routing::post(crate::anthropic::count_tokens))
        .with_state(state)
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024))
}

/// 面板调控覆盖的持久化形态（data.dir/overrides.json）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct Overrides {
    #[serde(default)]
    pub models: HashMap<String, f32>,
    /// normalized [quality, speed, cost, stability, headroom]
    #[serde(default)]
    pub weights: Option<[f32; 5]>,
    /// provider 配额方案订正（档位/方案/窗口说明）
    #[serde(default)]
    pub plans: HashMap<String, PlanOverride>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct PlanOverride {
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub scheme: Option<String>,
    #[serde(default)]
    pub windows: Option<String>,
}

pub fn overrides_path(data_dir: &str) -> std::path::PathBuf {
    let p = if let Some(rest) = data_dir.strip_prefix("~/") {
        std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(rest)).unwrap_or_else(|_| std::path::PathBuf::from(data_dir))
    } else {
        std::path::PathBuf::from(data_dir)
    };
    p.join("overrides.json")
}

pub fn load_overrides(data_dir: &str) -> Overrides {
    std::fs::read_to_string(overrides_path(data_dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_overrides(data_dir: &str, o: &Overrides) {
    if let Ok(t) = serde_json::to_string_pretty(o) {
        let _ = std::fs::write(overrides_path(data_dir), t);
    }
}
