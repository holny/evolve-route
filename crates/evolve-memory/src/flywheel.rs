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

/// ① 两阶段奖励归一化（BayesianRouter App.B）：
/// 先基线中心化（v − 滚动均值），再按 20/80 分位距缩放，钳制到 [0,1]。
/// 序列恒定（分位距 ≈ 0，无区分度）或样本 < 8 时返回中性 0.5。
fn normalize_signal(hist: &[f32], v: f32) -> f32 {
    if hist.len() < 8 {
        return 0.5;
    }
    let mean = hist.iter().sum::<f32>() / hist.len() as f32;
    let mut sorted = hist.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let q = |p: usize| sorted[(sorted.len() - 1) * p / 100];
    let iqr = q(80) - q(20);
    if !iqr.is_finite() || iqr < 1e-6 {
        return 0.5;
    }
    ((v - mean) / iqr).clamp(0.0, 1.0)
}

/// 单条 outcome 的融合 reward ∈ [0,1]：
/// ok_weight·窗口成功率 + lat_weight·(1−延迟信号)（延迟 ln 压缩后归一化，快 = 好）。
/// review#4 修复：ok 是二值信号，模型内 IQR 归一化在健康区间（失败率 <20%）
/// 恒为零 → 信号恒 0.5，成功率 85% 与 100% 的模型不可区分。改为直接使用
/// 窗口原始值（即逐条贡献 = 窗口成功率），跨模型区分由 ② 的跨模型
/// median 中心化承担——"学优势"语义不变，健康工况不再失效。
/// 延迟是连续信号，IQR 归一化保留。
/// 近窗样本 < 8 → None（统计不稳，调用方跳过该模型）。
/// pub：backtest 回放复用同一实现——策略公式的单一事实来源。
pub fn outcome_reward(
    recent: &[evolve_core::types::ReqSample],
    params: &crate::strategy::StrategyParams,
) -> Option<f32> {
    if recent.len() < 8 {
        return None;
    }
    let ok_hist: Vec<f32> = recent.iter().map(|r| if r.ok { 1.0 } else { 0.0 }).collect();
    let lat_hist: Vec<f32> = recent.iter().map(|r| (r.total_ms.max(1) as f32).ln()).collect();
    let rewards: Vec<f32> = recent
        .iter()
        .enumerate()
        .map(|(i, s)| {
            // 成功率信号：原始 0/1（逐条贡献即窗口 ok 率）；延迟信号保留 IQR 归一化
            let s_lat = normalize_signal(&lat_hist, lat_hist[i]);
            params.ok_weight * ok_hist[i] + params.lat_weight() * (1.0 - s_lat)
        })
        .collect();
    Some(rewards.iter().sum::<f32>() / rewards.len() as f32)
}

#[derive(Clone)]
pub struct Flywheel {
    inner: std::sync::Arc<Mutex<FlywheelInner>>,
    path: Option<PathBuf>,
    params: crate::strategy::StrategyParams,
}

const MIN_CALIBRATION_SAMPLES: u64 = 5;
const MIN_RELIABILITY_SAMPLES: u64 = 5;

impl Flywheel {
    pub fn open(dir: &str) -> Self {
        Self::open_with(dir, crate::strategy::StrategyParams::default())
    }

    /// ⑦ 策略参数注入：learned_bias 公式参数来自配置（[strategy] 段），
    /// 回滚 = 配置回到默认
    pub fn open_with(dir: &str, params: crate::strategy::StrategyParams) -> Self {
        let expanded = crate::events::shellexpand_home_pub(dir);
        let path = expanded.join("snapshot.json");
        let inner: FlywheelInner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        if !inner.models.is_empty() {
            tracing::info!(models = inner.models.len(), "flywheel snapshot restored");
        }
        let params = params.sanitized();
        tracing::info!(version = params.version, ok_weight = params.ok_weight, bias_gain = params.bias_gain, "flywheel strategy loaded");
        Self { inner: std::sync::Arc::new(Mutex::new(inner)), path: Some(path), params }
    }

    pub fn disabled() -> Self {
        Self {
            inner: std::sync::Arc::new(Mutex::new(FlywheelInner::default())),
            path: None,
            params: crate::strategy::StrategyParams::default(),
        }
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
        // review#6 修复：质量级联弃用的响应（退化/空）不再记成功——
        // 否则坏模型继续在 recent 环与 Thompson 后验里攒 ok 信用
        let quality_bad = event
            .get("quality")
            .map(|q| {
                q.get("degenerate").and_then(|v| v.as_bool()).unwrap_or(false)
                    || q.get("empty_response").and_then(|v| v.as_bool()).unwrap_or(false)
            })
            .unwrap_or(false);
        if (200..300).contains(&status) && !quality_bad {
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
                ok: (200..300).contains(&status)
                    && !q.and_then(|qq| qq.get("degenerate")).and_then(|v| v.as_bool()).unwrap_or(false)
                    && !q.and_then(|qq| qq.get("empty_response")).and_then(|v| v.as_bool()).unwrap_or(false),
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
        }
            // learned bias（BayesianRouter App.B 两阶段归一化 + BaRP Eq.6 基线相对更新）：
            // ① 多信号 outcome（成功率 0.7 + 延迟 0.3）逐条"减滚动均值 + 20/80 分位距钳制"到 [0,1]
            // ② 基线相对更新：模型 reward 中心 − 跨模型 median（学"优势"，不是绝对值——
            //    工作负载整体变难时所有模型 reward 同降，median 中心化后优势不变，不产生漂移）
            // ③ 分位钳制 [0.7, 1.3]——飞轮只能微调，不能推翻 Jev 判定
            let mut rewards: Vec<(String, f32)> = Vec::new();
            for (id, s) in &inner.models {
                if let Some(r) = outcome_reward(&s.recent, &self.params) {
                    rewards.push((id.clone(), r));
                }
            }
            if rewards.len() >= 3 {
                let mut rs: Vec<f32> = rewards.iter().map(|(_, r)| *r).collect();
                rs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let median = rs[rs.len() / 2];
                for (id, r) in rewards {
                    if let Some(t) = out.get_mut(&id) {
                        t.learned_bias =
                            Some((1.0 + (r - median) * self.params.bias_gain).clamp(0.7, 1.3));
                    }
                }
            }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_signal, outcome_reward};
    use evolve_core::types::ReqSample;

    fn sample(ok: bool, total_ms: u64) -> ReqSample {
        ReqSample {
            ts: 0,
            ttft_ms: total_ms / 2,
            total_ms,
            in_tok: 0,
            cached_tok: 0,
            out_tok: 0,
            ok,
            tools_total: 0,
            tools_ok: 0,
        }
    }

    #[test]
    fn normalize_constant_series_is_neutral() {
        let hist = vec![5.0; 10];
        assert_eq!(normalize_signal(&hist, 5.0), 0.5);
    }

    #[test]
    fn normalize_short_hist_is_neutral() {
        assert_eq!(normalize_signal(&[1.0, 2.0, 3.0], 1.0), 0.5);
    }

    #[test]
    fn normalize_scores_relative_to_rolling_distribution() {
        let hist: Vec<f32> = (0..20).map(|i| i as f32).collect();
        // q20=3, q80=15, mean=9.5：高值样本显著 >0.5，低值样本显著 <0.5
        assert!(normalize_signal(&hist, 18.0) > 0.6);
        assert!(normalize_signal(&hist, 1.0) < 0.4);
    }

    #[test]
    fn outcome_reward_ranks_reliable_fast_model_higher() {
        let good: Vec<ReqSample> = (0..12).map(|i| sample(i % 6 != 5, 800)).collect();
        let bad: Vec<ReqSample> = (0..12).map(|i| sample(i % 3 == 0, 9_000)).collect();
        let g = outcome_reward(&good, &crate::strategy::StrategyParams::default()).unwrap();
        let b = outcome_reward(&bad, &crate::strategy::StrategyParams::default()).unwrap();
        assert!(g > b, "reliable+fast ({g}) should outrank flaky+slow ({b})");
    }

    #[test]
    fn outcome_reward_needs_min_samples() {
        assert!(outcome_reward(&[sample(true, 100); 4], &crate::strategy::StrategyParams::default()).is_none());
    }

    #[test]
    fn outcome_reward_bounded_in_unit_interval() {
        let mixed: Vec<ReqSample> = (0..30)
            .map(|i| sample(i % 4 != 3, if i % 2 == 0 { 300 } else { 40_000 }))
            .collect();
        let r = outcome_reward(&mixed, &crate::strategy::StrategyParams::default()).unwrap();
        assert!((0.0..=1.0).contains(&r), "reward {r} out of [0,1]");
    }
}
