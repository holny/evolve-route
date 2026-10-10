use turbine_core::catalog::Catalog;
use turbine_core::config::FileConfig;
use turbine_core::engine::Engine;
use turbine_decision::DecisionBackend;
use turbine_memory::{EventLog, Flywheel, HealthRegistry, QuotaLedger, SessionStore};
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
    /// 共享记账（Arc：流式 Finalizer 与 API 读侧必须同一实例，否则用量入不了账）
    pub quota: std::sync::Arc<QuotaLedger>,
    pub flywheel: Flywheel,
    pub bus: broadcast::Sender<serde_json::Value>,
    pub http: reqwest::Client,
    /// 订阅方案预算（plan_key → 5h 积分额度），面板档位驱动
    pub plan_budgets: Mutex<HashMap<String, f64>>,
    /// 全局扫描进度（面板轮询展示：逐 provider 拉取日志）
    pub scan_progress: Mutex<ScanProgress>,
}

/// 全局扫描进度快照（/api/providers/scan-progress 响应体）
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ScanProgress {
    pub running: bool,
    pub done: bool,
    pub log: Vec<String>,
}

pub type AppState = Arc<Inner>;

pub fn build_state(config: FileConfig) -> AppState {
    // overrides 提前加载：removed_base_urls 过滤初始发现（重启不复活已删 provider）
    let ov = load_overrides(&config.data.dir);
    let removed: std::collections::HashSet<String> =
        ov.removed_base_urls.iter().cloned().collect();

    let mut discovered = turbine_discovery::discover(&config.discovery.agents);
    discovered.retain(|m| !removed.contains(&m.base_url));

    // remote /models discovery FIRST (provider union), THEN models.dev
    // enrichment can fill unknown windows for remote entries too
    if config.catalog.remote_fetch {
        let mut base_records = config.model_records();
        base_records.extend(discovered.iter().cloned());
        let self_origin = format!("{}:{}", config.server.host, config.server.port);
        discovered.extend(turbine_discovery::remote::discover_remote_blocking(
            &base_records,
            &config.data.dir,
            Some(&self_origin),
        ));
    }

    if config.catalog.modelsdev_reference {
        if let Some(api) = turbine_discovery::modelsdev::load_fresh(&config.data.dir) {
            turbine_discovery::modelsdev::enrich(&mut discovered, &api);
        }
        turbine_discovery::modelsdev::spawn_refresh(config.data.dir.clone());
    }

    // remote entries without a usable window are dead weight — drop them
    discovered.retain(|m| m.source != turbine_core::types::Source::Remote || m.context_window.is_some());
    discovered.retain(|m| !removed.contains(&m.base_url));

    let catalog = Catalog::build_with_discovered(&config, discovered);
    let policy = config.policy.clone();
    let backend = DecisionBackend::build(&config.decision.backend);
    let mut engine = Engine::new(catalog, policy, backend);

    // Jev cookbook intent-routing：TypeSafe 后端时接通 route_advisor——
    // TypesafeBackend 实现了 RouteAdvisor trait（推荐方法签名匹配 turbine-core trait）
    if std::env::var("TYPESAFE_API_KEY").ok().filter(|k| !k.is_empty()).is_some() {
        if let Some(backend) = turbine_decision::typesafe::TypesafeBackend::from_env() {
            engine.set_route_advisor(std::sync::Arc::new(backend));
            tracing::info!("route_advisor: typesafe jev (intent-routing pipeline connected)");
        }
    }

    // 面板调控覆盖（overrides.json，重启重放）：模型权重 + 公式权重
    for (id, w) in &ov.models {
        engine.set_weight_override(id, Some(*w));
    }
    if let Some(w) = ov.weights {
        engine.set_weights_override(Some(turbine_core::types::PolicyWeights {
            quality: w[0], speed: w[1], cost: w[2], stability: w[3], headroom: w[4],
        }));
    }

    // 订阅方案预算：档位→额度（registry），显式 allowance 覆盖优先。
    // 仅取 plans 层（方案级）；手动 provider 档位额度只进展示层（meta），
    // 不入路由预算——同 plan_key 多账号（如 opencode-go 与 -github）档位
    // 各异，方案级注入会错把一家档位额度压到别家账号头上
    let mut plan_budgets: HashMap<String, f64> = HashMap::new();
    for (key, po) in &ov.plans {
        if let Some(tier) = &po.tier
            && let Some(a) = turbine_core::plans::tier_allowance_by_key(key, tier, 0)
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
        quota: std::sync::Arc::new(QuotaLedger::new()),
        flywheel,
        bus,
        http,
        plan_budgets: Mutex::new(plan_budgets),
        scan_progress: Mutex::new(ScanProgress::default()),
    })
}

impl Inner {
    /// 按 id 查模型（读锁短临界区，克隆返回）
    pub fn catalog_get(&self, id: &str) -> Option<turbine_core::types::ModelRecord> {
        self.engine.catalog.read().ok().and_then(|c| c.get(id).cloned())
    }

    /// Provider 实时配额上报（每次响应头部到达即写）——面板展示以此为准。
    /// plan_key 由调用方按 record.base_url 解析；headers 为原始上游响应头
    pub fn observe_provider_quota(&self, plan_key: &str, headers: &axum::http::HeaderMap) {
        let parsed = turbine_memory::quota::QuotaLedger::parse_headers(headers);
        self.quota.observe_provider(plan_key, &parsed);
    }

    /// 订阅方案预算压力（plan_key → 消耗占比）：额度内积分消耗 / 档位额度。
    /// 无额度配置（未声明档位）的方案无压力。
    pub fn plan_pressure_map(&self) -> HashMap<String, f32> {
        let mut out = HashMap::new();
        let Ok(budgets) = self.plan_budgets.lock() else { return out };
        if budgets.is_empty() {
            return out;
        }
        let now = turbine_memory::quota::now_ms();
        let sums = self.quota.plan_usage_sums(now, 5 * 3600 * 1000);
        // 模型 → plan_key 映射（按 catalog base_url 匹配注册表）
        for m in self.engine.catalog_snapshot() {
            if let Some(key) = turbine_core::plans::plan_key_for(&m.base_url)
                && let Some(&allowance) = budgets.get(key)
                && allowance > 0.0
                && let Some((i, c, o)) = sums.get(&m.id)
            {
                // 同方案多模型共享账户额度——SUM 而非 MAX（MAX 会漏计并发燃烧）
                let credits = turbine_core::plans::plan_credits_used(&m.base_url, &m.id, *i, *c, *o);
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
    pub fn start_background(self: &AppState) {
        let flywheel = self.flywheel.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                flywheel.save();
            }
        });
        // 定期目录重扫（用户裁决）：每 5 分钟重新发现 agent 配置——新增自动进
        // 目录；删除不主动删（面板手动删）。每 60 分钟重拉远程 /models
        // （远端模型更新后网关及时跟进）。
        // AppState = Arc<Inner>，直接 clone 保持引用（网关生命周期内常驻）
        let state: AppState = self.clone();
        tokio::spawn(async move {
            let mut scan_tick = tokio::time::interval(Duration::from_secs(300));
            scan_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut remote_tick = tokio::time::interval(Duration::from_secs(3600));
            remote_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = scan_tick.tick() => {
                        state.rescan_discovery();
                    }
                    _ = remote_tick.tick() => {
                        state.refresh_remote_models();
                    }
                }
            }
        });
    }

    /// 重新扫描 agent 配置：新增模型合入 catalog，删除的保留（用户面板手动删）
    pub fn rescan_discovery(&self) {
        let discovered = turbine_discovery::discover(&self.config.discovery.agents);
        let removed: std::collections::HashSet<String> =
            load_overrides(&self.config.data.dir).removed_base_urls.into_iter().collect();
        let mut models = self.engine.catalog_snapshot();
        let mut added = 0usize;
        for d in discovered {
            if removed.contains(&d.base_url) {
                continue;
            }
            if !models.iter().any(|m| m.id == d.id) {
                tracing::info!(model = %d.id, "discovery rescan: new model added");
                models.push(d);
                added += 1;
            }
        }
        if added > 0 {
            self.engine.replace_catalog(models);
            tracing::info!(added, "discovery rescan complete");
        }
    }

    /// 重拉远程 /models：新模型合入，已删的不动
    pub fn refresh_remote_models(&self) {
        let base_records: Vec<turbine_core::types::ModelRecord> = self.engine.catalog_snapshot();
        let self_origin = format!("http://127.0.0.1:{}", self.config.server.port);
        let fresh = turbine_discovery::remote::discover_remote_blocking(
            &base_records,
            &self.config.data.dir,
            Some(&self_origin),
        );
        let removed: std::collections::HashSet<String> =
            load_overrides(&self.config.data.dir).removed_base_urls.into_iter().collect();
        let mut models = base_records;
        let mut added = 0usize;
        for r in fresh {
            if removed.contains(&r.base_url) {
                continue;
            }
            if !models.iter().any(|m| m.id == r.id) {
                models.push(r);
                added += 1;
            }
        }
        if added > 0 {
            self.engine.replace_catalog(models);
            tracing::info!(added, "remote models refresh: new models added");
        }
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
        .route("/api/providers",
            axum::routing::get(crate::meta::api_providers)
                .post(crate::meta::api_providers_set))
        .route("/api/providers/delete", axum::routing::post(crate::meta::api_providers_delete))
        .route("/api/providers/refresh", axum::routing::post(crate::meta::api_providers_refresh))
        .route("/api/providers/status", axum::routing::get(crate::meta::api_providers_status))
                .route("/api/providers/scan", axum::routing::post(crate::meta::api_providers_scan))
        .route("/api/providers/scan-progress", axum::routing::get(crate::meta::api_providers_scan_progress))
        .route("/api/providers/restore", axum::routing::post(crate::meta::api_providers_restore))
        .route("/api/providers/reconcile", axum::routing::post(crate::meta::api_providers_reconcile))
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
    /// 手动新增/修改的 provider 定义（面板 Provider 管理卡写入口）
    #[serde(default)]
    pub providers: HashMap<String, ProviderDef>,
    /// 用户在面板主动删除的模型 id（定期扫描不再自动加回）
    #[serde(default)]
    pub deleted_models: Vec<String>,
    /// 用户在面板删除的发现 provider（按 base_url 记忆，重扫不加回；恢复即移出此表）
    #[serde(default)]
    pub removed_base_urls: Vec<String>,
}

/// 手动 provider 定义——用户在面板新增/修改，与扫描发现并存
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct ProviderDef {
    pub base_url: String,
    pub api_key: Option<String>,
    /// 身份指纹（用户裁决：apiKey+baseUrl 唯一标识 provider）。编辑发现
    /// provider 而未重填 key 时回填其现有 key 指纹——同端点不同 key 的账号
    /// （如 opencode-go 与 opencode-go-github）不因此被遮蔽
    #[serde(default)]
    pub key_fp: Option<String>,
    /// openai / anthropic
    #[serde(default = "default_protocol")]
    pub protocol: String,
    /// coding / agent / go / api
    #[serde(default = "default_plan_kind")]
    pub plan_kind: String,
    #[serde(default)]
    pub docs_url: String,
    /// 档位：lite/pro/max/go/go-plus 等
    #[serde(default)]
    pub tier: String,
    /// 各窗口额度（积分/次数）——5h/weekly/monthly
    #[serde(default)]
    pub window_5h: Option<f64>,
    #[serde(default)]
    pub window_weekly: Option<f64>,
    #[serde(default)]
    pub window_monthly: Option<f64>,
    /// 模型级系数表（"model|in,cached,out" 多行）
    #[serde(default)]
    pub model_rates: String,
    /// 远程拉取到的模型（缓存）
    #[serde(default)]
    pub fetched_models: Vec<String>,
    /// 删除标记（软删除，面板可恢复）
    #[serde(default)]
    pub disabled: bool,
}

fn default_protocol() -> String { "openai".into() }
fn default_plan_kind() -> String { "api".into() }

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
