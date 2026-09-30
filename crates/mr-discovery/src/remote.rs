//! Remote model-list discovery: GET {base_url}/models on each known
//! provider (OpenAI-compatible), returning the union with user-configured
//! entries. Connection facts always come from the catalog records.

use mr_core::types::*;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Group records by (base_url, first key) so one fetch serves all aliases.
pub struct ProviderGroup {
    pub base_url: String,
    pub key: Option<String>,
    pub provider_hint: String,
}

pub fn provider_groups(records: &[ModelRecord]) -> Vec<ProviderGroup> {
    let mut groups: HashMap<(String, String), ProviderGroup> = HashMap::new();
    for r in records {
        if r.base_url.is_empty() || r.protocol != Protocol::OpenAI {
            continue;
        }
        let key = r.key_values().first().cloned().unwrap_or_default();
        let e = groups
            .entry((r.base_url.clone(), key.clone()))
            .or_insert_with(|| ProviderGroup {
                base_url: r.base_url.clone(),
                key: Some(key.clone()),
                provider_hint: r.provider.clone(),
            });
        if e.provider_hint == "custom" && r.provider != "custom" {
            e.provider_hint = r.provider.clone();
        }
    }
    groups.into_values().collect()
}

fn provider_name_of(base_url: &str) -> String {
    // https://api.deepseek.com -> deepseek ; https://ark.../api/coding/v3 -> ark
    base_url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|host| host.split('.').next())
        .unwrap_or("remote")
        .to_string()
}

/// GET {base_url}/models -> Vec<ModelRecord> (source = Remote).
/// Records inherit nothing: caller merges by provider sample ids for keys.
pub async fn fetch_provider_models(
    client: &reqwest::Client,
    base_url: &str,
    key: Option<&str>,
) -> anyhow::Result<Vec<ModelRecord>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = client.get(&url).timeout(Duration::from_secs(6));
    if let Some(k) = key.filter(|k| !k.is_empty()) {
        req = req.bearer_auth(k);
    }
    let resp = req.send().await?.error_for_status()?;
    let v: Value = resp.json().await?;
    let Some(data) = v.get("data").and_then(|d| d.as_array()) else {
        return Ok(Vec::new());
    };
    let provider = provider_name_of(base_url);
    let mut out = Vec::new();
    for m in data {
        let Some(id) = m.get("id").and_then(|i| i.as_str()) else { continue };
        out.push(ModelRecord {
            id: format!("{provider}/{id}"),
            provider: provider.clone(),
            protocol: Protocol::OpenAI,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: key.map(|k| k.to_string()),
            upstream_model: id.to_string(),
            context_window: m.get("context_length").and_then(|v| v.as_u64()),
            max_output: 8192,
            cost: None,
            tiers: Tiers::default(),
            speed_tier: 0.6,
            source: Source::Remote,
            source_note: Some("remote /models".into()),
            ..Default::default()
        });
    }
    Ok(out)
}

pub fn cache_path(dir: &str) -> PathBuf {
    let base = if dir.starts_with("~/") {
        std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(&dir[2..]))
            .unwrap_or(PathBuf::from(dir))
    } else {
        PathBuf::from(dir)
    };
    base.join("remote-models.json")
}

const CACHE_TTL_MS: u64 = 60 * 60 * 1000;

/// Blocking discovery with a 1h cache file. Called from build_state
/// (outside any runtime). Falls back to stale cache on fetch failure.
pub fn discover_remote_blocking(
    records: &[ModelRecord],
    cache_dir: &str,
) -> Vec<ModelRecord> {
    let path = cache_path(cache_dir);
    let fresh = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .map(|e| (e.as_millis() as u64) < CACHE_TTL_MS)
        .unwrap_or(false);
    if fresh
        && let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(v) = serde_json::from_str::<Value>(&text) {
                return parse_cached(&v);
            }
    let client = match reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .user_agent("modelroute/0.1")
        .build()
    {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut all = Vec::new();
    let mut errors = 0;
    let existing: std::collections::HashSet<(String, String)> = records
        .iter()
        .map(|r| (r.base_url.clone(), r.upstream_model.clone()))
        .collect();
    let mut group_meta: HashMap<String, String> = HashMap::new();
    for g in provider_groups(records) {
        group_meta.insert(g.base_url.clone(), g.provider_hint.clone());
        let base_url = g.base_url.clone();
        match fetch_provider_models_blocking(&client, &base_url, g.key.as_deref()) {
            Ok(mut models) => {
                let before = models.len();
                let hint = group_meta.get(&base_url).cloned().unwrap_or_default();
                // drop models the catalog already knows at this base_url
                models.retain(|m| {
                    !existing.contains(&(m.base_url.clone(), m.upstream_model.clone()))
                });
                for m in &mut models {
                    if !hint.is_empty() {
                        // id 用 hint 前缀（与已发现条目同族），保持目录整洁
                        m.id = format!("{hint}/{}", m.upstream_model);
                        m.provider = hint.clone();
                    }
                }
                let dropped = before - models.len();
                tracing::info!(provider = %base_url, fetched = before, deduped = dropped, kept = models.len(), "remote fetch done");
                all.append(&mut models);
            }
            Err(e) => {
                errors += 1;
                tracing::debug!(provider = %base_url, error = %e, "remote /models unavailable");
            }
        }
    }
    if errors as usize == provider_groups(records).len() && all.is_empty() {
        // every provider unreachable: keep stale cache usable next time
        return Vec::new();
    }
    let snap = serde_json::json!({ "models": all });
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, serde_json::to_string(&snap).unwrap_or_default());
    tracing::info!(count = all.len(), "remote model discovery complete");
    all
}

/// Parse a cached snapshot {"models": [ModelRecord...]}.
pub fn parse_cached(v: &Value) -> Vec<ModelRecord> {
    serde_json::from_value::<Vec<ModelRecord>>(v.get("models").cloned().unwrap_or(Value::Null))
        .unwrap_or_default()
}

fn fetch_provider_models_blocking(
    client: &reqwest::blocking::Client,
    base_url: &str,
    key: Option<&str>,
) -> anyhow::Result<Vec<ModelRecord>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = client.get(&url);
    if let Some(k) = key.filter(|k| !k.is_empty()) {
        req = req.bearer_auth(k);
    }
    let resp = req.send()?.error_for_status()?;
    let v: Value = resp.json()?;
    let Some(data) = v.get("data").and_then(|d| d.as_array()) else {
        return Ok(Vec::new());
    };
    let provider = provider_name_of(base_url);
    let mut out = Vec::new();
    for m in data {
        let Some(id) = m.get("id").and_then(|i| i.as_str()) else { continue };
        out.push(ModelRecord {
            id: format!("{provider}/{id}"),
            provider: provider.clone(),
            protocol: Protocol::OpenAI,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: key.map(|k| k.to_string()),
            upstream_model: id.to_string(),
            context_window: m.get("context_length").and_then(|v| v.as_u64()),
            max_output: 8192,
            tiers: Tiers::default(),
            speed_tier: 0.6,
            source: Source::Remote,
            source_note: Some("remote /models".into()),
            ..Default::default()
        });
    }
    Ok(out)
}

#[allow(dead_code)]
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_groups_dedupes_by_base_and_key() {
        let recs = vec![
            ModelRecord { id: "a/x".into(), base_url: "http://p1/v1".into(),
                          keys: vec![KeySlot { label: "k".into(), value: "k1".into() }],
                          ..Default::default() },
            ModelRecord { id: "a/y".into(), base_url: "http://p1/v1".into(),
                          keys: vec![KeySlot { label: "k".into(), value: "k1".into() }],
                          ..Default::default() },
        ];
        let groups = provider_groups(&recs);
        assert_eq!(groups.len(), 1, "same base+key = one fetch");
        assert_eq!(groups.len(), 1);
    }
}
