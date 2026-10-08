use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    OpenAI,
    Anthropic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    #[default]
    Builtin,
    User,
    Discovered,
    Remote,
}

impl Source {
    pub fn label(&self) -> &'static str {
        match self {
            Source::Builtin => "builtin",
            Source::User => "user",
            Source::Discovered => "discovered",
            Source::Remote => "remote",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Cost {
    pub input: f32,
    pub output: f32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Tiers {
    pub reasoning: f32,
    pub coding: f32,
    pub vision: f32,
    pub agentic: f32,
}

impl Default for Tiers {
    fn default() -> Self {
        Self { reasoning: 0.5, coding: 0.5, vision: 0.0, agentic: 0.5 }
    }
}

/// One credential slot for a model (multi-key pool, decision record #12).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeySlot {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelRecord {
    pub id: String,
    pub provider: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    /// Resolved key pool: first = primary, rest rotate on quota/auth errors.
    pub keys: Vec<KeySlot>,
    pub upstream_model: String,
    pub context_window: Option<u64>,
    pub max_output: u64,
    pub cost: Option<Cost>,
    pub tiers: Tiers,
    /// True when the user explicitly declared tiers (benchmarks blend at 0.3)
    pub tiers_explicit: bool,
    /// 订阅套餐标志：plan 模型边际成本≈0，配额窗口内优先（用掉才值）
    pub plan: bool,
    /// 计价货币：CNY / USD（缺省按厂商推断）
    pub currency: String,
    pub speed_tier: f32,
    /// User bias weight (participates in scoring, never decisive).
    pub weight: Option<f32>,
    pub source: Source,
    pub source_note: Option<String>,
}

impl Default for ModelRecord {
    fn default() -> Self {
        Self {
            id: String::new(),
            provider: "custom".into(),
            protocol: Protocol::OpenAI,
            base_url: String::new(),
            api_key_env: None,
            api_key: None,
            keys: Vec::new(),
            upstream_model: String::new(),
            context_window: None,
            max_output: 8192,
            cost: None,
            tiers: Tiers::default(),
            tiers_explicit: false,
            plan: false,
            currency: String::new(),
            speed_tier: 0.6,
            weight: None,
            source: Source::User,
            source_note: None,
        }
    }
}

/// 国内厂商按人民币计价；其余默认美元（可被配置显式覆盖）
pub fn infer_currency(provider: &str) -> &'static str {
    let p = provider.to_lowercase();
    for cn in ["zhipu", "deepseek", "minimax", "volces", "volcengine", "ark", "moonshot", "kimi",
               "qwen", "dashscope", "alibaba", "baidu", "ernie", "tencent", "hunyuan",
               "stepfun", "01ai", "baichuan", "sensetime", "doubao"] {
        if p.contains(cn) {
            return "CNY";
        }
    }
    "USD"
}

impl ModelRecord {
    pub fn has_credential(&self) -> bool {
        !self.keys.is_empty()
            || self.api_key.as_deref().map(|k| !k.is_empty()).unwrap_or(false)
            || self.base_url.contains("127.0.0.1")
            || self.base_url.contains("localhost")
    }

    /// Resolved key values in rotation order (api_keys_env first, then the
    /// legacy single api_key so old configs keep working).
    pub fn key_values(&self) -> Vec<String> {
        let mut v: Vec<String> = self.keys.iter().map(|k| k.value.clone()).collect();
        if v.is_empty()
            && let Some(k) = self.api_key.as_deref().filter(|k| !k.is_empty()) {
                v.push(k.to_string());
            }
        v
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Domain {
    Code,
    MathLogic,
    Writing,
    Lookup,
    Data,
    Chitchat,
    AgentOps,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RequestFeatures {
    pub est_input_tokens: u64,
    pub user_text_chars: usize,
    pub code_density: f32,
    pub tool_count: usize,
    pub tool_ratio: f32,
    pub has_images: bool,
    pub turn_count: usize,
    pub cjk_ratio: f32,
}

#[derive(Debug, Clone, Default)]
pub struct DigestSignals {
    pub first_user_text: String,
    pub last_user_text: String,
    pub overlap_ratio: f32,
    pub has_deixis: bool,
    pub topic_shift_marker: bool,
    pub session_tools_seen: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct JudgmentSet {
    pub domain: Domain,
    pub domain_confidence: f32,
    pub difficulty: f32,
    pub difficulty_confidence: f32,
    pub needs_vision: f32,
    pub is_trivial: f32,
    pub tool_heavy: f32,
    pub high_stakes: f32,
    pub session_relevance: f32,
    pub session_depth: f32,
    /// 谁做的判定：decision_model / decision_model+rules / heuristic /
    /// sticky / explicit——进事件供面板展示判定来源
    pub judge_source: &'static str,
}

pub trait Judge: Send + Sync {
    fn judge(&self, features: &RequestFeatures, digest: &DigestSignals) -> JudgmentSet;
}

#[derive(Debug, Clone, PartialEq)]
pub struct StickyState {
    pub chosen: String,
    pub est_tokens_band: i32,
    pub turns_left: u32,
    pub tools_sig: u64,
    pub domain: Domain,
    /// 上次判定的有效难度：粘性只对低难度任务延续（L2 上限）
    pub difficulty: f32,
    /// 上次请求的估算输入 tokens：粘性量级保护上限（2 倍暴增/减半即断）
    pub est_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FilteredOut {
    pub model: String,
    pub cause: String,
}

/// 逐候选评分分量（决策透明）：与公式权重一同随事件暴露
#[derive(Debug, Clone, Serialize)]
pub struct CandidateScore {
    pub model_id: String,
    pub score: f32,
    pub q: f32,
    pub s: f32,
    pub c: f32,
    pub r: f32,
    pub h: f32,
    /// 用户配置权重（clamp 后）
    pub uw: f32,
    /// 飞轮学习偏置
    pub bias: f32,
    /// 订阅方案预算压制系数（1.0=无压制，<1=预算软阈值降权）
    pub qp: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub id: String,
    pub chosen: String,
    pub upstream_model: String,
    pub chain: Vec<String>,
    pub reason: String,
    pub scores: BTreeMap<String, f32>,
    /// 逐候选评分分量（榜单质量/速度/成本/可靠/余量/用户权重/学习偏置/总分）
    /// ——决策透明的数据地基，随事件暴露给面板评分矩阵
    pub scored: Vec<CandidateScore>,
    /// 本次决策生效的公式权重（normalized [质量,速度,成本,可靠,余量]）
    pub weights: [f32; 5],
    pub judgment: JudgmentSet,
    pub filtered: Vec<FilteredOut>,
    /// 决策漏斗：目录总数→硬约束后→质量及格后→评分排序→选中
    pub funnel: [u32; 4],
    pub sticky: bool,
    pub est_input_tokens: u64,
    pub difficulty_eff: f32,
    /// 任务判定阶段耗时（决策模型 LLM 调用 + 规则融合）；粘性路径为 0
    pub judge_ms: u64,
}

impl Decision {
    pub fn header_reason(&self) -> String {
        self.reason.chars().map(|c| if c.is_ascii() { c } else { ' ' }).collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyWeights {
    pub quality: f32,
    pub speed: f32,
    pub cost: f32,
    pub stability: f32,
    pub headroom: f32,
}

impl Default for PolicyWeights {
    fn default() -> Self {
        Self::balanced()
    }
}

impl PolicyWeights {
    pub fn balanced() -> Self {
        Self { quality: 0.35, speed: 0.15, cost: 0.25, stability: 0.15, headroom: 0.10 }
    }
    pub fn cost() -> Self {
        Self { quality: 0.25, speed: 0.10, cost: 0.50, stability: 0.10, headroom: 0.05 }
    }
    pub fn quality() -> Self {
        Self { quality: 0.55, speed: 0.15, cost: 0.10, stability: 0.15, headroom: 0.05 }
    }

    pub fn normalized(&self) -> [f32; 5] {
        let sum = (self.quality + self.speed + self.cost + self.stability + self.headroom).max(1e-6);
        [
            self.quality / sum,
            self.speed / sum,
            self.cost / sum,
            self.stability / sum,
            self.headroom / sum,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyProfile {
    Balanced,
    Cost,
    Quality,
}

impl PolicyProfile {
    pub fn weights(&self) -> PolicyWeights {
        match self {
            PolicyProfile::Balanced => PolicyWeights::balanced(),
            PolicyProfile::Cost => PolicyWeights::cost(),
            PolicyProfile::Quality => PolicyWeights::quality(),
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "balanced" => Some(Self::Balanced),
            "cost" | "cheap" => Some(Self::Cost),
            "quality" | "best" => Some(Self::Quality),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthKind {
    Ok,
    AuthFailed,
    PaymentRequired,
    QuotaExhausted,
    RateLimited,
    Unsupported,
    ContextOverflow,
    Transient,
}

impl HealthKind {
    pub fn label(&self) -> &'static str {
        match self {
            HealthKind::Ok => "ok",
            HealthKind::AuthFailed => "auth failed",
            HealthKind::PaymentRequired => "no credit",
            HealthKind::QuotaExhausted => "quota exhausted",
            HealthKind::RateLimited => "rate limited",
            HealthKind::Unsupported => "model unsupported",
            HealthKind::ContextOverflow => "context overflow",
            HealthKind::Transient => "transient error",
        }
    }
    pub fn default_cooldown_ms(&self) -> u64 {
        match self {
            HealthKind::Ok => 0,
            HealthKind::AuthFailed | HealthKind::PaymentRequired => 30 * 60 * 1000,
            HealthKind::Unsupported => 24 * 60 * 60 * 1000,
            HealthKind::ContextOverflow => 2 * 60 * 1000,
            HealthKind::QuotaExhausted => 5 * 60 * 1000,
            HealthKind::RateLimited => 60 * 1000,
            HealthKind::Transient => 30 * 1000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HealthEntry {
    pub kind: HealthKind,
    pub until_epoch_ms: Option<u64>,
    pub message: String,
    pub hits: u32,
    pub updated_at: u64,
}

impl HealthEntry {
    pub fn available(&self, now_epoch_ms: u64) -> bool {
        match self.kind {
            HealthKind::Ok => true,
            _ => self.until_epoch_ms.map(|t| now_epoch_ms >= t).unwrap_or(false),
        }
    }
    pub fn cooldown_remaining_ms(&self, now_epoch_ms: u64) -> Option<u64> {
        self.until_epoch_ms.map(|t| t.saturating_sub(now_epoch_ms))
    }
}

pub type HealthMap = std::collections::HashMap<String, HealthEntry>;

/// 模型级可用性：HealthMap 键是 `模型⟨sep⟩keyidx`（多 key 池）或裸模型 id。
/// 任一 key 可用即视为可用；该模型无任何记录视为可用。
pub fn model_available(map: &HealthMap, id: &str, now_epoch_ms: u64) -> bool {
    let mut seen = false;
    for (k, e) in map {
        if k.split('\u{1f}').next() == Some(id) {
            seen = true;
            if e.available(now_epoch_ms) {
                return true;
            }
        }
    }
    !seen
}

/// Observed per-model telemetry snapshot fed back into scoring (flywheel).
#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelTelemetry {
    pub reliability: Option<f32>,
    pub speed_obs: Option<f32>,
    pub calibration: Option<f32>,
    /// Flywheel-learned bias: realized success vs catalog median, clamped.
    pub learned_bias: Option<f32>,
    /// Window lower-bound inference: largest prompt tokens a successful
    /// request actually accepted (for unknown-window models).
    pub max_accepted: Option<u64>,
    /// 请求样本数（含冷启动模型）——探索目标选择用
    pub samples: Option<u32>,
    /// 近 30 次请求实测可靠性（≥5 样本有效）——路由修正权重 30%
    pub rel_30: Option<f32>,
    /// 近 10 次请求实测可靠性（≥3 样本有效）——路由修正权重 20%
    pub rel_10: Option<f32>,
    /// 近期请求样本环（时间窗口聚合 + 趋势图）
    pub recent: Option<Vec<ReqSample>>,
}

pub type TelemetrySnapshot = std::collections::HashMap<String, ModelTelemetry>;

/// 单次请求样本（模型动态窗口聚合 + 趋势图数据源）
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ReqSample {
    /// 请求完成时刻（epoch ms）
    pub ts: u64,
    pub ttft_ms: u64,
    pub total_ms: u64,
    pub in_tok: u64,
    pub cached_tok: u64,
    pub out_tok: u64,
    pub ok: bool,
    pub tools_total: u32,
    pub tools_ok: u32,
}

/// Learned quota remaining (tokens) per model, best key across pool.
pub type QuotaView = std::collections::HashMap<String, u64>;

/// Gateway-side response quality analysis (L2 signals, no plugin needed).
#[derive(Debug, Clone, Default, Serialize)]
pub struct ResponseQuality {
    pub finish_reason: Option<String>,
    pub content_chars: usize,
    pub tool_calls_total: u32,
    pub tool_calls_valid_json: u32,
    pub tool_calls_known_name: u32,
    pub tool_calls_schema_ok: u32,
    /// finish_reason == length (truncated)
    pub truncated: bool,
    /// empty content with no tool calls and finish=stop
    pub empty_response: bool,
    /// repetition loop / refusal / garbage heuristics
    pub degenerate: bool,
    pub flags: Vec<String>,
}

impl ResponseQuality {
    pub fn syntactic_ok(&self) -> bool {
        self.tool_calls_total == 0
            || (self.tool_calls_valid_json == self.tool_calls_total
                && self.tool_calls_known_name == self.tool_calls_total)
    }
}

/// 双判官取严融合（决策记录 #24 扩展）：Jev 语义判定为主，heuristic 关键词
/// 为独立第二意见；逐字段取更保守值。专治 Jev 的 CJK 弱点——中文任务被
/// 误判为 Other/简单时，heuristic 把域和难度救回来。
pub fn fuse_judgments(jev: &JudgmentSet, heur: &JudgmentSet) -> JudgmentSet {
    let take_max = |a: f32, b: f32| a.max(b);
    // 域冲突：Jev 说 Other 而 heuristic 有明确域 -> 采信 heuristic（CJK 救援）
    let domain = if jev.domain == Domain::Other && heur.domain != Domain::Other {
        heur.domain
    } else if jev.domain_confidence >= heur.domain_confidence {
        jev.domain
    } else {
        heur.domain
    };
    JudgmentSet {
        domain,
        // 难度融合改均值：take_max 会把难度钉死在上限（双峰根因之一），
        // 双判官各给独立估计，均值更接近真实分布
        difficulty: (jev.difficulty + heur.difficulty) * 0.5,
        domain_confidence: jev.domain_confidence.min(heur.domain_confidence),
        difficulty_confidence: jev.difficulty_confidence.min(heur.difficulty_confidence),
        needs_vision: take_max(jev.needs_vision, heur.needs_vision),
        is_trivial: jev.is_trivial.min(heur.is_trivial),
        tool_heavy: take_max(jev.tool_heavy, heur.tool_heavy),
        high_stakes: take_max(jev.high_stakes, heur.high_stakes),
        // 会话相关性取严：任一判官认为跑题即按跑题处理
        session_relevance: jev.session_relevance.min(heur.session_relevance),
        session_depth: take_max(jev.session_depth, heur.session_depth),
        judge_source: "decision_model+rules",
    }
}

/// v2 直接路由顾问：决策模型看到候选全画像后直接推荐模型。
/// 与 Judge（语义判定器）不同——RouteAdvisor 的输出就是路由决定。
pub trait RouteAdvisor: Send + Sync {
    fn recommend(
        &self,
        task_summary: &str,
        candidates_json: &str,
        policy: &str,
    ) -> Option<(String, f32)>;
}

#[cfg(test)]
mod currency_tests {
    use super::infer_currency;

    #[test]
    fn cn_vendors_bill_in_cny() {
        for p in ["zhipuai-coding-plan", "volcengine-coding-plan", "volces",
                  "deepseek", "MiniMax", "moonshot"] {
            assert_eq!(infer_currency(p), "CNY", "{p} 应计人民币");
        }
    }

    #[test]
    fn international_vendors_bill_in_usd() {
        for p in ["opencode-go", "openai", "anthropic", "zen"] {
            assert_eq!(infer_currency(p), "USD", "{p} 应计美元");
        }
    }
}
