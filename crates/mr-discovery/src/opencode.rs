//! opencode configuration discovery.
//!
//! Ground rules (user decision #22): connection facts — baseUrl, providerId,
//! modelId, credentials — come verbatim from the user's opencode config.
//! models.dev and any external catalog are reference-only for metadata and
//! must never override those fields.

use mr_core::types::*;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Default endpoints for well-known providers whose opencode entries rely on
/// built-in provider definitions (no explicit baseURL in user config).
pub fn provider_default_base_url(provider_id: &str) -> Option<&'static str> {
    match provider_id {
        "zhipuai-coding-plan" => Some("https://open.bigmodel.cn/api/coding/paas/v4"),
        "kimi-coding-plan" => Some("https://api.kimi.com/coding/v1"),
        "github-copilot" => Some("https://api.githubcopilot.com"),
        _ => None,
    }
}

pub fn default_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        let cfg = Path::new(&home).join(".config").join("opencode");
        paths.push(cfg.join("opencode.jsonc"));
        paths.push(cfg.join("opencode.json"));
        paths.push(Path::new(&home).join(".opencode.json"));
    }
    paths
}

/// Parse opencode JSON/JSONC text: strips comments and trailing commas
/// before feeding serde_json.
pub fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if i + 1 < bytes.len() && bytes[i + 1] == '/' => {
                while i < bytes.len() && bytes[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '/' if i + 1 < bytes.len() && bytes[i + 1] == '*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == '*' && bytes[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            _ => out.push(c),
        }
        i += 1;
    }
    // strip trailing commas
    let mut clean = String::with_capacity(out.len());
    let chars: Vec<char> = out.chars().collect();
    let mut in_str = false;
    let mut esc = false;
    for (idx, &c) in chars.iter().enumerate() {
        if in_str {
            clean.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        if c == '"' {
            in_str = true;
            clean.push(c);
            continue;
        }
        if c == ',' {
            let mut j = idx + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                continue;
            }
        }
        clean.push(c);
    }
    clean
}

pub fn discover_from_text(text: &str) -> anyhow::Result<Vec<ModelRecord>> {
    let v: Value = serde_json::from_str(&strip_jsonc(text))?;
    Ok(extract_models(&v))
}

pub fn discover_from_file(path: &Path) -> anyhow::Result<Vec<ModelRecord>> {
    let text = std::fs::read_to_string(path)?;
    discover_from_text(&text)
}

pub fn discover_default() -> Vec<ModelRecord> {
    let mut out = Vec::new();
    for p in default_config_paths() {
        if p.is_file() {
            match discover_from_file(&p) {
                Ok(mut models) => {
                    tracing::info!(path = %p.display(), count = models.len(), "discovered opencode models");
                    out.append(&mut models);
                }
                Err(e) => tracing::warn!(path = %p.display(), error = %e, "opencode discovery failed"),
            }
        }
    }
    out
}

fn extract_models(root: &Value) -> Vec<ModelRecord> {
    let Some(providers) = root.get("provider").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for (provider_id, pv) in providers {
        // "modelroute" is the reserved self-adapter name: client configs
        // point at our own gateway, never an upstream
        if provider_id == "modelroute" {
            continue;
        }
        let base_url = pv
            .get("options")
            .and_then(|o| o.get("baseURL"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim_end_matches('/').to_string())
            .or_else(|| pv.get("api").and_then(|v| v.as_str()).map(|s| s.trim_end_matches('/').to_string()))
            .or_else(|| provider_default_base_url(provider_id).map(|s| s.to_string()));

        let api_key = pv
            .get("options")
            .and_then(|o| o.get("apiKey"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                pv.get("env")
                    .and_then(|e| e.as_array())
                    .and_then(|names| names.first())
                    .and_then(|n| n.as_str())
                    .and_then(|name| std::env::var(name).ok())
            });

        let Some(models) = pv.get("models").and_then(|v| v.as_object()) else { continue };
        for (model_key, mv) in models {
            let upstream_model = mv
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or(model_key)
                .to_string();
            let context_window = mv
                .get("limit")
                .and_then(|l| l.get("context"))
                .and_then(|v| v.as_u64());
            let max_output = mv
                .get("limit")
                .and_then(|l| l.get("output"))
                .and_then(|v| v.as_u64())
                .unwrap_or(8192)
                .min(65_536);
            let vision = mv
                .get("modalities")
                .and_then(|m| m.get("input"))
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().any(|x| x.as_str() == Some("image")))
                .unwrap_or(false);
            let reasoning = mv.get("reasoning").and_then(|v| v.as_bool()).unwrap_or(false);
            let tool_call = mv.get("tool_call").and_then(|v| v.as_bool()).unwrap_or(true);
            let cost = mv.get("cost").map(|c| Cost {
                    input: c.get("input").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32,
                    output: c.get("output").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32,
                });
            records.push(ModelRecord {
                id: format!("{provider_id}/{model_key}"),
                provider: provider_id.clone(),
                protocol: Protocol::OpenAI,
                base_url: base_url.clone().unwrap_or_default(),
                api_key_env: None,
                api_key: api_key.clone(),
                keys: api_key.clone().map(|k| vec![KeySlot { label: "inline".into(), value: k }]).unwrap_or_default(),
                upstream_model,
                context_window,
                max_output,
                cost,
                tiers: Tiers {
                    reasoning: if reasoning { 0.75 } else { 0.5 },
                    coding: 0.6,
                    vision: if vision { 0.85 } else { 0.0 },
                    agentic: if tool_call { 0.7 } else { 0.4 },
                },
                tiers_explicit: false,
                plan: false,
                currency: mr_core::types::infer_currency(provider_id).to_string(),
                speed_tier: 0.6,
                weight: None,
                source: Source::Discovered,
                source_note: Some("opencode".into()),
            });
        }
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      // comment with "quotes" and urls https://x.y
      "$schema": "https://opencode.ai/config.json",
      "provider": {
        "zhipuai-coding-plan": {
          "options": { "apiKey": "key-a" },
          "models": {
            /* block comment */
            "glm-5.3-flash": { "name": "Flash", "limit": { "context": 1024000, "output": 131072 },
              "modalities": { "input": ["text", "image"], "output": ["text"] } },
          },
        },
        "deepseek": {
          "options": { "baseURL": "https://api.deepseek.com", "apiKey": "key-b" },
          "models": {
            "deepseek-flash": { "name": "flash", "limit": { "context": 1024000, "output": 1024000 } },
          },
        },
      },
      "model": "zhipuai-coding-plan/glm-5.3-flash",
    }"#;

    #[test]
    fn parses_jsonc_and_respects_user_config() {
        let models = discover_from_text(FIXTURE).unwrap();
        assert_eq!(models.len(), 2);
        let flash = models.iter().find(|m| m.id == "zhipuai-coding-plan/glm-5.3-flash").unwrap();
        assert_eq!(flash.base_url, provider_default_base_url("zhipuai-coding-plan").unwrap());
        assert_eq!(flash.api_key.as_deref(), Some("key-a"));
        assert_eq!(flash.context_window, Some(1_024_000));
        assert_eq!(flash.tiers.vision, 0.85);
        let ds = models.iter().find(|m| m.id == "deepseek/deepseek-flash").unwrap();
        assert_eq!(ds.base_url, "https://api.deepseek.com", "explicit baseURL from user config wins");
        assert_eq!(ds.api_key.as_deref(), Some("key-b"));
    }

    #[test]
    fn trailing_commas_and_block_comments_survive() {
        let out = strip_jsonc("{\n \"a\": [1, 2,], /* x */ \"b\": \"c, d\", \n}");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["a"].as_array().unwrap().len(), 2);
        assert_eq!(v["b"], "c, d");
    }

    #[test]
    fn comment_with_url_not_treated_as_comment() {
        let out = strip_jsonc(r#"{"url": "https://x.y//not-comment", "n": 1}"#);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["url"], "https://x.y//not-comment");
    }
}
