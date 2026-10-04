use crate::types::*;

#[derive(Debug, Clone)]
pub struct CandidateScore {
    pub model_id: String,
    pub score: f32,
    pub q: f32,
    pub s: f32,
    pub c: f32,
    pub r: f32,
    pub h: f32,
}

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
                // 订阅套餐：配额内边际成本≈0 —— 与目录最便宜按量模型同级
                1.0
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
            CandidateScore { model_id: m.id.clone(), score, q, s, c, r, h }
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
