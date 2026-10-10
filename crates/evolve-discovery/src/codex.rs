//! codex configuration discovery (~/.codex/config.toml).
//!
//! codex selects one model via top-level `model` + `model_provider`; the
//! provider table carries base_url / env_key / wire_api. We surface the
//! configured pair as a single discovered model per provider entry. The
//! gateway only supports wire_api = "chat" (Responses API lands later).

use evolve_core::types::*;
use std::path::PathBuf;

pub fn default_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        paths.push(PathBuf::from(home).join(".codex").join("config.toml"));
    }
    paths
}

pub fn discover_from_text(text: &str) -> anyhow::Result<Vec<ModelRecord>> {
    let cfg: toml::Value = toml::from_str(text)?;
    let Ok(_table) = cfg.as_table().ok_or(()).cloned() else {
        return Ok(Vec::new());
    };
    let default_model = cfg.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string();
    let active_provider = cfg.get("model_provider").and_then(|m| m.as_str()).unwrap_or("openai").to_string();

    let mut records = Vec::new();
    if let Some(providers) = cfg.get("model_providers").and_then(|p| p.as_table()) {
        for (provider_id, pv) in providers {
            let wire_api = pv.get("wire_api").and_then(|w| w.as_str()).unwrap_or("chat");
            if wire_api != "chat" {
                tracing::warn!(provider = %provider_id, wire_api, "codex provider skipped: only wire_api=chat is supported");
                continue;
            }
            let base_url = pv
                .get("base_url")
                .and_then(|b| b.as_str())
                .map(|s| s.trim_end_matches('/').to_string())
                .unwrap_or_default();
            if base_url.is_empty() {
                continue;
            }
            let api_key = pv
                .get("env_key")
                .and_then(|e| e.as_str())
                .and_then(|env| std::env::var(env).ok())
                .filter(|k| !k.is_empty());

            // codex keeps a single active model per provider; use the
            // top-level model when this is the active provider.
            let model = if provider_id == &active_provider && !default_model.is_empty() {
                default_model.clone()
            } else {
                continue;
            };
            records.push(ModelRecord {
                id: format!("codex/{model}"),
                provider: format!("codex-{provider_id}"),
                plan: false,
                currency: evolve_core::types::infer_currency(&format!("codex-{provider_id}")).to_string(),
                protocol: Protocol::OpenAI,
                base_url,
                api_key_env: None,
                api_key,
                keys: vec![],

                upstream_model: model,
                context_window: None,
                max_output: 8192,
                cost: None,
                tiers: Tiers::default(),
                tiers_explicit: false,
                speed_tier: 0.6,
                weight: None,
                source: Source::Discovered,
                source_note: Some("codex".into()),
            });
        }
    }
    Ok(records)
}

pub fn discover_default() -> Vec<ModelRecord> {
    let mut out = Vec::new();
    for p in default_config_paths() {
        if p.is_file() {
            match std::fs::read_to_string(&p)
                .map_err(anyhow::Error::from)
                .and_then(|t| discover_from_text(&t))
            {
                Ok(mut models) => {
                    tracing::info!(path = %p.display(), count = models.len(), "discovered codex models");
                    out.append(&mut models);
                }
                Err(e) => tracing::warn!(path = %p.display(), error = %e, "codex discovery failed"),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"
model = "gpt-5.3"
model_provider = "evolve"

[model_providers.evolve]
name = "EvolveRouter"
base_url = "http://127.0.0.1:8787/v1"
wire_api = "chat"
env_key = "MODELROUTE_KEY"

[model_providers.ollama]
name = "Ollama"
base_url = "http://localhost:11434/v1"
wire_api = "chat"
"#;

    #[test]
    fn parses_active_provider_pair() {
        // std::env::remove_var is unsafe in edition 2024; env access in
        // discovery happens per-request from the gateway binary instead.
        // (No MODELROUTE_KEY is set in CI; api_key stays None.)
        let models = discover_from_text(FIXTURE).unwrap();
        assert_eq!(models.len(), 1);
        let m = &models[0];
        assert_eq!(m.id, "codex/gpt-5.3");
        assert_eq!(m.base_url, "http://127.0.0.1:8787/v1");
        assert_eq!(m.source, Source::Discovered);
        assert!(m.source_note.as_deref() == Some("codex"));
    }
}
