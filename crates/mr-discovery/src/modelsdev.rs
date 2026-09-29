//! models.dev reference metadata (decision record #22: reference ONLY).
//! Fills *missing* metadata (context window, cost) for catalog entries;
//! never overrides user or agent-config values. Cached with 24h TTL in the
//! data dir; fetching happens in the background after startup.

use mr_core::types::{Cost, ModelRecord};
use serde_json::Value;
use std::path::PathBuf;

pub const API_URL: &str = "https://models.dev/api.json";
pub const TTL_MS: u64 = 24 * 60 * 60 * 1000;

fn cache_path(dir: &str) -> PathBuf {
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
    let base = if dir.starts_with("~/") {
        home.join(dir.trim_start_matches("~/"))
    } else {
        PathBuf::from(dir)
    };
    base.join("modelsdev.json")
}

pub fn load_fresh(dir: &str) -> Option<Value> {
    let path = cache_path(dir);
    let meta = std::fs::metadata(&path).ok()?;
    let age = meta.modified().ok()?.elapsed().ok()?;
    if age.as_millis() as u64 > TTL_MS {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn spawn_refresh(dir: String) {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .user_agent("modelroute/0.1")
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        match client.get(API_URL).send().await {
            Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                Ok(bytes) => {
                    let path = cache_path(&dir);
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if std::fs::write(&path, &bytes).is_ok() {
                        tracing::info!(bytes = bytes.len(), "models.dev reference cached");
                    }
                }
                Err(e) => tracing::debug!(error = %e, "models.dev fetch body failed"),
            },
            Ok(resp) => tracing::debug!(status = %resp.status(), "models.dev fetch rejected"),
            Err(e) => tracing::debug!(error = %e, "models.dev fetch failed"),
        }
    });
}

/// Enrich missing metadata only. Never touches id/base_url/keys/protocol.
pub fn enrich(records: &mut [ModelRecord], api: &Value) {
    for r in records.iter_mut() {
        let model_entry = find_model(api, &r.provider, &r.upstream_model).or_else(|| find_model_global(api, &r.upstream_model));
        let Some(m) = model_entry else { continue };
        let mut touched = false;
        if r.context_window.is_none()
            && let Some(w) = m.get("limit").and_then(|l| l.get("context")).and_then(|v| v.as_u64()) {
                r.context_window = Some(w);
                touched = true;
            }
        if r.cost.is_none()
            && let Some(cost) = m.get("cost").and_then(|c| {
                let input = c.get("input").and_then(|v| v.as_f64())? as f32;
                let output = c.get("output").and_then(|v| v.as_f64())? as f32;
                Some(Cost { input, output })
            }) {
                r.cost = Some(cost);
                touched = true;
            }
        if touched {
            r.source_note = Some(match r.source_note.take() {
                Some(n) => format!("{n} +models.dev"),
                None => "models.dev".into(),
            });
        }
    }
}

fn find_model<'a>(api: &'a Value, provider: &str, model_id: &str) -> Option<&'a Value> {
    let p = api.get(provider)?.get("models")?.get(model_id)?;
    Some(p)
}

fn find_model_global<'a>(api: &'a Value, model_id: &str) -> Option<&'a Value> {
    api.as_object()?.values().find_map(|p| p.get("models")?.get(model_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fills_missing_only() {
        let api = json!({
            "zhipu": {
                "models": {
                    "glm-5.3": {"limit": {"context": 200_000},
                                "cost": {"input": 1.0, "output": 4.0}}
                }
            }
        });
        let mut records = vec![
            ModelRecord { id: "zhipu/glm-5.3".into(), provider: "zhipu".into(),
                          upstream_model: "glm-5.3".into(), ..Default::default() },
            ModelRecord { id: "zhipu/glm-x".into(), provider: "zhipu".into(),
                          upstream_model: "glm-x".into(),
                          context_window: Some(99_000), ..Default::default() },
        ];
        enrich(&mut records, &api);
        assert_eq!(records[0].context_window, Some(200_000));
        assert_eq!(records[0].cost.unwrap().input, 1.0);
        // never overrides present values
        assert_eq!(records[1].context_window, Some(99_000));
        assert!(records[1].cost.is_none());
    }
}
