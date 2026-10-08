use crate::config::FileConfig;
use crate::types::*;
use std::collections::HashMap;

pub struct Catalog {
    pub models: Vec<ModelRecord>,
    index: HashMap<String, usize>,
}

impl Catalog {
    pub fn from_records(models: Vec<ModelRecord>) -> Self {
        let mut models = models;
        for m in models.iter_mut() {
            if m.currency.is_empty() {
                m.currency = infer_currency(&m.provider).to_string();
            }
        }
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
        let user: Vec<ModelRecord> = file
            .model_records()
            .into_iter()
            .filter(|m| m.provider != "modelroute")
            .collect();
        let _user_ids: Vec<&str> = user.iter().map(|m| m.id.as_str()).collect();
        // user light-weight overrides (e.g. only weight) inherit connection
        // facts from the discovered layer with the same id — the discovered
        // facts also originate from user agent configs (decision #22)
        let disc_index: HashMap<&str, &ModelRecord> =
            discovered.iter().map(|m| (m.id.as_str(), m)).collect();
        let user: Vec<ModelRecord> = user
            .into_iter()
            .map(|mut u| {
                if let Some(d) = disc_index.get(u.id.as_str()) {
                    if u.base_url.is_empty() {
                        u.base_url = d.base_url.clone();
                    }
                    if u.api_key.is_none() && u.keys.is_empty() {
                        u.api_key = d.api_key.clone();
                        u.keys = d.keys.clone();
                    }
                    if u.context_window.is_none() {
                        u.context_window = d.context_window;
                    }
                    if u.cost.is_none() {
                        u.cost = d.cost;
                    }
                    if u.upstream_model.is_empty() {
                        u.upstream_model = d.upstream_model.clone();
                    }
                }
                u
            })
            .collect();
        let user_ids: Vec<&str> = user.iter().map(|m| m.id.as_str()).collect();
        let mut models: Vec<ModelRecord> = Vec::new();
        for d in discovered {
            if d.provider == "modelroute" {
                continue;
            }
            if !user_ids.contains(&d.id.as_str()) {
                models.push(d);
            }
        }
        let present: Vec<String> =
            models.iter().map(|m| m.id.clone()).chain(user.iter().map(|m| m.id.clone())).collect();
        if file.catalog.builtin_priors {
            for b in builtin_catalog() {
                if !present.iter().any(|id| id == &b.id) {
                    models.push(b);
                }
            }
        }
        models.extend(user);
        for m in models.iter_mut() {
            if m.currency.is_empty() {
                m.currency = infer_currency(&m.provider).to_string();
            }
            // 订阅方案自动标记：baseUrl 命中积分制/订阅接入（Coding/Agent/Go Plan）
            // 的模型默认 plan=true —— 成本因子才能按官方系数折算配额消耗速率
            if !m.plan && crate::plans::plan_for(&m.base_url).is_some() {
                m.plan = true;
            }
        }
        let index = models.iter().enumerate().map(|(i, m)| (m.id.clone(), i)).collect();
        Self { models, index }
    }

    pub fn get(&self, id: &str) -> Option<&ModelRecord> {
        self.index.get(id).map(|&i| &self.models[i])
    }

    /// 面板权重调控：热更新模型用户权重（overrides.json 持久化后重启重放）
    pub fn update_weight(&mut self, id: &str, weight: f32) {
        if let Some(&i) = self.index.get(id) {
            self.models[i].weight = Some(weight.clamp(0.2, 3.0));
        }
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
