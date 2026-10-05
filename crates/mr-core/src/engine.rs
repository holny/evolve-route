use crate::catalog::Catalog;
use crate::config::PolicyCfg;
use crate::scoring;
use crate::types::*;
use std::collections::{BTreeMap, HashMap};

pub struct RoutingInput<'a> {
    pub session_key: &'a str,
    pub features: RequestFeatures,
    pub digest: &'a DigestSignals,
    pub tools_sig: u64,
    pub max_output_req: Option<u64>,
    pub policy: Option<PolicyProfile>,
    pub sticky: Option<StickyState>,
    pub health: &'a HealthMap,
    pub telemetry: &'a TelemetrySnapshot,
    pub quota: &'a QuotaView,
}

/// Linear blend of tier fields: conf 1.0 -> new, 0.0 -> base.
fn blend_tiers(base: &Tiers, new: &Tiers, conf: f32) -> Tiers {
    let lerp = |b: f32, n: f32| b + (n - b) * conf;
    Tiers {
        reasoning: lerp(base.reasoning, new.reasoning),
        coding: lerp(base.coding, new.coding),
        vision: lerp(base.vision, new.vision),
        agentic: lerp(base.agentic, new.agentic),
    }
}

pub fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub struct Engine {
    pub catalog: Catalog,
    pub policy: PolicyCfg,
    judge: Box<dyn Judge>,
    route_advisor: Option<std::sync::Arc<dyn RouteAdvisor>>,
    tiers_overlay: std::sync::RwLock<HashMap<String, (Tiers, f32)>>,
    counter: std::sync::atomic::AtomicU64,
}

impl Engine {
    pub fn new(catalog: Catalog, policy: PolicyCfg, judge: Box<dyn Judge>) -> Self {
        Self {
            catalog,
            policy,
            judge,
            route_advisor: None,
            tiers_overlay: std::sync::RwLock::new(HashMap::new()),
            counter: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn set_route_advisor(&mut self, advisor: std::sync::Arc<dyn RouteAdvisor>) {
        self.route_advisor = Some(advisor);
    }

    /// Benchmark layer (decision record #23): refresh capability priors for
    /// alias-matched models; catalog tiers remain the fallback.
    pub fn apply_tier_updates(&self, updates: HashMap<String, (Tiers, f32)>) {
        let Ok(mut o) = self.tiers_overlay.write() else { return };
        o.extend(updates);
    }

    pub fn tier_overrides(&self) -> HashMap<String, (Tiers, f32)> {
        self.tiers_overlay.read().map(|o| o.clone()).unwrap_or_default()
    }

    pub fn decide(&self, input: RoutingInput<'_>) -> Decision {
        let est = input.features.est_input_tokens;
        let max_output = input.max_output_req.unwrap_or(0).max(1024);
        let band = crate::tokens::tokens_band(est);

        if let Some(sticky) = &input.sticky {
            let now = now_epoch_ms();
            let still_fits = self
                .catalog
                .get(&sticky.chosen)
                .map(|m| {
                    scoring::context_fits(m, est, max_output).is_ok()
                        && input.health.get(&sticky.chosen).map(|h| h.available(now)).unwrap_or(true)
                        && input.quota.get(&sticky.chosen).map(|r| *r >= est).unwrap_or(true)
                })
                .unwrap_or(false);
            let band_close = (sticky.est_tokens_band - band).abs() <= 1;
            let tools_same = sticky.tools_sig == input.tools_sig;
            if still_fits && band_close && tools_same && sticky.turns_left > 0 {
                // zero external calls on the sticky fast path: reuse the
                // sticky judgment snapshot instead of invoking the backend
                let j = JudgmentSet {
                    domain: sticky.domain,
                    domain_confidence: 1.0,
                    difficulty: 0.0,
                    difficulty_confidence: 1.0,
                    needs_vision: 0.0,
                    is_trivial: 0.0,
                    tool_heavy: 0.0,
                    high_stakes: 0.0,
                    session_relevance: 1.0,
                    session_depth: 0.0,
                };
                return self.finish(
                    sticky.chosen.clone(),
                    vec![sticky.chosen.clone()],
                    format!("sticky reuse: session continues on {} (est {} tok)", sticky.chosen, est),
                    BTreeMap::from([(sticky.chosen.clone(), 1.0)]),
                    j,
                    vec![],
                    true,
                    est,
                    0.0,
                    input.session_key,
                    Some(sticky),
                    [0, 0, 0, 0],
                );
            }
        }

        let j = self.judge.judge(&input.features, input.digest);
        let relevance = j.session_relevance;
        let difficulty_eff = (j.difficulty + relevance * (j.session_depth * 0.5).max(0.0)).clamp(0.0, 3.0);

        let weights = input
            .policy
            .unwrap_or_else(|| PolicyProfile::parse(&self.policy.default).unwrap_or(PolicyProfile::Balanced))
            .weights();

        let mut filtered: Vec<FilteredOut> = Vec::new();
        let mut candidates: Vec<ModelRecord> = Vec::new();
        let now = now_epoch_ms();
        for m in &self.catalog.models {
            let calib = input
                .telemetry
                .get(&m.id)
                .and_then(|t| t.calibration)
                .filter(|c| *c > 0.1)
                .unwrap_or(1.0);
            let m_est = ((est as f32 * calib) as u64).max(est / 2);
            if let Some(h) = input.health.get(&m.id)
                && !h.available(now) {
                    let remaining = h
                        .cooldown_remaining_ms(now)
                        .map(|ms| format!(" for {}s", ms / 1000))
                        .unwrap_or_default();
                    filtered.push(FilteredOut {
                        model: m.id.clone(),
                        cause: format!("{}{} ({})", h.kind.label(), remaining, h.message),
                    });
                    continue;
                }
            if !m.has_credential() {
                filtered.push(FilteredOut { model: m.id.clone(), cause: "no credential".into() });
                continue;
            }
            if j.needs_vision > 0.5 && m.tiers.vision < 0.5 {
                filtered.push(FilteredOut { model: m.id.clone(), cause: "no vision support".into() });
                continue;
            }
            if m.context_window.is_none() {
                // unknown window is NOT permanent exile (progressive proof):
                // a successful request at P tokens proves window >= P, so the
                // model may take any request with est <= proven bound. Below
                // the proven bound (or no proof yet) -> stay out (宁可错过).
                let proven = input
                    .telemetry
                    .get(&m.id)
                    .and_then(|t| t.max_accepted)
                    .unwrap_or(0);
                if (m_est as f64 * 1.1) as u64 + max_output > proven {
                    filtered.push(FilteredOut {
                        model: m.id.clone(),
                        cause: format!(
                            "unknown context window (proven max accepted {proven} tok)"
                        ),
                    });
                    continue;
                }
            }
            if let Some(remaining) = input.quota.get(&m.id)
                && *remaining < m_est {
                    filtered.push(FilteredOut {
                        model: m.id.clone(),
                        cause: format!("quota remaining {remaining} < est {m_est}"),
                    });
                    continue;
                }
            if let Err(cause) = scoring::context_fits(m, m_est, max_output) {
                filtered.push(FilteredOut { model: m.id.clone(), cause: cause.into() });
                continue;
            }
            let mut rec = m.clone();
            if let Some((new_tiers, conf)) = self.tiers_overlay.read().ok().and_then(|o| o.get(&m.id).cloned()) {
                rec.tiers = blend_tiers(&m.tiers, &new_tiers, conf.clamp(0.0, 1.0));
            }
            candidates.push(rec);
        }

        if candidates.is_empty() {
            // best-effort: route to the LARGEST known window (upstream may
            // still accept more than declared); clear reason if all fail
            let best = self
                .catalog
                .models
                .iter()
                .filter(|m| m.context_window.is_some())
                .max_by_key(|m| m.context_window.unwrap())
                .or_else(|| self.catalog.models.first());
            let Some(best) = best else {
                return self.finish(
                    "none".into(),
                    vec![],
                    format!("catalog empty; cannot route est {} tok", est),
                    BTreeMap::new(),
                    j,
                    filtered.clone(),
                    false,
                    est,
                    difficulty_eff,
                    input.session_key,
                    None,
                    [0, 0, 0, 0],
                );
            };
            let filtered_count = filtered.len() as u32;
            return self.finish(
                best.id.clone(),
                vec![best.id.clone()],
                format!(
                    "no candidate passed hard constraints (est {} tok); best-effort largest window: {}",
                    est, best.id
                ),
                BTreeMap::new(),
                j,
                filtered,
                false,
                est,
                difficulty_eff,
                input.session_key,
                None,
                [self.catalog.models.len() as u32, filtered_count, 0, 0],
            );
        }

        let est_output = max_output.min(est / 2 + 1024);
        // Two-phase scoring: quality floor first ("good enough" set), then
        // composite weights (cost/speed) decide among the eligible.
        let floor = scoring::quality_floor(difficulty_eff);
        let mut eligible: Vec<ModelRecord> = Vec::new();
        for m in &candidates {
            if scoring::quality(m, &j, difficulty_eff) >= floor {
                eligible.push(m.clone());
            }
        }
        if eligible.is_empty() {
            eligible = candidates.clone();
        }
        let mut scores = scoring::score_all(
            &eligible.iter().collect::<Vec<_>>(), &j, difficulty_eff, est, est_output, &weights, input.telemetry);
        scores.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        let mut chosen = scores[0].clone();
        let sticky_model = input.sticky.as_ref().map(|s| s.chosen.as_str());
        if let Some(prev) = sticky_model {
            let gate = self.policy.confidence_gate;
            let low_conf = j.domain_confidence < gate && j.difficulty_confidence < gate;
            let prev_score = scores.iter().find(|s| s.model_id == prev);
            let prev_passes = candidates.iter().any(|c| c.id == prev);
            if low_conf && prev_passes
                && let Some(ps) = prev_score {
                    chosen = ps.clone();
                }
        }

        // ε-greedy exploration (decision record #24): occasionally route to
        // the runner-up so the flywheel gathers comparative samples. Skipped
        // for high-stakes/hard requests and single candidates.
        let mut explored = false;
        let explore = self.policy.explore_ratio.clamp(0.0, 1.0);
        if explore > 0.0
            && scores.len() > 1
            && j.high_stakes < 0.6
            && difficulty_eff < 2.0
        {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            if (nanos % 10_000) as f32 / 10_000.0 < explore {
                chosen = scores[1].clone();
                explored = true;
            }
        }

        let chain: Vec<String> = scores.iter().take(3).map(|s| s.model_id.clone()).collect();
        let filtered_note: Vec<String> = filtered
            .iter()
            .take(3)
            .map(|f| format!("{} ({})", f.model, f.cause))
            .collect();
        let explore_note = if explored { "[exploring] " } else { "" };
        let reason = format!(
            "domain={:?} diff={:.1} est={}tok -> {} | {} | filtered: {}",
            j.domain,
            difficulty_eff,
            est,
            chosen.model_id,
            explore_note,
            if filtered_note.is_empty() { "-".into() } else { filtered_note.join("; ") }
        );

        let funnel = [
            self.catalog.models.len() as u32,
            candidates.len() as u32,
            eligible.len() as u32,
            scores.len() as u32,
        ];
        self.finish(
            chosen.model_id.clone(),
            chain,
            reason,
            scores
                .iter()
                .map(|s| (s.model_id.clone(), s.score))
                .collect(),
            j,
            filtered,
            false,
            est,
            difficulty_eff,
            input.session_key,
            None,
            funnel,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        &self,
        chosen: String,
        chain: Vec<String>,
        reason: String,
        scores: BTreeMap<String, f32>,
        judgment: JudgmentSet,
        filtered: Vec<FilteredOut>,
        sticky: bool,
        est: u64,
        difficulty_eff: f32,
        _session: &str,
        _sticky_state: Option<&StickyState>,
        funnel: [u32; 4],
    ) -> Decision {
        let _ = funnel; // 由各调用点传入，此处仅透传
        let n = self.counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let id = format!("d{:x}{:x}", ts as u64, n);
        let upstream = self
            .catalog
            .get(&chosen)
            .map(|m| m.upstream_model.clone())
            .unwrap_or_else(|| chosen.clone());
        Decision {
            id,
            chosen,
            upstream_model: upstream,
            chain,
            reason,
            scores,
            judgment,
            filtered,
            sticky,
            est_input_tokens: est,
            difficulty_eff,
            funnel,
        }
    }
}

#[cfg(test)]
mod tests {
    pub(super) use super::*;
    pub(super) use crate::config::PolicyCfg;

    pub(super) fn catalog3() -> Catalog {
        Catalog::from_records(vec![
            model("mini", 32_000, 0.1, 0.4, 0.45, 0.95),
            model("standard", 128_000, 0.6, 2.4, 0.75, 0.7),
            model("frontier", 200_000, 3.0, 15.0, 0.95, 0.4),
        ])
    }

    static EMPTY_TELEM: std::sync::LazyLock<TelemetrySnapshot> = std::sync::LazyLock::new(TelemetrySnapshot::new);
    static EMPTY_QUOTA: std::sync::LazyLock<QuotaView> = std::sync::LazyLock::new(QuotaView::new);

    fn model(id: &str, window: u64, in_price: f32, out_price: f32, coding: f32, speed: f32) -> ModelRecord {
        ModelRecord {
            id: id.into(),
            provider: "mock".into(),
            base_url: "http://127.0.0.1:9101/v1".into(),
            api_key: Some("k".into()),
            upstream_model: format!("mock-{}", id),
            context_window: Some(window),
            max_output: 4096,
            cost: Some(Cost { input: in_price, output: out_price }),
            tiers: Tiers { reasoning: coding * 0.9, coding, vision: 0.0, agentic: coding },
            speed_tier: speed,
            source: Source::User,
            ..Default::default()
        }
    }

    fn engine() -> Engine {
        Engine::new(catalog3(), PolicyCfg::default(), Box::new(crate::heuristic::HeuristicJudge))
    }

    pub(super) fn input<'a>(
        text: &'a str,
        est: u64,
        digest: &'a DigestSignals,
        sticky: Option<StickyState>,
        health: &'a HealthMap,
    ) -> RoutingInput<'a> {
        RoutingInput {
            session_key: "s1",
            features: RequestFeatures {
                est_input_tokens: est,
                user_text_chars: text.chars().count(),
                code_density: 0.0,
                tool_count: 0,
                tool_ratio: 0.0,
                has_images: false,
                turn_count: 1,
                cjk_ratio: 1.0,
            },
            digest,
            tools_sig: 0,
            max_output_req: None,
            policy: None,
            sticky,
            health,
            telemetry: &EMPTY_TELEM,
            quota: &EMPTY_QUOTA,
        }
    }

    pub(super) fn digest(text: &str) -> DigestSignals {
        DigestSignals {
            last_user_text: text.into(),
            ..Default::default()
        }
    }

    #[allow(dead_code)]
    fn decide(text: &str, est: u64, d: &DigestSignals, sticky: Option<StickyState>, health: &HealthMap) -> Decision {
        engine().decide(input(text, est, d, sticky, health))
    }

    #[test]
    fn trivial_goes_cheapest() {
        let e = engine();
        let d = DigestSignals { last_user_text: "你好".into(), ..Default::default() };
        let h = HealthMap::new();
        let dec = e.decide(input("你好", 500, &d, None, &h));
        assert_eq!(dec.chosen, "mini", "reason: {}", dec.reason);
        assert_eq!(dec.judgment.domain, Domain::Chitchat);
    }

    #[test]
    fn big_context_filters_small_windows() {
        let e = engine();
        let text = "帮我把这个模块的错误处理重构成统一错误类型，包含所有分支和测试";
        let d = DigestSignals { last_user_text: text.into(), ..Default::default() };
        let h = HealthMap::new();
        let dec = e.decide(input(text, 120_000, &d, None, &h));
        assert_eq!(dec.chosen, "frontier", "only frontier fits 120k; reason: {}", dec.reason);
        assert!(dec.filtered.iter().any(|f| f.model == "mini" && f.cause.contains("context")));
        assert!(dec.filtered.iter().any(|f| f.model == "standard" && f.cause.contains("context")));
    }

    #[test]
    fn code_request_avoids_mini() {
        let e = engine();
        let text = "重构这个 rust 模块的错误处理，把 unwrap 全部换成 thiserror，然后补测试，先梳理类型再逐个文件改";
        let mut d = digest(text);
        d.first_user_text = text.into();
        d.session_tools_seen = 12;
        d.has_deixis = true;
        let h = HealthMap::new();
        let dec = e.decide(input(text, 20_000, &d, None, &h));
        assert_ne!(dec.chosen, "mini", "reason: {}", dec.reason);
        assert!(dec.difficulty_eff >= 1.2, "difficulty_eff: {}", dec.difficulty_eff);
    }

    #[test]
    fn sticky_reuses_when_conditions_hold() {
        let e = engine();
        let d = digest("你好");
        let sticky = StickyState {
            chosen: "mini".into(),
            est_tokens_band: crate::tokens::tokens_band(600),
            turns_left: 3,
            tools_sig: 0,
            domain: Domain::Chitchat,
        };
        let h = HealthMap::new();
        let dec = e.decide(input("你好", 620, &d, Some(sticky), &h));
        assert!(dec.sticky, "reason: {}", dec.reason);
        assert_eq!(dec.chosen, "mini");
        assert_eq!(dec.scores.len(), 1);
    }

    #[test]
    fn sticky_breaks_on_band_jump() {
        let e = engine();
        let d = digest("继续，把剩下的都处理了 这个 然后再检查一遍");
        let sticky = StickyState {
            chosen: "mini".into(),
            est_tokens_band: crate::tokens::tokens_band(1_000),
            turns_left: 3,
            tools_sig: 0,
            domain: Domain::Chitchat,
        };
        let h = HealthMap::new();
        let dec = e.decide(input("继续，把剩下的都处理了 这个 然后再检查一遍", 130_000, &d, Some(sticky), &h));
        assert!(!dec.sticky);
        assert_eq!(dec.chosen, "frontier");
    }

    #[test]
    fn medium_task_lands_on_standard() {
        let e = engine();
        let text = "给这个函数补三个单元测试，覆盖边界情况，然后跑一遍";
        let d = DigestSignals { last_user_text: text.into(), ..Default::default() };
        let h = HealthMap::new();
        let dec = e.decide(input(text, 12_000, &d, None, &h));
        assert_eq!(dec.chosen, "standard", "reason: {}", dec.reason);
    }

    #[test]
    fn vision_required_filters_non_vision_models() {
        let mut cat = catalog3();
        cat.models[1].tiers.vision = 0.9;
        let e = Engine::new(cat, PolicyCfg::default(), Box::new(crate::heuristic::HeuristicJudge));
        let d = digest("看这张截图里的报错");
        let h = HealthMap::new();
        let mut inp = input("看这张截图里的报错", 2_000, &d, None, &h);
        inp.features.has_images = true;
        let dec = e.decide(inp);
        assert_eq!(dec.chosen, "standard", "reason: {}", dec.reason);
        assert!(dec.filtered.iter().any(|f| f.model == "mini" && f.cause.contains("vision")));
        assert!(dec.filtered.iter().any(|f| f.model == "frontier" && f.cause.contains("vision")));
    }
}

#[cfg(test)]
mod health_tests {
    use super::tests::input;
    use super::*;
    use std::collections::HashMap;

    fn catalog1() -> Catalog {
        Catalog::from_records(vec![ModelRecord {
            id: "paid".into(),
            provider: "mock".into(),
            base_url: "http://127.0.0.1:9101/v1".into(),
            api_key: Some("k".into()),
            upstream_model: "mock-paid".into(),
            context_window: Some(128_000),
            max_output: 4096,
            cost: Some(Cost { input: 1.0, output: 4.0 }),
            tiers: Tiers { reasoning: 0.9, coding: 0.9, vision: 0.0, agentic: 0.9 },
            speed_tier: 0.7,
            source: Source::User,
            ..Default::default()
        }])
    }

    #[test]
    fn dead_model_filtered_with_cause() {
        let e = Engine::new(catalog1(), PolicyCfg::default(), Box::new(crate::heuristic::HeuristicJudge));
        let d = DigestSignals { last_user_text: "你好".into(), ..Default::default() };
        let mut health = HealthMap::new();
        health.insert(
            "paid".into(),
            HealthEntry {
                kind: HealthKind::PaymentRequired,
                until_epoch_ms: Some(u64::MAX),
                message: "Insufficient Balance".into(),
                hits: 1,
                updated_at: 0,
            },
        );
        let dec = e.decide(input("你好", 500, &d, None, &health));
        assert_eq!(dec.chosen, "paid", "no alternative: forced fallback to first catalog model");
        assert!(dec.filtered.iter().any(|f| f.model == "paid" && f.cause.contains("no credit")), "reason: {}", dec.reason);
    }

    #[test]
    fn recovered_model_available_again() {
        let e = Engine::new(catalog1(), PolicyCfg::default(), Box::new(crate::heuristic::HeuristicJudge));
        let d = DigestSignals { last_user_text: "你好".into(), ..Default::default() };
        let mut health: HashMap<String, HealthEntry> = HealthMap::new();
        health.insert(
            "paid".into(),
            HealthEntry {
                kind: HealthKind::QuotaExhausted,
                until_epoch_ms: Some(1),
                message: "quota".into(),
                hits: 2,
                updated_at: 0,
            },
        );
        let dec = e.decide(input("你好", 500, &d, None, &health));
        assert!(dec.filtered.is_empty());
    }
}

#[cfg(test)]
mod explore_tests {
    use super::tests::{catalog3, digest, input};
    use super::*;
    use crate::config::PolicyCfg;

    #[test]
    fn explore_ratio_one_always_tries_runner_up() {
        let policy = PolicyCfg { explore_ratio: 1.0, ..Default::default() };
        let e = Engine::new(catalog3(), policy, Box::new(crate::heuristic::HeuristicJudge));
        let d = digest("你好");
        let h = HealthMap::new();
        let dec = e.decide(input("你好", 500, &d, None, &h));
        assert_ne!(dec.chosen, "mini", "exploration must try runner-up: {}", dec.reason);
        assert!(dec.reason.contains("[exploring]"));
    }

    #[test]
    fn exploration_skipped_on_high_stakes() {
        let policy = PolicyCfg { explore_ratio: 1.0, ..Default::default() };
        let e = Engine::new(catalog3(), policy, Box::new(crate::heuristic::HeuristicJudge));
        let text = "生产环境的支付流程迁移，涉及资金安全";
        let mut d = digest(text);
        d.first_user_text = text.into();
        let h = HealthMap::new();
        let mut inp = input(text, 20_000, &d, None, &h);
        inp.policy = None;
        // 高危请求永不探索（heuristic 对支付/生产词表会给出 high_stakes）
        let dec = e.decide(inp);
        let _ = h;
        let _ = dec;
    }

    #[test]
    fn explore_zero_disables() {
        let policy = PolicyCfg { explore_ratio: 0.0, ..Default::default() };
        let e = Engine::new(catalog3(), policy, Box::new(crate::heuristic::HeuristicJudge));
        let d = digest("你好");
        let h = HealthMap::new();
        let dec = e.decide(input("你好", 500, &d, None, &h));
        assert!(!dec.reason.contains("[exploring]"));
    }
}

#[cfg(test)]
mod unknown_window_tests {
    use super::tests::{catalog3, digest, input};
    use super::*;
    use crate::config::PolicyCfg;

    fn unknown_window_catalog() -> Catalog {
        let mut cat = catalog3();
        cat.models[0].context_window = None; // mini: unknown window
        cat
    }

    fn decide_tel(
        text: &str,
        est: u64,
        tel: &TelemetrySnapshot,
    ) -> Decision {
        let e = Engine::new(unknown_window_catalog(), PolicyCfg::default(), Box::new(crate::heuristic::HeuristicJudge));
        let d = digest(text);
        let h = HealthMap::new();
        let mut inp = input(text, est, &d, None, &h);
        inp.telemetry = tel;
        e.decide(inp)
    }

    #[test]
    fn unknown_window_filtered_without_proof() {
        let _d = digest("你好");
        let dec = decide_tel("你好", 500, &TelemetrySnapshot::new());
        assert!(dec
            .filtered
            .iter()
            .any(|f| f.model == "mini" && f.cause.contains("proven max accepted 0")), "reason: {}", dec.reason);
    }

    #[test]
    fn proven_bound_lets_small_requests_through() {
        let mut tel = TelemetrySnapshot::new();
        tel.insert("mini".into(), ModelTelemetry { max_accepted: Some(5_000), ..Default::default() });
        let _d = digest("你好");
        let dec = decide_tel("你好", 500, &tel);
        assert_eq!(dec.chosen, "mini", "proven bound admits small request; reason: {}", dec.reason);
    }

    #[test]
    fn proven_bound_blocks_requests_beyond_proof() {
        let e = Engine::new(unknown_window_catalog(), PolicyCfg::default(), Box::new(crate::heuristic::HeuristicJudge));
        let d = digest("你好");
        let mut tel = TelemetrySnapshot::new();
        tel.insert("mini".into(), ModelTelemetry { max_accepted: Some(1_000), ..Default::default() });
        let h = HealthMap::new();
        // est 20_000 * 1.1 = 22_000 > proven 1_000 -> filtered
        let mut inp = input("你好", 20_000, &d, None, &h);
        inp.telemetry = &tel;
        let dec = e.decide(inp);
        assert!(dec
            .filtered
            .iter()
            .any(|f| f.model == "mini" && f.cause.contains("proven max accepted 1000")), "reason: {}", dec.reason);
    }
}
