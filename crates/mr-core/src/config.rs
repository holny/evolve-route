use crate::types::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ServerCfg {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PolicyCfg {
    pub default: String,
    pub weights: PolicyWeights,
    pub sticky_turns: u32,
    pub confidence_gate: f32,
}

impl Default for PolicyCfg {
    fn default() -> Self {
        Self {
            default: "balanced".into(),
            weights: PolicyWeights::balanced(),
            sticky_turns: 6,
            confidence_gate: 0.35,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct DecisionCfg {
    pub backend: String,
    pub redact: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct CatalogCfg {
    pub remote_fetch: bool,
    /// models.dev as reference-only metadata (never overrides user values)
    pub modelsdev_reference: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct QuotaCfg {
    pub learn_from_headers: bool,
}

/// External benchmark feed source (decision record #23).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct BenchSourceCfg {
    pub name: String,
    pub url: String,
    pub format: String,
    pub interval_hours: Option<u64>,
    pub headers: Option<HashMap<String, String>>,
    pub alias_key: Option<String>,
    pub score_key: Option<String>,
    pub max_score: Option<f32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BenchmarksCfg {
    pub enabled: bool,
    pub interval_hours: u64,
    pub sources: Vec<BenchSourceCfg>,
}

impl Default for BenchmarksCfg {
    fn default() -> Self {
        // enabled by default: the embedded curated seed applies cold-start
        // tiers offline; configured HTTP sources layer on top
        Self { enabled: true, interval_hours: 24, sources: Vec::new() }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct DataCfg {
    pub dir: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DiscoveryCfg {
    pub agents: Vec<String>,
}

impl Default for DiscoveryCfg {
    fn default() -> Self {
        Self { agents: vec!["opencode".into()] }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ModelEntry {
    pub id: String,
    pub provider: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub api_keys_env: Option<Vec<String>>,
    pub upstream_model: Option<String>,
    pub context_window: Option<u64>,
    pub max_output: u64,
    pub cost: Option<Cost>,
    pub tiers: Tiers,
    pub speed_tier: f32,
    pub weight: Option<f32>,
    pub source_note: Option<String>,
}

impl Default for ModelEntry {
    fn default() -> Self {
        let d = ModelRecord::default();
        Self {
            id: String::new(),
            provider: d.provider.clone(),
            protocol: d.protocol,
            base_url: d.base_url.clone(),
            api_key_env: None,
            api_keys_env: None,
            upstream_model: None,
            context_window: d.context_window,
            max_output: d.max_output,
            cost: None,
            tiers: d.tiers,
            speed_tier: d.speed_tier,
            weight: None,
            source_note: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct FileConfig {
    pub server: ServerCfg,
    pub policy: PolicyCfg,
    pub decision: DecisionCfg,
    pub catalog: CatalogCfg,
    pub quota: QuotaCfg,
    pub data: DataCfg,
    pub discovery: DiscoveryCfg,
    pub benchmarks: BenchmarksCfg,
    pub models: Vec<ModelEntry>,
}

impl FileConfig {
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    pub fn from_paths(candidates: &[std::path::PathBuf], embedded_default: &str) -> anyhow::Result<(Self, Option<std::path::PathBuf>)> {
        for p in candidates {
            if p.is_file() {
                let text = std::fs::read_to_string(p)?;
                return Ok((Self::parse(&text)?, Some(p.clone())));
            }
        }
        Ok((Self::parse(embedded_default)?, None))
    }

    pub fn resolve_model(&self, entry: &ModelEntry) -> ModelRecord {
        let upstream_model = entry
            .upstream_model
            .clone()
            .unwrap_or_else(|| entry.id.clone());
        let mut keys: Vec<KeySlot> = Vec::new();
        if let Some(envs) = &entry.api_keys_env {
            for (i, env) in envs.iter().enumerate() {
                if let Ok(v) = std::env::var(env)
                    && !v.is_empty() {
                        keys.push(KeySlot { label: format!("{env}[{i}]"), value: v });
                    }
            }
        }
        if keys.is_empty()
            && let Some(env) = &entry.api_key_env
                && let Ok(v) = std::env::var(env)
                    && !v.is_empty() {
                        keys.push(KeySlot { label: env.clone(), value: v });
                    }
        let api_key = entry.api_key_env.as_ref().map(|env| std::env::var(env).unwrap_or_default());
        ModelRecord {
            id: entry.id.clone(),
            provider: entry.provider.clone(),
            protocol: entry.protocol,
            base_url: entry.base_url.trim_end_matches('/').to_string(),
            api_key_env: entry.api_key_env.clone(),
            api_key,
            keys,
            upstream_model,
            context_window: entry.context_window,
            max_output: entry.max_output,
            cost: entry.cost,
            tiers: entry.tiers,
            tiers_explicit: entry.tiers != Tiers::default(),
            speed_tier: entry.speed_tier,
            weight: entry.weight,
            source: Source::User,
            source_note: entry.source_note.clone(),
        }
    }

    pub fn model_records(&self) -> Vec<ModelRecord> {
        self.models.iter().map(|m| self.resolve_model(m)).collect()
    }
}

pub fn default_config_paths() -> Vec<std::path::PathBuf> {
    let mut paths = vec![Path::new("modelroute.toml").to_path_buf()];
    if let Ok(home) = std::env::var("HOME") {
        paths.push(Path::new(&home).join(".modelroute").join("modelroute.toml"));
    }
    paths
}
