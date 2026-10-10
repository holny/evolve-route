//! Flywheel: aggregates chat events into per-model statistics, learns token
//! estimation calibration, and persists a snapshot so learning survives
//! restarts. Feeds scoring (reliability/speed) and hard constraints
//! (calibration) back into the decision engine.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelStats {
    pub requests: u64,
    pub success: u64,
    pub failures: u64,
    pub ttft_ms_sum: u64,
    pub ttft_n: u64,
    pub total_ms_sum: u64,
    pub total_ms_n: u64,
    pub completion_tokens: u64,
    pub prompt_tokens: u64,
    pub prompt_n: u64,
    pub est_tokens: u64,
    pub est_n: u64,
    pub cached_tokens: u64,
    pub cache_write_tokens: u64,
    // L2 gateway-side response quality
    pub tc_total: u64,
    pub tc_valid_json: u64,
    pub tc_known_name: u64,
    pub tc_schema_ok: u64,
    pub truncations: u64,
    pub degenerate: u64,
    pub empty_responses: u64,
    // L3 session-loop semantic outcomes
    pub sem_matched: u64,
    pub sem_total: u64,
    // L4 plugin-reported explicit feedback (tool execution results)
    pub fb_ok: u64,
    pub fb_total: u64,
    // 窗口下界推断：该模型实际接受过的最大 prompt_tokens（成功请求）
    pub max_accepted_tokens: u64,
    // 近期请求样本环（模型动态窗口聚合 + 趋势图；cap 300 条/模型）
    pub recent: Vec<evolve_core::types::ReqSample>,
    // recency for the dashboard "最近耗时"
    pub last_seen_ms: u64,
    pub last_total_ms: u64,
    pub last_ttft_ms: u64,
    pub last_rate_tok_s: u64,
    pub cost_usd: f64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FlywheelInner {
    models: HashMap<String, ModelStats>,
    dirty: bool,
}

/// 分层窗口可靠性（三重约束）：样本时间 ≤ 回溯期限（不能太老）、
/// 条数 ≤ take、条数 ≥ min（不能太新——不足则弃用回落上层）
fn rel_window(
    recent: &[evolve_core::types::ReqSample],
    take: usize,
    min: usize,
    window_ms: u64,
    now_ms: u64,
) -> Option<f32> {
    let tail: Vec<&evolve_core::types::ReqSample> = recent
        .iter()
        .rev()
        .filter(|r| now_ms.saturating_sub(r.ts) <= window_ms)
        .take(take)
        .collect();
    if tail.len() < min {
        return None;
    }
    let ok = tail.iter().filter(|r| r.ok).count();
    Some(ok as f32 / tail.len() as f32)
}

#[derive(Clone)]
pub struct Flywheel {
    inner: std::sync::Arc<Mutex<FlywheelInner>>,
    path: Option<PathBuf>,
}

const MIN_CALIBRATION_SAMPLES: u64 = 5;
const MIN_RELIABILITY_SAMPLES: u64 = 5;

impl Flywheel {
    pub fn open(dir: &str) -> Self {
        let expanded = crate::events::shellexpand_home_pub(dir);
        let path = expanded.join("snapshot.json");
        let inner: FlywheelInner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        if !inner.models.is_empty() {
            tracing::info!(models = inner.models.len(), "flywheel snapshot restored");
        }
        Self { inner: std::sync::Arc::new(Mutex::new(inner)), path: Some(path) }
    }

    pub fn disabled() -> Self {
        Self { inner: std::sync::Arc::new(Mutex::new(FlywheelInner::default())), path: None }
    }

    /// Feed one chat event (the same JSON written to events.jsonl).
    pub fn observe(&self, event: &Value) {
        if event.get("kind").and_then(|k| k.as_str()) != Some("chat") {
            return;
        }
        let Some(chosen) = event.get("chosen").and_then(|c| c.as_str()) else { return };
        let Ok(mut inner) = self.inner.lock() else { return };
        let s = inner.models.entry(chosen.to_string()).or_default();
        s.requests += 1;
        let status = event.get("status").and_then(|v| v.as_u64()).unwrap_or(0);
        if (200..300).contains(&status) {
            s.success += 1;
        } else if status != 0 {
            s.failures += 1;
        }
        if let Some(t) = event.get("ttft_ms").and_then(|v| v.as_u64()) {
            s.ttft_ms_sum += t;
            s.ttft_n += 1;
        }
        if let Some(t) = event.get("total_ms").and_then(|v| v.as_u64()) {
            s.total_ms_sum += t;
            s.total_ms_n += 1;
        }
        if let Some(c) = event.get("completion_tokens").and_then(|v| v.as_u64()) {
            s.completion_tokens += c;
        }
        let prompt = event.get("prompt_tokens").and_then(|v| v.as_u64());
        if let Some(p) = prompt {
            s.prompt_tokens += p;
            s.prompt_n += 1;
        }
        if let Some(c) = event.get("cached_tokens").and_then(|v| v.as_u64()) {
            s.cached_tokens += c;
        }
        if let Some(c) = event.get("cache_write_tokens").and_then(|v| v.as_u64()) {
            s.cache_write_tokens += c;
        }
        // 窗口下界：成功请求的 prompt_tokens 证明窗口 ≥ 该值
        if (200..300).contains(&status)
            && let Some(p) = event.get("prompt_tokens").and_then(|v| v.as_u64())
            && p > s.max_accepted_tokens
        {
            s.max_accepted_tokens = p;
        }
        s.last_seen_ms = now_ms();
        if let Some(t) = event.get("total_ms").and_then(|v| v.as_u64()) {
            s.last_total_ms = t;
        }
        if let Some(t) = event.get("ttft_ms").and_then(|v| v.as_u64()) {
            s.last_ttft_ms = t;
        }
        if let Some(r) = event.get("est_cost_usd").and_then(|v| v.as_f64()) {
            s.cost_usd += r;
        }
        // 最近吐字速率：completion_tokens / 生成窗口（total - ttft）
        if let (Some(c), Some(tt), Some(t0)) = (
            event.get("completion_tokens").and_then(|v| v.as_u64()),
            event.get("total_ms").and_then(|v| v.as_u64()),
            event.get("ttft_ms").and_then(|v| v.as_u64()),
        ) {
            let gen_ms = tt.saturating_sub(t0);
            if gen_ms > 50 {
                s.last_rate_tok_s = (c * 1000 / gen_ms).min(9999);
            }
        }
        // 近期请求样本环（窗口聚合 + 趋势图；cap 300 条/模型）
        {
            let ts = event.get("ts").and_then(|v| v.as_u64()).unwrap_or_else(now_ms);
            let q = event.get("quality");
            let sample = evolve_core::types::ReqSample {
                ts,
                ttft_ms: event.get("ttft_ms").and_then(|v| v.as_u64()).unwrap_or(0),
                total_ms: event.get("total_ms").and_then(|v| v.as_u64()).unwrap_or(0),
                in_tok: event.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                cached_tok: event.get("cached_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                out_tok: event.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                ok: (200..300).contains(&status),
                tools_total: q.and_then(|qq| qq.get("tool_calls_total")).and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                tools_ok: q.and_then(|qq| qq.get("tool_calls_valid_json")).and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            };
            s.recent.push(sample);
            if s.recent.len() > 300 {
                s.recent.remove(0);
            }
        }
        let est = event.get("est_tokens").and_then(|v| v.as_u64());
        if let (Some(p), Some(e)) = (prompt, est)
            && p > 0 && e > 0 {
                s.est_tokens += e;
                s.est_n += 1;
            }
        // L2 response quality fields
        if let Some(q) = event.get("quality").and_then(|v| v.as_object()) {
            s.tc_total += q.get("tool_calls_total").and_then(|v| v.as_u64()).unwrap_or(0);
            s.tc_valid_json += q.get("tool_calls_valid_json").and_then(|v| v.as_u64()).unwrap_or(0);
            s.tc_known_name += q.get("tool_calls_known_name").and_then(|v| v.as_u64()).unwrap_or(0);
            s.tc_schema_ok += q.get("tool_calls_schema_ok").and_then(|v| v.as_u64()).unwrap_or(0);
            if q.get("truncated").and_then(|v| v.as_bool()).unwrap_or(false) {
                s.truncations += 1;
            }
            if q.get("degenerate").and_then(|v| v.as_bool()).unwrap_or(false) {
                s.degenerate += 1;
            }
            if q.get("empty_response").and_then(|v| v.as_bool()).unwrap_or(false) {
                s.empty_responses += 1;
            }
        }
        inner.dirty = true;
    }

    /// L3 semantic outcome: how many of the previous turn's tool calls came
    /// back executed (role=tool messages) in this session.
    pub fn observe_semantic(&self, model_id: &str, matched: u64, total: u64) {
        if total == 0 {
            return;
        }
        let Ok(mut inner) = self.inner.lock() else { return };
        let s = inner.models.entry(model_id.to_string()).or_default();
        s.sem_matched += matched;
        s.sem_total += total;
        inner.dirty = true;
    }

    /// L4 explicit plugin feedback (tool executed ok/failed in the agent).
    pub fn observe_feedback(&self, model_id: &str, ok: bool) {
        let Ok(mut inner) = self.inner.lock() else { return };
        let s = inner.models.entry(model_id.to_string()).or_default();
        s.fb_total += 1;
        if ok {
            s.fb_ok += 1;
        }
        inner.dirty = true;
    }

    /// Reliability blends transport success, tool-call syntax and (when
    /// available) session-loop semantic confirmation.
    pub fn reliability_of(s: &ModelStats) -> Option<f32> {
        if s.requests < MIN_RELIABILITY_SAMPLES {
            return None;
        }
        let base = s.success as f32 / s.requests as f32;
        let mut rel = base;
        if s.tc_total > 0 {
            let json_ok = s.tc_valid_json as f32 / s.tc_total as f32;
            let name_ok = s.tc_known_name as f32 / s.tc_total as f32;
            rel *= 0.5 + 0.25 * json_ok + 0.25 * name_ok;
        }
        if s.sem_total >= 3 {
            let sem = s.sem_matched as f32 / s.sem_total as f32;
            rel = 0.5 * rel + 0.5 * sem;
        }
        if s.fb_total >= 3 {
            let fb = s.fb_ok as f32 / s.fb_total as f32;
            rel = 0.6 * rel + 0.4 * fb;
        }
        Some(rel.clamp(0.0, 1.0))
    }

    pub fn stats(&self) -> HashMap<String, ModelStats> {
        self.inner.lock().map(|i| i.models.clone()).unwrap_or_default()
    }

    pub fn save(&self) {
        let Ok(mut inner) = self.inner.lock() else { return };
        if !inner.dirty {
            return;
        }
        inner.dirty = false;
        if let Some(path) = &self.path {
            let tmp = path.with_extension("json.tmp");
            if let Ok(text) = serde_json::to_string_pretty(&*inner)
                && std::fs::write(&tmp, text).is_ok() {
                    let _ = std::fs::rename(&tmp, path);
                }
        }
    }

    /// Flush pending aggregates on shutdown.
    pub fn flush(&self) {
        self.save();
    }

    /// Telemetry view consumed by the decision engine.
    pub fn telemetry_snapshot(&self) -> evolve_core::types::TelemetrySnapshot {
        use evolve_core::types::{ModelTelemetry, TelemetrySnapshot};
        let inner = self.inner.lock();
        let Ok(inner) = inner else { return TelemetrySnapshot::new() };
        let mut out = TelemetrySnapshot::new();
        let mut sampled: Vec<(String, f32)> = Vec::new();
        let now = now_ms();
        for (id, s) in &inner.models {
            let reliability = Self::reliability_of(s);
            let speed_obs = if s.total_ms_n >= MIN_RELIABILITY_SAMPLES {
                let avg_ms = (s.total_ms_sum as f32 / s.total_ms_n as f32).max(1.0);
                // Monotonic log falloff: 1s -> ~0.79, 5s -> ~0.55, 30s -> ~0.22, 100s -> ~0.0
                Some((1.0 - (avg_ms.ln() / 100_000.0f32.ln())).clamp(0.0, 1.0))
            } else {
                None
            };
            let max_accepted = (s.max_accepted_tokens > 0).then_some(s.max_accepted_tokens);
            let calibration = if s.est_n >= MIN_CALIBRATION_SAMPLES && s.prompt_n >= MIN_CALIBRATION_SAMPLES {
                let avg_est = s.est_tokens as f32 / s.est_n as f32;
                let avg_actual = s.prompt_tokens as f32 / s.prompt_n as f32;
                if avg_est > 0.0 {
                    Some((avg_actual / avg_est).clamp(0.5, 3.0))
                } else {
                    None
                }
            } else {
                None
            };
            // 期限分层：近10次≤30分钟（瞬时）、近30次≤6小时（短期波动）——
            // 太老无法感知波动，太新统计不稳（min 门槛兜底）
            let rel_30 = rel_window(&s.recent, 30, 5, 6 * 3600 * 1000, now);
            let rel_10 = rel_window(&s.recent, 10, 3, 30 * 60 * 1000, now);
            out.insert(
                id.clone(),
                ModelTelemetry {
                    reliability,
                    speed_obs,
                    calibration,
                    learned_bias: None,
                    max_accepted,
                    samples: Some(s.requests.min(u32::MAX as u64) as u32),
                    recent: Some(s.recent.clone()),
                    rel_30,
                    rel_10,
                },
            );
            sampled.push((id.clone(), s.success as f32 / s.requests.max(1) as f32));
        }
        // learned bias: realized success vs catalog median, EMA-able later
        if sampled.len() >= 3 {
            let mut rates: Vec<f32> = sampled.iter().map(|(_, r)| *r).collect();
            rates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let median = rates[rates.len() / 2].max(0.05);
            for (id, rate) in sampled {
                if let Some(t) = out.get_mut(&id)
                    && rate > 0.0 {
                        t.learned_bias = Some((rate / median).clamp(0.7, 1.3));
                    }
            }
        }
        out
    }
}
