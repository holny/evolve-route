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
            speed_tier: 0.6,
            weight: None,
            source: Source::User,
            source_note: None,
        }
    }
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
}

#[derive(Debug, Clone, Serialize)]
pub struct FilteredOut {
    pub model: String,
    pub cause: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub id: String,
    pub chosen: String,
    pub upstream_model: String,
    pub chain: Vec<String>,
    pub reason: String,
    pub scores: BTreeMap<String, f32>,
    pub judgment: JudgmentSet,
    pub filtered: Vec<FilteredOut>,
    pub sticky: bool,
    pub est_input_tokens: u64,
    pub difficulty_eff: f32,
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

/// Observed per-model telemetry snapshot fed back into scoring (flywheel).
#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelTelemetry {
    pub reliability: Option<f32>,
    pub speed_obs: Option<f32>,
    pub calibration: Option<f32>,
    /// Flywheel-learned bias: realized success vs catalog median, clamped.
    pub learned_bias: Option<f32>,
}

pub type TelemetrySnapshot = std::collections::HashMap<String, ModelTelemetry>;

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
