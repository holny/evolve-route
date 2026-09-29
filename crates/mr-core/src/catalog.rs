use crate::config::FileConfig;
use crate::types::*;
use std::collections::HashMap;

pub struct Catalog {
    pub models: Vec<ModelRecord>,
    index: HashMap<String, usize>,
}

impl Catalog {
    pub fn from_records(models: Vec<ModelRecord>) -> Self {
        let index = models.iter().enumerate().map(|(i, m)| (m.id.clone(), i)).collect();
        Self { models, index }
    }

    pub fn build(file: &FileConfig) -> Self {
        Self::build_with_discovered(file, Vec::new())
    }

    /// Merge priority per id: user (modelroute.toml) > discovered (agent
    /// configs) > builtin priors. Connection facts always come from the
    /// user config layer.
    pub fn build_with_discovered(file: &FileConfig, discovered: Vec<ModelRecord>) -> Self {
        let user: Vec<ModelRecord> = file.model_records();
        let user_ids: Vec<&str> = user.iter().map(|m| m.id.as_str()).collect();
        let mut models: Vec<ModelRecord> = Vec::new();
        for d in discovered {
            if !user_ids.contains(&d.id.as_str()) {
                models.push(d);
            }
        }
        let present: Vec<String> =
            models.iter().map(|m| m.id.clone()).chain(user.iter().map(|m| m.id.clone())).collect();
        for b in builtin_catalog() {
            if !present.iter().any(|id| id == &b.id) {
                models.push(b);
            }
        }
        models.extend(user);
        let index = models.iter().enumerate().map(|(i, m)| (m.id.clone(), i)).collect();
        Self { models, index }
    }

    pub fn get(&self, id: &str) -> Option<&ModelRecord> {
        self.index.get(id).map(|&i| &self.models[i])
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

fn builtin_catalog() -> Vec<ModelRecord> {
    let mk = |id: &str, provider: &str, window: u64, cost: Cost, tiers: Tiers, speed: f32| ModelRecord {
        id: id.into(),
        provider: provider.into(),
        base_url: String::new(),
        upstream_model: id.into(),
        context_window: Some(window),
        max_output: 8192,
        cost: Some(cost),
        tiers,
        speed_tier: speed,
        source: Source::Builtin,
        ..Default::default()
    };
    vec![
        mk(
            "deepseek-chat",
            "deepseek",
            64_000,
            Cost { input: 0.27, output: 1.1 },
            Tiers { reasoning: 0.45, coding: 0.8, vision: 0.0, agentic: 0.75 },
            0.85,
        ),
        mk(
            "deepseek-reasoner",
            "deepseek",
            64_000,
            Cost { input: 0.55, output: 2.19 },
            Tiers { reasoning: 0.85, coding: 0.85, vision: 0.0, agentic: 0.7 },
            0.45,
        ),
        mk(
            "claude-sonnet",
            "anthropic",
            200_000,
            Cost { input: 3.0, output: 15.0 },
            Tiers { reasoning: 0.9, coding: 0.95, vision: 0.9, agentic: 0.95 },
            0.65,
        ),
        mk(
            "gpt-5-mini",
            "openai",
            128_000,
            Cost { input: 0.25, output: 2.0 },
            Tiers { reasoning: 0.7, coding: 0.75, vision: 0.6, agentic: 0.75 },
            0.8,
        ),
    ]
}
