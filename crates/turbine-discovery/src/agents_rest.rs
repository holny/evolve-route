//! Remaining agent discovery: openclaw, hermes, dsh.
//! Each is best-effort scanning of well-known config paths; connection
//! facts always come from the user's own config (decision record #22).

use turbine_core::types::*;
use serde_json::Value;
use std::path::PathBuf;

pub fn discover_default() -> Vec<ModelRecord> {
    let mut out = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        let openclaw = PathBuf::from(&home).join(".openclaw").join("openclaw.json");
        if openclaw.is_file() {
            match std::fs::read_to_string(&openclaw).map_err(anyhow::Error::from).and_then(|t| {
                let v: Value = serde_json::from_str(&t)?;
                Ok(extract_openclaw(&v))
            }) {
                Ok(mut models) => out.append(&mut models),
                Err(e) => tracing::debug!(error = %e, "openclaw discovery skipped"),
            }
        }
        let hermes = PathBuf::from(&home).join(".hermes").join("config.yaml");
        if hermes.is_file() {
            match std::fs::read_to_string(&hermes).map_err(anyhow::Error::from).and_then(|t| {
                let v: serde_json::Value = serde_yaml::from_str(&t)?;
                Ok(extract_hermes(&v))
            }) {
                Ok(Some(mut m)) => out.append(&mut m),
                Ok(None) => {}
                Err(e) => tracing::debug!(error = %e, "hermes discovery skipped"),
            }
        }
    }
    // pi: optional ~/.pi/models.json with {providers:{id:{baseUrl,models}}}
    if let Ok(home) = std::env::var("HOME") {
        let pi = PathBuf::from(&home).join(".pi").join("models.json");
        if pi.is_file() {
            match std::fs::read_to_string(&pi).map_err(anyhow::Error::from).and_then(|t| {
                let v: Value = serde_json::from_str(&t)?;
                Ok(extract_openclaw(&v)) // same tolerant baseUrl+models shape
            }) {
                Ok(mut models) => out.append(&mut models),
                Err(e) => tracing::debug!(error = %e, "pi discovery skipped"),
            }
        }
    }
    // dsh: env-based configuration
    if let (Ok(base), Ok(model)) = (std::env::var("DEEPSEEK_BASE_URL"), std::env::var("DSH_MODEL"))
        && !base.is_empty() && !model.is_empty() {
            out.push(ModelRecord {
                id: format!("dsh/{model}"),
                provider: "dsh".into(),
                protocol: Protocol::OpenAI,
                base_url: base.trim_end_matches('/').to_string(),
                upstream_model: model,
                context_window: None,
                max_output: 8192,
                tiers: Tiers::default(),
                speed_tier: 0.6,
                source: Source::Discovered,
                source_note: Some("dsh env".into()),
                ..Default::default()
            });
        }
    out
}

/// openclaw: walk the JSON for provider objects that declare both a
/// baseUrl and a models map/array; surface each model.
fn extract_openclaw(root: &Value) -> Vec<ModelRecord> {
    let mut out = Vec::new();
    walk_providers(root, &mut |name, pv| {
        let base_url = pv.get("baseUrl").and_then(|b| b.as_str()).map(|s| s.trim_end_matches('/').to_string());
        let Some(base_url) = base_url else { return };
        let models: Vec<String> = pv
            .get("models")
            .and_then(|m| {
                m.as_array().map(|a| {
                    a.iter()
                        .map(|x| x.as_str().unwrap_or("").to_string())
                        .collect()
                })
            })
            .unwrap_or_default();
        for model in models {
            if model.is_empty() {
                continue;
            }
            out.push(ModelRecord {
                id: format!("{name}/{model}"),
                provider: name.to_string(),
                protocol: Protocol::OpenAI,
                base_url: base_url.clone(),
                upstream_model: model,
                context_window: None,
                max_output: 8192,
                tiers: Tiers::default(),
                speed_tier: 0.6,
                source: Source::Discovered,
                source_note: Some("openclaw".into()),
                ..Default::default()
            });
        }
    });
    out
}

fn walk_providers(v: &Value, f: &mut impl FnMut(&str, &Value)) {
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if val.get("baseUrl").is_some() && val.get("models").is_some() {
                f(k, val);
            }
            walk_providers(val, f);
        }
    }
}

/// hermes config.yaml: model.base_url + model.default (+ provider).
fn extract_hermes(v: &Value) -> Option<Vec<ModelRecord>> {
    let model_section = v.get("model")?;
    let base_url = model_section.get("base_url")?.as_str()?.trim_end_matches('/').to_string();
    let default = model_section.get("default").and_then(|d| d.as_str())?;
    let provider = model_section.get("provider").and_then(|p| p.as_str()).unwrap_or("hermes");
    Some(vec![ModelRecord {
        id: format!("{provider}/{default}"),
        provider: provider.to_string(),
        protocol: Protocol::OpenAI,
        base_url,
        upstream_model: default.to_string(),
        context_window: None,
        max_output: 8192,
        tiers: Tiers::default(),
        speed_tier: 0.6,
        source: Source::Discovered,
        source_note: Some("hermes".into()),
        ..Default::default()
    }])
}
