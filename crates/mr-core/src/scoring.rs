use crate::types::*;

pub use crate::types::CandidateScore;

pub fn tier_for_domain(domain: Domain, tiers: &Tiers, difficulty: f32) -> f32 {
    let d = (difficulty / 3.0).clamp(0.0, 1.0);
    match domain {
        Domain::Code => tiers.coding * (1.0 - d * 0.4) + tiers.reasoning * (d * 0.4),
        Domain::MathLogic => tiers.reasoning,
        Domain::Data => tiers.coding * 0.7 + tiers.reasoning * 0.3,
        Domain::AgentOps => tiers.agentic,
        Domain::Writing | Domain::Lookup | Domain::Chitchat | Domain::Other => {
            1.0 - d * 0.2 + 0.2 * d * tiers.reasoning
        }
    }
}

pub fn quality(m: &ModelRecord, j: &JudgmentSet, difficulty_eff: f32) -> f32 {
    let tier = tier_for_domain(j.domain, &m.tiers, difficulty_eff);
    let requirement = (difficulty_eff / 3.0).clamp(0.0, 1.0);
    let steep = 1.0 + 2.0 * requirement;
    let tier_st = tier.powf(steep);
    let q = 1.0 - requirement * (1.0 - tier_st);
    if j.high_stakes > 0.6 {
        q * (0.6 + 0.4 * tier.max(m.tiers.reasoning))
    } else {
        q
    }
}

/// Quality floor below which a model is considered not good enough for the
/// task difficulty; difficulty 0 → 1.0 (everyone qualifies, cost decides),
/// difficulty 3 → 0.55 (only strong models qualify).
pub fn quality_floor(difficulty_eff: f32) -> f32 {
    1.0 - 0.45 * (difficulty_eff / 3.0).clamp(0.0, 1.0)
}

/// 难度-费用联动（用户裁决：简单任务用便宜模型，复杂任务可以用贵的模型）。
/// 难度 0：成本权重 ×2、质量权重 ×0.75（省钱优先）；
/// 难度 3：成本权重 ×0.4、质量权重满额（成本让位，质量主导）。
/// 返回归一化后的五因子 [质量, 速度, 成本, 可靠, 余量]。
pub fn difficulty_weights(w: &PolicyWeights, difficulty_eff: f32) -> [f32; 5] {
    let [wq, ws, wc, wr, wh] = w.normalized();
    let d = (difficulty_eff / 3.0).clamp(0.0, 1.0);
    let cost_boost = 2.0 - 1.6 * d;
    let quality_scale = 0.75 + 0.25 * d;
    let mut v = [wq * quality_scale, ws, wc * cost_boost, wr, wh];
    let sum: f32 = v.iter().sum();
    for x in v.iter_mut() {
        *x /= sum.max(1e-6);
    }
    v
}

pub fn blended_price(m: &ModelRecord, est_input: u64, est_output: u64) -> f32 {
    match m.cost {
        Some(c) => (est_input as f32 / 1e6) * c.input + (est_output as f32 / 1e6) * c.output,
        None => 0.0,
    }
}

pub fn score_all(
    candidates: &[&ModelRecord],
    j: &JudgmentSet,
    difficulty_eff: f32,
    est_input: u64,
    est_output: u64,
    weights: &PolicyWeights,
    telemetry: &TelemetrySnapshot,
) -> Vec<CandidateScore> {
    let [wq, ws, wc, wr, wh] = weights.normalized();
    // Known-cost models only anchor the price floor; unknown-cost models
    // are cost-neutral (0.5) — never rewarded as if they were cheapest.
    let min_price = candidates
        .iter()
        .filter_map(|m| m.cost.map(|_| blended_price(m, est_input, est_output)))
        .fold(f32::MAX, f32::min);
    // 订阅模型配额消耗强度（官方系数折算）：方案内 flash≈1.0，旗舰 3-9 倍
    let min_plan_hint = candidates
        .iter()
        .filter(|m| m.plan)
        .filter_map(|m| crate::plans::model_price_hint(&m.base_url, &m.id))
        .fold(f32::MAX, f32::min);

    candidates
        .iter()
        .map(|m| {
            let q = quality(m, j, difficulty_eff);
            let prior_speed = m.speed_tier.clamp(0.0, 1.0);
            let s = match telemetry.get(&m.id).and_then(|t| t.speed_obs) {
                Some(obs) => 0.7 * prior_speed + 0.3 * obs.clamp(0.0, 1.0),
                None => prior_speed,
            };
            let c = if m.plan {
                // 订阅套餐：配额内边际成本≈0，但配额消耗速率按官方系数折算
                // （同方案内旗舰烧配额是轻量模型的数倍——简单任务便宜模型胜出）
                match crate::plans::model_price_hint(&m.base_url, &m.id) {
                    Some(hint) if min_plan_hint.is_finite() && min_plan_hint > 0.0 => {
                        (min_plan_hint / hint.max(1e-6)).clamp(0.05, 1.0)
                    }
                    _ => 1.0,
                }
            } else {
                match m.cost {
                    Some(_) => {
                        let price = blended_price(m, est_input, est_output).max(1e-6);
                        (min_price / price).clamp(0.0, 1.0)
                    }
                    None => 0.5,
                }
            };
            let r = telemetry
                .get(&m.id)
                .and_then(|t| t.reliability)
                .unwrap_or(0.7)
                .clamp(0.0, 1.0);
            let h = m
                .context_window
                .map(|w| ((w as f64 - est_input as f64 - est_output as f64) / w as f64).clamp(0.0, 1.0) as f32)
                .unwrap_or(0.0);
            let mut score = wq * q + ws * s + wc * c + wr * r + wh * h;
            // user weight x learned bias: soft multiplier, never decisive
            let user_w = m.weight.unwrap_or(1.0).clamp(0.2, 3.0);
            let bias = telemetry
                .get(&m.id)
                .and_then(|t| t.learned_bias)
                .unwrap_or(1.0)
                .clamp(0.7, 1.3);
            score *= (user_w * bias).sqrt().clamp(0.4, 1.8);
            CandidateScore { model_id: m.id.clone(), score, q, s, c, r, h, uw: user_w, bias }
        })
        .collect()
}

pub fn context_fits(m: &ModelRecord, est_input: u64, max_output: u64) -> Result<(), &'static str> {
    // None window: the engine's proven-bound check (telemetry max_accepted)
    // is the single gate — reaching here means the model is proven, so no
    // declared-window check applies.
    let Some(window) = m.context_window else { return Ok(()) };
    let need = (est_input as f64 * 1.1) as u64 + max_output;
    if need <= window {
        Ok(())
    } else {
        Err("context window too small")
    }
}

#[cfg(test)]
mod difficulty_cost_tests {
    use super::*;

    fn plan_model(id: &str, tier: f32) -> ModelRecord {
        ModelRecord {
            id: id.into(),
            provider: "zhipu".into(),
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4".into(),
            api_key: Some("k".into()),
            upstream_model: id.into(),
            context_window: Some(1_000_000),
            max_output: 4096,
            cost: None,
            plan: true,
            tiers: Tiers { reasoning: tier, coding: tier, vision: 0.0, agentic: tier },
            speed_tier: 0.5,
            source: Source::User,
            ..Default::default()
        }
    }

    fn judge(domain: Domain, difficulty: f32) -> JudgmentSet {
        JudgmentSet {
            domain,
            domain_confidence: 1.0,
            difficulty,
            difficulty_confidence: 1.0,
            needs_vision: 0.0,
            is_trivial: 0.0,
            tool_heavy: 0.0,
            high_stakes: 0.0,
            session_relevance: 0.0,
            session_depth: 0.0,
            judge_source: "test",
        }
    }

    fn rank(eff: [f32; 5], difficulty: f32) -> Vec<String> {
        let flash = plan_model("zhipuai-coding-plan/glm-5.3-flash", 0.70);
        let big = plan_model("zhipuai-coding-plan/glm-5.3", 0.95);
        let cands = vec![&flash, &big];
        let j = judge(Domain::Code, difficulty);
        let w = PolicyWeights {
            quality: eff[0], speed: eff[1], cost: eff[2], stability: eff[3], headroom: eff[4],
        };
        let tel = TelemetrySnapshot::new();
        let mut out = score_all(&cands, &j, difficulty, 10_000, 4_096, &w, &tel);
        out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        out.iter().map(|s| s.model_id.clone()).collect()
    }

    #[test]
    fn dbg_scores() {
        let flash = plan_model("zhipuai-coding-plan/glm-5.3-flash", 0.70);
        let big = plan_model("zhipuai-coding-plan/glm-5.3", 0.95);
        let cands = vec![&flash, &big];
        let j = judge(Domain::Code, 2.5);
        let eff = difficulty_weights(&PolicyWeights::balanced(), 2.5);
        println!("eff={:?}", eff);
        let w = PolicyWeights { quality: eff[0], speed: eff[1], cost: eff[2], stability: eff[3], headroom: eff[4] };
        let tel = TelemetrySnapshot::new();
        let out = score_all(&cands, &j, 2.5, 10_000, 4_096, &w, &tel);
        for s in &out { println!("{} score={:.4} q={:.4} s={:.4} c={:.4} r={:.4} h={:.4}", s.model_id, s.score, s.q, s.s, s.c, s.r, s.h); }
    }

    /// 用户裁决：简单任务便宜模型胜（成本权重 ×2，且 flash 配额消耗仅 1/3）
    #[test]
    fn easy_task_prefers_credit_light_model() {
        let eff = difficulty_weights(&PolicyWeights::balanced(), 0.3);
        assert_eq!(rank(eff, 0.3)[0], "zhipuai-coding-plan/glm-5.3-flash");
    }

    /// 复杂任务质量主导：成本让位（×0.4），旗舰胜出
    #[test]
    fn hard_task_prefers_capable_model_despite_cost() {
        let eff = difficulty_weights(&PolicyWeights::balanced(), 2.5);
        assert_eq!(rank(eff, 2.5)[0], "zhipuai-coding-plan/glm-5.3");
    }

    /// 难度联动本身：成本权重单调递减
    #[test]
    fn cost_weight_monotonically_decreases_with_difficulty() {
        let lo = difficulty_weights(&PolicyWeights::balanced(), 0.0)[2];
        let mid = difficulty_weights(&PolicyWeights::balanced(), 1.5)[2];
        let hi = difficulty_weights(&PolicyWeights::balanced(), 3.0)[2];
        assert!(lo > mid && mid > hi, "lo={lo} mid={mid} hi={hi}");
    }
}
