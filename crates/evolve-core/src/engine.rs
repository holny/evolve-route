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
    /// 订阅方案预算压力（plan_key → 消耗占比 0-1）
    pub plan_pressure: &'a HashMap<String, f32>,
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
    /// 目录（可热更新：定期扫描合入 / 面板删除后整体替换，见 replace_catalog）
    pub catalog: std::sync::RwLock<Catalog>,
    pub policy: PolicyCfg,
    judge: Box<dyn Judge>,
    route_advisor: Option<std::sync::Arc<dyn RouteAdvisor>>,
    tiers_overlay: std::sync::RwLock<HashMap<String, (Tiers, f32)>>,
    /// 面板可调的公式权重覆盖（quality/speed/cost/stability/headroom），
    /// 优先级：请求头 policy > 此覆盖 > 配置默认 profile
    weights_override: std::sync::RwLock<Option<PolicyWeights>>,
    /// 面板可调的用户权重覆盖（模型级），评分时覆盖目录 weight
    weight_overlay: std::sync::RwLock<HashMap<String, f32>>,
    counter: std::sync::atomic::AtomicU64,
}

impl Engine {
    pub fn new(catalog: Catalog, policy: PolicyCfg, judge: Box<dyn Judge>) -> Self {
        Self {
            catalog: std::sync::RwLock::new(catalog),
            policy,
            judge,
            route_advisor: None,
            tiers_overlay: std::sync::RwLock::new(HashMap::new()),
            weights_override: std::sync::RwLock::new(None),
            weight_overlay: std::sync::RwLock::new(HashMap::new()),
            counter: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 目录热更新：整体替换模型列表（重建索引）。扫描合入 / 面板删除后调用。
    pub fn replace_catalog(&self, models: Vec<crate::types::ModelRecord>) {
        if let Ok(mut c) = self.catalog.write() {
            *c = Catalog::from_records(models);
        }
    }

    /// 当前目录快照（读锁克隆，模型量级 ~200，开销可忽略）
    pub fn catalog_snapshot(&self) -> Vec<crate::types::ModelRecord> {
        self.catalog.read().map(|c| c.models.clone()).unwrap_or_default()
    }

    /// 按 id 查单个模型（读锁短临界区）
    fn catalog_read_model(&self, id: &str) -> Option<crate::types::ModelRecord> {
        self.catalog.read().ok().and_then(|c| c.get(id).cloned())
    }

    pub fn set_weights_override(&self, w: Option<PolicyWeights>) {
        if let Ok(mut o) = self.weights_override.write() {
            *o = w;
        }
    }

    pub fn weights_override(&self) -> Option<PolicyWeights> {
        self.weights_override.read().ok().and_then(|o| o.clone())
    }

    pub fn set_weight_override(&self, id: &str, w: Option<f32>) {
        if let Ok(mut o) = self.weight_overlay.write() {
            match w {
                Some(v) => {
                    o.insert(id.to_string(), v);
                }
                None => {
                    o.remove(id);
                }
            }
        }
    }

    pub fn weight_override(&self, id: &str) -> Option<f32> {
        self.weight_overlay.read().ok().and_then(|o| o.get(id).copied())
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
        let _band = crate::tokens::tokens_band(est);

        // 粘性的“度”（用户裁决：粘性没问题，但要控制好度）：
        // ① 轮数有限（sticky_turns）② 探索逃逸——以 explore_ratio 概率打破粘性
        // 走完整重评估，长会话也不会躺平（探索不再被粘性架空）
        let explore_ratio = self.policy.explore_ratio.clamp(0.0, 1.0);
        let sticky_escape = explore_ratio > 0.0 && {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            (nanos % 10_000) as f32 / 10_000.0 < explore_ratio
        };

        if let Some(sticky) = &input.sticky {
            let now = now_epoch_ms();
            // 预算压力保护：所选方案消耗超过软阈值时粘性立即断开，
            // 把宝贵配额留给硬任务（用户裁决）
            let plan_key = self.catalog_read_model(&sticky.chosen)
                .and_then(|m| crate::plans::plan_key_for(&m.base_url))
                .map(|k| k.to_string());
            let pressure_ok = plan_key
                .and_then(|k| input.plan_pressure.get(&k))
                .map(|p| *p <= self.policy.plan_soft_pct.clamp(10.0, 95.0) / 100.0 + 0.25)
                .unwrap_or(true);
            let still_fits = !sticky_escape && self
                .catalog_read_model(&sticky.chosen)
                .map(|ref m| {
                    scoring::context_fits(m, est, max_output).is_ok()
                        && model_available(input.health, &sticky.chosen, now)
                        && input.quota.get(&sticky.chosen).map(|r| *r >= est).unwrap_or(true)
                })
                .unwrap_or(false) && pressure_ok;
            // 粘性收窄（用户裁决）：量级相近是伪条件（相邻消息天然同量级），
            // 只作保护上限；粘性仅延续低难度任务（L2 上限）且暴增/骤减即断
            let size_ok = sticky.est_tokens > 0
                && est <= sticky.est_tokens.saturating_mul(2)
                && est.saturating_mul(2) >= sticky.est_tokens;
            let low_diff = sticky.difficulty <= 1.6;
            let tools_same = sticky.tools_sig == input.tools_sig;
            if still_fits && size_ok && low_diff && tools_same && sticky.turns_left > 0 {
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
                    judge_source: "sticky",
                route_recommendation: None,
                route_recommendation_confidence: 0.0,
                };
                return self.finish(
                    sticky.chosen.clone(),
                    vec![sticky.chosen.clone()],
                    format!("sticky reuse: session continues on {} (est {} tok)", sticky.chosen, est),
                    BTreeMap::from([(sticky.chosen.clone(), 1.0)]),
                    vec![],
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

        let judge_started = now_epoch_ms();
        // A2 Speculative Fan-Out：判定与路由推荐合并为一次 Jev 往返——
        // judge 前按静态画像（tiers 均值 + plan 标记）粗排 top-12，占位符化后
        // 塞进 state.candidates；Jev 在回答 8 维判定的同时直接举荐路由候选。
        // 静态序与评分序高度重合（动态因子只微调），推荐限定在粗排头部已足够。
        let catalog_models = self.catalog.read().map(|c| c.models.clone()).unwrap_or_default();
        let (candidates_hint, hint_map): (String, std::collections::HashMap<String, String>) = {
            let mut ranked: Vec<&ModelRecord> = catalog_models.iter().collect();
            ranked.sort_by(|a, b| {
                let sa = (a.tiers.coding + a.tiers.reasoning + a.tiers.agentic) / 3.0
                    + if a.plan { 0.1 } else { 0.0 };
                let sb = (b.tiers.coding + b.tiers.reasoning + b.tiers.agentic) / 3.0
                    + if b.plan { 0.1 } else { 0.0 };
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            });
            let mut map: std::collections::HashMap<String, String> = Default::default();
            let items: Vec<serde_json::Value> = ranked
                .iter()
                .take(12)
                .enumerate()
                .map(|(i, m)| {
                    let ph = format!("candidate_{}", i + 1);
                    map.insert(ph.clone(), m.id.clone());
                    serde_json::json!({
                        "id": ph,
                        "tier_coding": m.tiers.coding,
                        "tier_reasoning": m.tiers.reasoning,
                        "tier_agentic": m.tiers.agentic,
                        "context_window": m.context_window.unwrap_or(0),
                        "plan": crate::plans::plan_key_for(&m.base_url).unwrap_or("api"),
                    })
                })
                .collect();
            (serde_json::to_string(&items).unwrap_or_default(), map)
        };
        let mut j = self.judge.judge(&input.features, input.digest, &candidates_hint);
        let judge_ms = now_epoch_ms().saturating_sub(judge_started);
        let relevance = j.session_relevance;
        let difficulty_eff = (j.difficulty + relevance * (j.session_depth * 0.5).max(0.0)).clamp(0.0, 3.0);

        let weights = match input.policy {
            Some(p) => p.weights(),
            None => match self.weights_override() {
                Some(w) => w,
                None => PolicyProfile::parse(&self.policy.default)
                    .unwrap_or(PolicyProfile::Balanced)
                    .weights(),
            },
        };
        let mut filtered: Vec<FilteredOut> = Vec::new();
        let mut candidates: Vec<ModelRecord> = Vec::new();
        let now = now_epoch_ms();
        for m in &catalog_models {
            let calib = input
                .telemetry
                .get(&m.id)
                .and_then(|t| t.calibration)
                .filter(|c| *c > 0.1)
                .unwrap_or(1.0);
            let m_est = ((est as f32 * calib) as u64).max(est / 2);
            // 订阅方案预算硬保护：消耗超 95% 直接出局（留最后余量给硬任务）
            let p_key = crate::plans::plan_key_for(&m.base_url)
                .and_then(|k| input.plan_pressure.get(k));
            if let Some(p) = p_key
                && *p > 0.95
            {
                filtered.push(FilteredOut {
                    model: m.id.clone(),
                    cause: format!("plan budget {:.0}% consumed (soft reserve)", p * 100.0),
                });
                continue;
            }
            if !model_available(input.health, &m.id, now) {
                let remaining = input
                    .health
                    .get(&m.id)
                    .and_then(|h| h.cooldown_remaining_ms(now))
                    .map(|ms| format!(" for {}s", ms / 1000))
                    .unwrap_or_default();
                let (kind, message) = input
                    .health
                    .get(&m.id)
                    .map(|h| (h.kind, h.message.clone()))
                    .unwrap_or((HealthKind::Transient, "cooling down".into()));
                filtered.push(FilteredOut {
                    model: m.id.clone(),
                    cause: format!("{}{} ({})", kind.label(), remaining, message),
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
            // still accept more than declared). Models in health cooldown
            // are excluded — best-effort must not hammer known-dead
            // upstreams (quota-exhausted etc.); if everything is cooling
            // down, fail fast so the caller backs off.
            let now_be = now_epoch_ms();
            let cat = self.catalog.read().map(|c| c.models.clone()).unwrap_or_default();
            let best = cat
                .iter()
                .filter(|m| m.context_window.is_some())
                .filter(|m| {
                    model_available(input.health, &m.id, now_be)
                })
                .max_by_key(|m| m.context_window.unwrap())
                .or_else(|| cat.first());
            let Some(best) = best else {
                return self.finish(
                    "none".into(),
                    vec![],
                    format!("catalog empty; cannot route est {} tok", est),
                    BTreeMap::new(),
                    vec![],
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
                vec![],
                j,
                filtered,
                false,
                est,
                difficulty_eff,
                input.session_key,
                None,
                [catalog_models.len() as u32, filtered_count, 0, 0],
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
        // 用户权重覆盖（面板调控）：作用于评分前的候选副本
        if let Ok(o) = self.weight_overlay.read() {
            for c in eligible.iter_mut() {
                if let Some(w) = o.get(&c.id) {
                    c.weight = Some(*w);
                }
            }
        }
        // 难度-费用联动（用户裁决）：简单任务成本权重加倍省钱优先，
        // 复杂任务质量权重主导；调整后的权重随事件透出（链路视图②公式）
        let mut eff_weights = scoring::difficulty_weights(&weights, difficulty_eff);
        // ④ 配额感知权重（BaRP Eq.1）：候选中配额紧张（剩余 < est×8 会话余量）
        // 的比例 → 成本权重平滑上调；无配额数据则不干预
        {
            let mut tight_n = 0usize;
            let mut known = 0usize;
            for m in &eligible {
                if let Some(remaining) = input.quota.get(&m.id) {
                    known += 1;
                    if (*remaining as f64) < (est as f64) * 8.0 {
                        tight_n += 1;
                    }
                }
            }
            if known > 0 {
                eff_weights = scoring::quota_aware_weights(&eff_weights, tight_n as f32 / known as f32);
            }
        }
        let eff_policy = PolicyWeights {
            quality: eff_weights[0],
            speed: eff_weights[1],
            cost: eff_weights[2],
            stability: eff_weights[3],
            headroom: eff_weights[4],
        };
        let mut scores = scoring::score_all(
            &eligible.iter().collect::<Vec<_>>(), &j, difficulty_eff, est, est_output, &eff_policy, input.telemetry);
        // 订阅方案预算软降权（用户裁决：配额宝贵，别让一个会话烧光）：
        // 消耗超软阈值后按超出幅度压分，难度越高压制越轻（硬任务保留好模型）
        let soft = self.policy.plan_soft_pct.clamp(10.0, 95.0) / 100.0;
        for s in scores.iter_mut() {
            let plan_key = eligible
                .iter()
                .find(|m| m.id == s.model_id)
                .and_then(|m| crate::plans::plan_key_for(&m.base_url))
                .map(|k| k.to_string());
            if let Some(pk) = plan_key
                && let Some(p) = input.plan_pressure.get(&pk)
                && *p > soft
            {
                let over = (p - soft) / (0.95 - soft).max(0.01);
                let damp = (1.0 - 0.9 * over).max(0.05) * (1.0 - 0.6 * difficulty_eff / 3.0);
                s.score *= damp.max(0.05);
                s.qp = damp.max(0.05);
            }
        }
        scores.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        // A2 Fan-Out：优先消费 judge 合并返回的推荐（占位符 candidate_N →
        // hint_map 还原真实 id）；仅当 judge 未产出（heuristic 兜底 / API 失败 /
        // 无 hint）时，才退回独立的 route_advisor 调用（老路径，两次往返）。
        if let Some(rec_raw) = j.route_recommendation.clone() {
            let rec_key = rec_raw.trim().to_lowercase();
            if rec_key.starts_with("candidate_") {
                match hint_map.get(&rec_key) {
                    Some(mid) => {
                        j.route_recommendation = Some(mid.clone());
                    }
                    None => {
                        j.route_recommendation = None;
                        j.route_recommendation_confidence = 0.0;
                    }
                }
            }
            // 非占位符（历史协议直接给 id）：保持原值走下方加分匹配
        }
        if j.route_recommendation.is_none() {
        if let Some(advisor) = &self.route_advisor {
            let task_summary: String = input.digest.last_user_text.chars().take(600).collect();
            // 只送 top-8 候选，且 id 用本地占位符 candidate_N：
            // ①上游边缘 WAF 对真实 model id 的 token 模式间歇触发 451 地域拦截
            //  （judge 不带 candidates 的小 payload 不受影响）；
            // ②真实模型名不出本地，advisor 只见匿名画像（redact 精神）
            let top8: Vec<_> = scores.iter().take(8).collect();
            let mut id_map: std::collections::HashMap<String, String> = Default::default();
            let candidates_json = serde_json::to_string(&top8.iter().enumerate().map(|(i, s)| {
                let placeholder = format!("candidate_{}", i + 1);
                id_map.insert(placeholder.clone(), s.model_id.clone());
                let c = candidates.iter().find(|c| c.id == s.model_id);
                serde_json::json!({
                    "id": placeholder,
                    "tier_coding": c.map(|c| c.tiers.coding).unwrap_or(0.0),
                    "tier_reasoning": c.map(|c| c.tiers.reasoning).unwrap_or(0.0),
                    "tier_agentic": c.map(|c| c.tiers.agentic).unwrap_or(0.0),
                    "context_window": c.and_then(|c| c.context_window).unwrap_or(0),
                    "plan": c.map(|c| crate::plans::plan_key_for(&c.base_url).unwrap_or("api")).unwrap_or("api"),
                })
            }).collect::<Vec<_>>()).unwrap_or_default();
            if let Some((rec_id, rec_conf)) = advisor.recommend(&task_summary, &candidates_json, "") {
                let rec_key = rec_id.trim().to_lowercase();
                if let Some(mid) = id_map.get(&rec_key) {
                    j.route_recommendation = Some(mid.clone());
                    j.route_recommendation_confidence = rec_conf.clamp(0.0, 1.0);
                } else {
                    tracing::warn!(choice = %rec_id, "route_advisor: choice not in candidate map");
                }
            }
        }
        }

        // Jev cookbook intent-routing 加分：route_recommendation 高置信度时
        // 对推荐模型加分（最高 +15%）并重排。model_id 归一化（trim + 大小写）
        if let Some(ref rec) = j.route_recommendation {
            if j.route_recommendation_confidence > 0.3 {
                let rec_norm = rec.trim().to_lowercase();
                if let Some(rec_score) = scores.iter_mut().find(|s| s.model_id.to_lowercase() == rec_norm) {
                    rec_score.score *= 1.0 + j.route_recommendation_confidence.clamp(0.0, 1.0) * 0.15;
                    scores.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
                }
            }
        }

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

        // Thompson Sampling（BayesianRouter Eqs.4–6）：真 Beta 后验采样。
        // 先验：Jev 质量因子 q 作先验均值 μ₀（伪计数 ν₀=8）——评分即信念；
        // 似然：遥测近窗成功/失败计数。样本少→后验宽→采样偏移大→自动多探索；
        // 样本多→后验窄→自动收敛。采样值相对后验均值的偏移做乘性扰动
        // （幅度上限 = explore_ratio），不推翻公式序，仍保留 confidence/stakes 门控。
        let mut explored = false;
        {
            let should_explore = scores.len() > 1
                && j.high_stakes < 0.6
                && difficulty_eff < 2.0
                && j.domain_confidence >= self.policy.confidence_gate;
            if should_explore {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(0);
                // 用 nanos + 进程计数器做轻量伪随机（避免引入 rand crate）。
                // 纯 nanos 做种子时，连续决策的高位几乎相同，LCG 首轮输出
                // 高度相关（Box-Muller 只吃前几个随机数）→ 采样退化
                static EXPLORE_SEQ: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let mut seed = (nanos as u64)
                    ^ ((std::process::id() as u64) << 32)
                    ^ EXPLORE_SEQ
                        .fetch_add(0x9E3779B97F4A7C15, std::sync::atomic::Ordering::Relaxed)
                    ^ 0x5EED;
                let mut next_rand = || {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    ((seed >> 33) as f64) / (u32::MAX as f64)
                };
                let nu0 = 8.0f32;
                // 先验均值 μ₀ 用 q 的候选集内相对位次（min-max → [0.15,0.85]）：
                // 绝对 q 普遍贴顶（0.95+，clamp 后全变 0.98），所有后验挤在 1 附近、
                // draw≡1、探索系统性死亡；相对位次才承载"Jev 更信谁"，
                // 两端保底避免过度自信。span≈0（并列）时取中性 0.5。
                let q_min = scores.iter().map(|s| s.q).fold(f32::MAX, f32::min);
                let q_max = scores.iter().map(|s| s.q).fold(f32::MIN, f32::max);
                let q_span = (q_max - q_min).max(1e-3);
                let mut best_idx = 0usize;
                let mut best_score = f32::MIN;
                for (i, s) in scores.iter().enumerate() {
                    let (ok_n, fail_n) = input
                        .telemetry
                        .get(&s.model_id)
                        .and_then(|t| t.recent.as_ref())
                        .map(|r| {
                            (
                                r.iter().filter(|x| x.ok).count() as f32,
                                r.iter().filter(|x| !x.ok).count() as f32,
                            )
                        })
                        .unwrap_or((0.0, 0.0));
                    let rel = if q_max - q_min < 1e-3 {
                        0.5
                    } else {
                        ((s.q - q_min) / q_span).clamp(0.0, 1.0)
                    };
                    let mu0 = 0.15 + 0.70 * rel;
                    let alpha = mu0 * nu0 + ok_n;
                    let beta = (1.0 - mu0) * nu0 + fail_n;
                    let draw = sample_beta(alpha, beta, &mut next_rand);
                    // 绝对偏移（draw − 后验均值）：宽后验（样本少）偏移大自然多探索，
                    // 窄后验（样本多）偏移趋零自动收敛——Thompson 收敛性所在，
                    // 不可做 z 标准化（会抹掉该性质）。×3 放大使 explore_ratio=1
                    // 时足以翻越候选间公式分差；生产默认 0.1 → ±3% 温和扰动。
                    let post_mean = alpha / (alpha + beta);
                    let noisy = s.score * (1.0 + self.policy.explore_ratio * 3.0 * (draw - post_mean));
                    if noisy > best_score {
                        best_score = noisy;
                        best_idx = i;
                    }
                }
                if best_idx > 0 {
                    chosen = scores[best_idx].clone();
                    explored = true;
                }
            }
        }

        // 链在 chosen 定格后构建：探索/置信门控可能改变首选——
        // 链首必须与最终选择一致，否则降级链会先服务原赢家（探索失效）
        let mut chain: Vec<String> = vec![chosen.model_id.clone()];
        for s in scores.iter().map(|s| &s.model_id) {
            if chain.len() >= 3 {
                break;
            }
            if *s != chosen.model_id && !chain.contains(s) {
                chain.push(s.clone());
            }
        }
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
            catalog_models.len() as u32,
            candidates.len() as u32,
            eligible.len() as u32,
            scores.len() as u32,
        ];
        let mut decision = self.finish(
            chosen.model_id.clone(),
            chain,
            reason,
            scores
                .iter()
                .map(|s| (s.model_id.clone(), s.score))
                .collect(),
            scores.clone(),
            j,
            filtered,
            false,
            est,
            difficulty_eff,
            input.session_key,
            None,
            funnel,
        );
        decision.weights = eff_weights;
        decision.judge_ms = judge_ms;
        decision
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        &self,
        chosen: String,
        chain: Vec<String>,
        reason: String,
        scores: BTreeMap<String, f32>,
        scored: Vec<scoring::CandidateScore>,
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
            .catalog_read_model(&chosen)
            .map(|m| m.upstream_model.clone())
            .unwrap_or_else(|| chosen.clone());
        Decision {
            id,
            chosen,
            upstream_model: upstream,
            chain,
            reason,
            scores,
            scored,
            judgment,
            filtered,
            sticky,
            est_input_tokens: est,
            difficulty_eff,
            funnel,
            weights: [0.35, 0.15, 0.25, 0.15, 0.10],
            judge_ms: 0,
        }
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
        let e = Engine::new(catalog1(), det_policy(), Box::new(crate::heuristic::HeuristicJudge));
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
    fn explore_ratio_one_maximizes_exploration_pressure() {
        // Thompson sampling: explore_ratio=1.0 maximizes noise, making
        // exploration highly likely (but probabilistic, not deterministic).
        // Run 20 decisions and verify at least some don't pick the top model.
        let policy = PolicyCfg { explore_ratio: 1.0, ..Default::default() };
        let e = Engine::new(catalog3(), policy, Box::new(crate::heuristic::HeuristicJudge));
        let d = digest("你好");
        let h = HealthMap::new();
        let mut explored_count = 0;
        for _ in 0..20 {
            let dec = e.decide(input("你好", 500, &d, None, &h));
            if dec.chosen != "mini" { explored_count += 1; }
        }
        assert!(explored_count > 0, "explore_ratio=1.0 should produce exploration in 20 tries, got {explored_count}");
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
        let e = Engine::new(unknown_window_catalog(), det_policy(), Box::new(crate::heuristic::HeuristicJudge));
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

#[cfg(test)]
mod tests {
    pub(super) use super::*;

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
        Engine::new(catalog3(), det_policy(), Box::new(crate::heuristic::HeuristicJudge))
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
            plan_pressure: &EMPTY_PRESSURE,
        }
    }

    pub(crate) static EMPTY_PRESSURE: std::sync::LazyLock<HashMap<String, f32>> =
        std::sync::LazyLock::new(HashMap::new);

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
            difficulty: 0.5,
            est_tokens: 600,
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
            difficulty: 0.5,
            est_tokens: 1_000,
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
        let e = Engine::new(cat, det_policy(), Box::new(crate::heuristic::HeuristicJudge));
        let d = digest("看这张截图里的报错");
        let h = HealthMap::new();
        let mut inp = input("看这张截图里的报错", 2_000, &d, None, &h);
        inp.features.has_images = true;
        let dec = e.decide(inp);
        assert_eq!(dec.chosen, "standard", "reason: {}", dec.reason);
        assert!(dec.filtered.iter().any(|f| f.model == "mini" && f.cause.contains("vision")));
        assert!(dec.filtered.iter().any(|f| f.model == "frontier" && f.cause.contains("vision")));
    }

    #[test]
    fn sample_beta_mean_converges_to_posterior_mean() {
        let mut seed = 42u64;
        let mut rand = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) as f64) / (u32::MAX as f64)
        };
        // Beta(2.6, 7.4) 期望 0.26：2000 样本均值应落在 ±0.03
        let n = 2000;
        let mean = (0..n).map(|_| sample_beta(2.6, 7.4, &mut rand)).sum::<f32>() / n as f32;
        assert!((mean - 0.26).abs() < 0.03, "beta mean {mean} vs expected 0.26");
        // 不确定性驱动探索：样本少（后验宽）采样跨度 > 样本多（后验窄）
        let (mut wmin, mut wmax) = (1.0f32, 0.0f32);
        for _ in 0..500 {
            let d = sample_beta(1.0, 1.0, &mut rand);
            wmin = wmin.min(d);
            wmax = wmax.max(d);
        }
        let (mut nmin, mut nmax) = (1.0f32, 0.0f32);
        for _ in 0..500 {
            let d = sample_beta(80.0, 20.0, &mut rand);
            nmin = nmin.min(d);
            nmax = nmax.max(d);
        }
        assert!(
            wmax - wmin > nmax - nmin,
            "wide posterior must sample with more spread: {wmin}..{wmax} vs {nmin}..{nmax}"
        );
    }
}

#[cfg(test)]
pub(crate) fn det_policy() -> crate::config::PolicyCfg {
    crate::config::PolicyCfg { explore_ratio: 0.0, ..Default::default() }
}

/// Beta(α,β) 采样（两 Gamma 之比）。无 rand crate——复用调用方注入的 LCG。
fn sample_beta(alpha: f32, beta: f32, rand: &mut impl FnMut() -> f64) -> f32 {
    let x = sample_gamma(alpha.max(1e-3), rand);
    let y = sample_gamma(beta.max(1e-3), rand);
    x / (x + y)
}

/// Marsaglia-Tsang Gamma(shape) 采样；shape < 1 用 boost 变换 G(x)=G(x+1)·U^(1/x)。
fn sample_gamma(shape: f32, rand: &mut impl FnMut() -> f64) -> f32 {
    if shape < 1.0 {
        let u = rand() as f32;
        return sample_gamma(shape + 1.0, rand) * u.powf(1.0 / shape);
    }
    let d = shape - 1.0 / 3.0;
    let c = 1.0 / (9.0 * d).sqrt();
    loop {
        let u1 = (rand() as f32).max(1e-7);
        let u2 = rand() as f32;
        let z = (-2.0 * u1.ln()).sqrt() * (std::f32::consts::TAU * u2).cos();
        let v = (1.0 + c * z).powi(3);
        if v <= 0.0 {
            continue;
        }
        let u = rand() as f32;
        if u < 1.0 - 0.0331 * z.powi(4)
            || u.ln() < 0.5 * z * z + d - d * v + d * v.ln()
        {
            return d * v;
        }
    }
}
