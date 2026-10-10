//! Benchmark capability feeds (decision record #23): external leaderboards
//! refresh model capability priors, sitting between builtin defaults and
//! live flywheel telemetry. Rules:
//!  - sources are CONFIG-DRIVEN (urls/auth change constantly in the wild)
//!  - only fills/refreshes `tiers` for models whose aliases match
//!  - never touches user weights/limits/connection facts (#22)
//!  - fetch failure keeps last-good scores

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub use turbine_core::config::BenchSourceCfg;

/// Curated cold-start snapshot, embedded from the repo (no network needed).
pub const SEED_JSON: &str = include_str!("../../../config/benchmarks-seed.json");

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BenchScore {
    pub alias: String,
    pub coding: Option<f32>,
    pub reasoning: Option<f32>,
    pub agentic: Option<f32>,
    pub raw: Value,
    pub fetched_at_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceState {
    pub fetched_at_ms: u64,
    pub scores: Vec<BenchScore>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BenchSnapshot {
    pub sources: HashMap<String, SourceState>,
}

/// Normalize a model name for cross-board matching: lowercase, strip
/// provider prefixes, date suffixes and version noise.
pub fn normalize_alias(name: &str) -> String {
    let mut s = name.to_lowercase();
    if let Some(pos) = s.find('/') {
        s = s[pos + 1..].to_string();
    }
    for pat in [
        "-20241120", "-20241119", "-20241217", "-20250101", "-20250514",
        "-2024-11-20", "-2024-12-17", "-2025-01-01", "-2025-05-14",
        "-20240229", "-20240306", "-20240409", "-20240620", "-20240827",
        "-20240919", "-20241022",
    ] {
        s = s.replace(pat, "");
    }
    for pat in ["-preview", "-latest", "-instruct", "-it", "-turbo"] {
        s = s.replace(pat, "");
    }
    s.trim_matches('-').to_string()
}

/// Min-max normalize into [0.3, 1.0]: order preserved, but the weakest
/// model on a board stays viable (0.0 would nuke its tier outright).
fn min_max(values: &mut [(usize, f32)]) {
    let min = values.iter().map(|(_, v)| *v).fold(f32::MAX, f32::min);
    let max = values.iter().map(|(_, v)| *v).fold(f32::MIN, f32::max);
    if max <= min {
        return;
    }
    for (_, v) in values.iter_mut() {
        let n = (*v - min) / (max - min);
        *v = 0.3 + 0.7 * n.clamp(0.0, 1.0);
    }
}

/// Parse one source payload into normalized scores.
pub fn parse_source(cfg: &BenchSourceCfg, payload: &Value) -> anyhow::Result<Vec<BenchScore>> {
    let now = now_ms();
    match cfg.format.as_str() {
        // HF datasets-server rows: {"rows":[{"row":{...}}]}
        "lmarena_rows" => {
            let rows = payload
                .get("rows")
                .and_then(|r| r.as_array())
                .ok_or_else(|| anyhow::anyhow!("missing rows array"))?;
            let mut scored: Vec<(usize, f32)> = Vec::new();
            let mut out: Vec<BenchScore> = Vec::new();
            for (idx, r) in rows.iter().enumerate() {
                let row = r.get("row").unwrap_or(r);
                let Some(alias) = row.get("model_name").and_then(|m| m.as_str()) else { continue };
                let mut b = BenchScore {
                    alias: normalize_alias(alias),
                    raw: row.clone(),
                    fetched_at_ms: now,
                    ..Default::default()
                };
                if let Some(c) = row.get("coding").and_then(|v| v.as_f64()) {
                    b.coding = Some(((c as f32 - 1000.0) / 400.0).clamp(0.0, 1.0));
                }
                if let Some(m) = row.get("math").and_then(|v| v.as_f64()) {
                    b.reasoning = Some(((m as f32 - 1000.0) / 400.0).clamp(0.0, 1.0));
                }
                if let Some(e) = row
                    .get("arena_score")
                    .or_else(|| row.get("overall"))
                    .and_then(|v| v.as_f64())
                {
                    scored.push((idx, e as f32));
                }
                out.push(b);
            }
            min_max(&mut scored);
            for (idx, norm) in scored {
                if let Some(b) = out.get_mut(idx) {
                    b.reasoning = Some(norm);
                }
            }
            Ok(out)
        }
        // SWE-bench verified: array (top-level / "leaderboard" / "data")
        "swebench" => {
            let arr = payload
                .as_array()
                .or_else(|| payload.get("leaderboard").and_then(|l| l.as_array()))
                .or_else(|| payload.get("data").and_then(|d| d.as_array()))
                .ok_or_else(|| anyhow::anyhow!("missing leaderboard array"))?;
            let mut out = Vec::new();
            for row in arr {
                let Some(alias) = row
                    .get("name")
                    .or_else(|| row.get("model"))
                    .or_else(|| row.get("model_name"))
                    .and_then(|m| m.as_str())
                else {
                    continue;
                };
                let Some(score) = row
                    .get("resolved")
                    .or_else(|| row.get("percent_resolved"))
                    .or_else(|| row.get("score"))
                    .and_then(|v| v.as_f64())
                else {
                    continue;
                };
                let pct = if score <= 1.0 { score as f32 * 100.0 } else { score as f32 };
                out.push(BenchScore {
                    alias: normalize_alias(alias),
                    coding: Some((pct / 100.0).clamp(0.0, 1.0)),
                    agentic: Some((pct / 100.0).clamp(0.0, 1.0)),
                    raw: row.clone(),
                    fetched_at_ms: now,
                    ..Default::default()
                });
            }
            Ok(out)
        }
        // LiveBench: {"model": {"reasoning": 0-100, "coding": ...}}
        "livebench" => {
            let Some(map) = payload.as_object() else { return Ok(Vec::new()) };
            let mut out = Vec::new();
            for (name, cats) in map {
                if !cats.is_object() {
                    continue;
                }
                out.push(BenchScore {
                    alias: normalize_alias(name),
                    coding: cats.get("coding").and_then(|v| v.as_f64()).map(|v| (v / 100.0).clamp(0.0, 1.0) as f32),
                    reasoning: cats.get("reasoning").and_then(|v| v.as_f64()).map(|v| (v / 100.0).clamp(0.0, 1.0) as f32),
                    raw: cats.clone(),
                    fetched_at_ms: now,
                    ..Default::default()
                });
            }
            Ok(out)
        }
        // curated seed: {"scores": {"alias": {"coding":0-1,...}}}
        "seed" => {
            let Some(map) = payload.get("scores").and_then(|s| s.as_object()) else {
                return Ok(Vec::new());
            };
            let mut out = Vec::new();
            for (name, dims) in map {
                out.push(BenchScore {
                    alias: normalize_alias(name),
                    coding: dims.get("coding").and_then(|v| v.as_f64()).map(|v| v as f32),
                    reasoning: dims.get("reasoning").and_then(|v| v.as_f64()).map(|v| v as f32),
                    agentic: dims.get("agentic").and_then(|v| v.as_f64()).map(|v| v as f32),
                    raw: dims.clone(),
                    fetched_at_ms: now,
                });
            }
            Ok(out)
        }
        // Artificial Analysis (requires free AA_API_KEY; v2 models payload):
        // array of {model_name, intelligence_index, coding_index, math_index} (0-100)
        "artificialanalysis" => {
            let arr = payload
                .as_array()
                .or_else(|| payload.get("data").and_then(|d| d.as_array()))
                .ok_or_else(|| anyhow::anyhow!("missing data array"))?;
            let mut out = Vec::new();
            for row in arr {
                let Some(alias) = row
                    .get("model_name")
                    .or_else(|| row.get("name"))
                    .and_then(|m| m.as_str())
                else {
                    continue;
                };
                let norm = |k: &str| row.get(k).and_then(|v| v.as_f64()).map(|v| (v / 100.0).clamp(0.0, 1.0) as f32);
                out.push(BenchScore {
                    alias: normalize_alias(alias),
                    coding: norm("coding_index"),
                    reasoning: norm("intelligence_index").or_else(|| norm("math_index")),
                    agentic: norm("agentic_index"),
                    raw: row.clone(),
                    fetched_at_ms: now,
                });
            }
            Ok(out)
        }
        // generic: array + configured alias/score keys, min-max normalized
        "generic" => {
            let alias_key = cfg.alias_key.as_deref().unwrap_or("model");
            let score_key = cfg.score_key.as_deref().unwrap_or("score");
            let arr = payload
                .as_array()
                .or_else(|| payload.get("data").and_then(|d| d.as_array()))
                .ok_or_else(|| anyhow::anyhow!("missing data array"))?;
            let mut scored: Vec<(usize, f32)> = Vec::new();
            let mut out: Vec<BenchScore> = Vec::new();
            for (idx, row) in arr.iter().enumerate() {
                let Some(alias) = row.get(alias_key).and_then(|m| m.as_str()) else { continue };
                let Some(score) = row.get(score_key).and_then(|v| v.as_f64()) else { continue };
                out.push(BenchScore {
                    alias: normalize_alias(alias),
                    raw: row.clone(),
                    fetched_at_ms: now,
                    ..Default::default()
                });
                scored.push((idx, score as f32));
            }
            min_max(&mut scored);
            for (idx, norm) in scored {
                if let Some(b) = out.get_mut(idx) {
                    b.coding = Some(norm);
                }
            }
            Ok(out)
        }
        other => Err(anyhow::anyhow!("unknown benchmark format '{other}'")),
    }
}

/// Merge per-source scores into per-alias blended tiers (mean of sources).
pub fn blend_tiers(sources: &[(String, Vec<BenchScore>)]) -> HashMap<String, BenchScore> {
    let mut acc: HashMap<String, [f32; 6]> = HashMap::new();
    for (_, scores) in sources {
        for s in scores {
            let e = acc.entry(s.alias.clone()).or_default();
            if let Some(c) = s.coding {
                e[0] += c;
                e[1] += 1.0;
            }
            if let Some(r) = s.reasoning {
                e[2] += r;
                e[3] += 1.0;
            }
            if let Some(a) = s.agentic {
                e[4] += a;
                e[5] += 1.0;
            }
        }
    }
    acc.into_iter()
        .map(|(alias, e)| {
            (
                alias.clone(),
                BenchScore {
                    alias,
                    coding: (e[1] > 0.0).then(|| e[0] / e[1]),
                    reasoning: (e[3] > 0.0).then(|| e[2] / e[3]),
                    agentic: (e[5] > 0.0).then(|| e[4] / e[5]),
                    ..Default::default()
                },
            )
        })
        .collect()
}

/// Apply blended benchmark scores to catalog records (decision record #24):
///   L1 exact  -> tiers applied as-is
///   L2 tokens -> 50/50 blend with current tiers
///   umbrella  -> family mean at 50%; explicit user tiers cap confidence at 0.3
/// `tiers_explicit` marks catalog entries whose tiers were user-declared.
/// Returns (model_id, tiers, confidence) for the engine overlay.
pub fn tier_updates_for_catalog(
    blended: &HashMap<String, BenchScore>,
    records: &[turbine_core::types::ModelRecord],
) -> Vec<(String, turbine_core::types::Tiers, f32)> {
    use crate::alias;
    use turbine_core::types::Tiers;
    let mut out = Vec::new();
    for r in records {
        let mut conf: Option<f32> = None;
        let mut bench_owned: Option<BenchScore> = None;

        // dynamic aliases: "glm-latest" = newest family version (user-confirmed);
        // "auto" = provider routes internally, unknown => family mean
        if alias::is_dynamic(&r.upstream_model) || alias::is_dynamic(&r.id) {
            let probe = if alias::is_dynamic(&r.upstream_model) { &r.upstream_model } else { &r.id };
            let family = alias::family_key(probe);
            if let Some(family) = family {
                let members: Vec<&BenchScore> = blended
                    .iter()
                    .filter(|(a, _)| a.starts_with(&family) && a.len() > family.len())
                    .map(|(_, b)| b)
                    .collect();
                if !members.is_empty() {
                    let avg = |get: fn(&BenchScore) -> Option<f32>| -> Option<f32> {
                        let vs: Vec<f32> = members.iter().filter_map(|b| get(b)).collect();
                        (!vs.is_empty()).then(|| vs.iter().sum::<f32>() / vs.len() as f32)
                    };
                    let latest_mode = probe.to_lowercase().contains("latest");
                    if latest_mode {
                        // newest family version: highest numeric version among aliases
                        let newest = members
                            .iter()
                            .max_by(|a, b| {
                                let va = alias::version_num(&a.alias).unwrap_or(0.0);
                                let vb = alias::version_num(&b.alias).unwrap_or(0.0);
                                va.partial_cmp(&vb).unwrap_or(std::cmp::Ordering::Equal)
                            })
                            .copied();
                        if let Some(b) = newest {
                            conf = Some(alias::CONF_LATEST);
                            bench_owned = Some(BenchScore { alias: family.clone(), ..b.clone() });
                        }
                    } else {
                        conf = Some(alias::CONF_UMBRELLA);
                        bench_owned = Some(BenchScore {
                            alias: family.clone(),
                            coding: avg(|b| b.coding),
                            reasoning: avg(|b| b.reasoning),
                            agentic: avg(|b| b.agentic),
                            ..Default::default()
                        });
                    }
                }
            }
        } else {
            for (a, b) in blended {
                if let Some(c) = alias::match_confidence(a, &r.upstream_model)
                    .or_else(|| alias::match_confidence(a, &r.id))
                    && conf.map(|cur| c > cur).unwrap_or(true) {
                        conf = Some(c);
                        bench_owned = Some(b.clone());
                    }
            }
        }

        let Some(bench) = bench_owned else { continue };
        let Some(conf) = conf else { continue };
        let conf = if r.tiers_explicit {
            (conf * alias::CONF_EXPLICIT_TIERS_RATIO).min(conf)
        } else {
            conf
        };
        let t = Tiers {
            reasoning: bench.reasoning.unwrap_or(r.tiers.reasoning),
            coding: bench.coding.unwrap_or(r.tiers.coding),
            vision: r.tiers.vision, // vision benchmarks not in scope yet
            agentic: bench.agentic.unwrap_or(r.tiers.agentic),
        };
        out.push((r.id.clone(), t, conf));
    }
    out
}

/// Load the embedded curated seed as a baseline source.
pub fn load_seed() -> Vec<BenchScore> {
    parse_source(
        &BenchSourceCfg { format: "seed".into(), ..Default::default() },
        &serde_json::from_str(SEED_JSON).unwrap_or(serde_json::Value::Null),
    ).unwrap_or_default()
}

/// Fetch one source (per-source headers, 15s timeout).
pub async fn fetch_source(
    client: &reqwest::Client,
    cfg: &BenchSourceCfg,
) -> anyhow::Result<Vec<BenchScore>> {
    let mut req = client.get(&cfg.url).timeout(std::time::Duration::from_secs(15));
    if let Some(hs) = &cfg.headers {
        for (k, v) in hs {
            req = req.header(k.as_str(), v.as_str());
        }
    }
    let payload: Value = req.send().await?.error_for_status()?.json().await?;
    parse_source(cfg, &payload)
}

/// Apply blended scores to catalog records -> engine overlay updates.
pub fn apply_to_engine(
    engine: &turbine_core::engine::Engine,
    sources: &[(String, Vec<BenchScore>)],
) -> usize {
    let blended = blend_tiers(sources);
    let records: Vec<turbine_core::types::ModelRecord> =
        engine.catalog_snapshot();
    let updates = tier_updates_for_catalog(&blended, &records);
    let n = updates.len();
    let map: HashMap<String, (turbine_core::types::Tiers, f32)> =
        updates.into_iter().map(|(id, t, c)| (id, (t, c))).collect();
    engine.apply_tier_updates(map);
    n
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn alias_normalizer_strips_dates_and_prefixes() {
        assert_eq!(normalize_alias("openai/gpt-4o-2024-11-20"), "gpt-4o");
        assert_eq!(normalize_alias("anthropic/claude-3-5-sonnet-20241022"), "claude-3-5-sonnet");
        assert_eq!(normalize_alias("GLM-5.3-Preview"), "glm-5.3");
        assert_eq!(normalize_alias("deepseek/deepseek-chat"), "deepseek-chat");
    }

    #[test]
    fn lmarena_rows_parse() {
        let cfg = BenchSourceCfg { format: "lmarena_rows".into(), ..Default::default() };
        let payload = json!({
            "rows": [
                {"row": {"model_name": "openai/gpt-4o-2024-11-20", "arena_score": 1372,
                         "coding": 1300, "math": 1280}},
                {"row": {"model_name": "claude-3-5-sonnet-20241022", "arena_score": 1271,
                         "coding": 1250, "math": 1200}}
            ]
        });
        let scores = parse_source(&cfg, &payload).unwrap();
        let gpt = scores.iter().find(|s| s.alias == "gpt-4o").unwrap();
        let claude = scores.iter().find(|s| s.alias == "claude-3-5-sonnet").unwrap();
        assert!(gpt.reasoning.unwrap() > claude.reasoning.unwrap(), "higher elo => higher norm");
    }

    #[test]
    fn swebench_parse_both_shapes() {
        let cfg = BenchSourceCfg { format: "swebench".into(), ..Default::default() };
        let scores = parse_source(&cfg, &json!([
            {"name": "Claude 3.5 Sonnet", "resolved": 49.0},
            {"name": "GPT-4o", "resolved": 33.2}
        ])).unwrap();
        let c = scores.iter().find(|s| s.alias.contains("claude")).unwrap();
        assert!((c.coding.unwrap() - 0.49).abs() < 1e-5);
        assert!(c.agentic.unwrap() > 0.3);
    }

    #[test]
    fn livebench_and_generic_parse() {
        let lb = BenchSourceCfg { format: "livebench".into(), ..Default::default() };
        let scores = parse_source(&lb, &json!({
            "deepseek-v4": {"reasoning": 78.5, "coding": 82.0}
        })).unwrap();
        assert_eq!(scores[0].alias, "deepseek-v4");
        assert!((scores[0].coding.unwrap() - 0.82).abs() < 1e-5);

        let generic = BenchSourceCfg {
            format: "generic".into(),
            alias_key: Some("name".into()),
            score_key: Some("swe_score".into()),
            max_score: Some(100.0),
            ..Default::default()
        };
        let scores = parse_source(&generic, &json!([
            {"name": "glm-5.3", "swe_score": 61.0},
            {"name": "kimi-k3", "swe_score": 55.0}
        ])).unwrap();
        let glm = scores.iter().find(|s| s.alias == "glm-5.3").unwrap();
        let kimi = scores.iter().find(|s| s.alias == "kimi-k3").unwrap();
        assert!(glm.coding.unwrap() > kimi.coding.unwrap(), "order preserved");
        assert!(kimi.coding.unwrap() >= 0.3, "floor band 0.3");
        assert!(glm.coding.unwrap() <= 1.0);
    }

    #[test]
    fn blend_across_sources() {
        let merged = blend_tiers(&[
            ("swe".into(), vec![BenchScore { alias: "glm-5.3".into(), coding: Some(0.7), agentic: Some(0.7), ..Default::default() }]),
            ("lb".into(), vec![BenchScore { alias: "glm-5.3".into(), coding: Some(0.9), reasoning: Some(0.6), ..Default::default() }]),
        ]);
        let g = merged.get("glm-5.3").unwrap();
        assert!((g.coding.unwrap() - 0.8).abs() < 1e-5);
        assert_eq!(g.reasoning, Some(0.6));
        assert_eq!(g.agentic, Some(0.7));
    }
}

#[cfg(test)]
mod umbrella_tests {
    use super::*;
    
    use turbine_core::types::{ModelRecord, Tiers};
    use std::collections::HashMap;

    fn blended_glm_family() -> HashMap<String, BenchScore> {
        HashMap::from([
            ("glm-5.3".into(), BenchScore { alias: "glm-5.3".into(), coding: Some(0.9), reasoning: Some(0.85), agentic: Some(0.8), ..Default::default() }),
            ("glm-5.2".into(), BenchScore { alias: "glm-5.2".into(), coding: Some(0.7), reasoning: Some(0.65), agentic: Some(0.6), ..Default::default() }),
        ])
    }

    fn catalog_with(upstream: &str, explicit: bool) -> Vec<ModelRecord> {
        vec![ModelRecord {
            id: format!("zhipu/{upstream}"),
            upstream_model: upstream.into(),
            tiers: Tiers { reasoning: 0.5, coding: 0.5, vision: 0.0, agentic: 0.5 },
            tiers_explicit: explicit,
            ..Default::default()
        }]
    }

    #[test]
    fn latest_alias_picks_newest_family_member() {
        let blended = blended_glm_family();
        let records = catalog_with("glm-latest", false);
        let updates = tier_updates_for_catalog(&blended, &records);
        let (_, tiers, conf) = &updates[0];
        // glm-latest == newest (5.3), not the mean of 5.3/5.2
        assert!((tiers.coding - 0.9).abs() < 1e-5, "tiers: {tiers:?}");
        assert!((conf - 0.7).abs() < 1e-5, "conf: {conf}");
    }

    #[test]
    fn auto_alias_uses_family_mean() {
        let blended = blended_glm_family();
        let records = catalog_with("auto-provider/glm-auto", false);
        let updates = tier_updates_for_catalog(&blended, &records);
        let (_, tiers, conf) = &updates[0];
        assert!((tiers.coding - 0.8).abs() < 1e-5, "mean(0.9,0.7)=0.8; tiers: {tiers:?}");
        assert!((conf - 0.5).abs() < 1e-5);
    }

    #[test]
    fn flash_and_pro_get_their_own_scores() {
        let blended = HashMap::from([
            ("deepseek-v4-flash".into(), BenchScore { alias: "deepseek-v4-flash".into(), coding: Some(0.5), ..Default::default() }),
            ("deepseek-v4-pro".into(), BenchScore { alias: "deepseek-v4-pro".into(), coding: Some(0.85), ..Default::default() }),
        ]);
        let records = catalog_with("deepseek-v4-pro", false);
        let updates = tier_updates_for_catalog(&blended, &records);
        let (_, tiers, _) = &updates[0];
        assert!((tiers.coding - 0.85).abs() < 1e-5, "pro must not inherit flash");
    }

    #[test]
    fn explicit_user_tiers_capped_blend() {
        let blended = blended_glm_family();
        let records = catalog_with("glm-5.3", true); // user declared tiers
        let updates = tier_updates_for_catalog(&blended, &records);
        let (_, _, conf) = &updates[0];
        assert!((conf - 0.3).abs() < 1e-5, "explicit tiers cap: {conf}");
    }
}

#[cfg(test)]
mod seed_tests {
    use super::*;

    #[test]
    fn seed_loads_and_matches_flash() {
        let seed = load_seed();
        assert!(seed.iter().any(|s| s.alias == "glm-5.3-flash"), "seed has glm-5.3-flash");
        let blended = blend_tiers(&[("curated-seed".into(), seed)]);
        let records = vec![turbine_core::types::ModelRecord {
            id: "opencode-go/glm-5.3-flash".into(),
            upstream_model: "glm-5.3-flash".into(),
            ..Default::default()
        }];
        let updates = tier_updates_for_catalog(&blended, &records);
        assert_eq!(updates.len(), 1, "seed must match upstream_model exactly");
    }
}
