//! TypeSafe Jev decision backend (System One: state + typed questions).
//!
//! Context management: the 8 question definitions are the stable part of
//! every request; only `state` varies. State is budget-capped (extractive
//! digest + bucketized features, no raw content under redact=true) so each
//! decision costs a bounded number of input tokens. Sticky fast-path and
//! judgment reuse keep the call frequency low; on any backend failure we
//! fall back to the heuristic judge so routing never blocks.

use async_trait::async_trait;
use evolve_core::types::*;
use serde_json::json;
use std::time::Duration;

pub const STATE_TOKEN_BUDGET: usize = 2048;

/// advisor 451/5xx 失败后的全局冷却截止（epoch ms）：风控期内不再白打调用
static ADVISOR_COOLDOWN_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone)]
pub struct DecisionModelBackend {
    pub client: reqwest::Client,
    pub api_key: String,
    pub model: String,
    pub endpoint: String,
    pub redact: bool,
}

#[async_trait]
impl Judge for DecisionModelBackend {
    fn judge(&self, features: &RequestFeatures, digest: &DigestSignals, candidates_hint: &str) -> JudgmentSet {
        let payload = self.build_payload(features, digest, candidates_hint);
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let api_key = self.api_key.clone();
        let _model = self.model.clone();
        let call = async move {
            client
                .post(&endpoint)
                .bearer_auth(&api_key)
                .timeout(Duration::from_millis(3000))
                .json(&payload)
                .send()
                .await?
                .error_for_status()?
                .json::<serde_json::Value>()
                .await
        };
        // Engine is sync; we are always called from the multi-thread runtime.
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::try_current().ok().and_then(|h| h.block_on(async { call.await.ok() }))
        });
        match result.as_ref().and_then(parse_judgment) {
            Some(j) => j,
            None => {
                tracing::warn!("typesafe judgment failed, falling back to heuristic");
                evolve_core::heuristic::HeuristicJudge.judge(features, digest, candidates_hint)
            }
        }
    }
}

impl DecisionModelBackend {
    /// v2 直接路由推荐：决策模型看到任务文本+候选全画像，直接推荐哪个模型。
    /// 返回 (model_id, confidence)。调用失败返回 None（调用方走公式回退）。
    pub fn recommend_route(
        &self,
        task_summary: &str,
        candidates_json: &str,
        policy: &str,
    ) -> Option<(String, f32)> {
        // 冷却期内直接跳过（静默，不刷日志）
        if now_millis() < ADVISOR_COOLDOWN_UNTIL.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        // choice 问题的 criteria 是必填的选项映射（缺失 → 422）：
        // 从候选画像提取占位 id 动态生成
        let cand_ids: Vec<String> = serde_json::from_str::<serde_json::Value>(candidates_json)
            .ok()
            .and_then(|v| v.as_array().cloned())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("id").and_then(|i| i.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mut criteria = serde_json::Map::new();
        for id in &cand_ids {
            criteria.insert(
                id.clone(),
                json!("candidate profile listed in state.candidates at this position"),
            );
        }
        let payload = serde_json::json!({
            "model": &self.model,
            "state": {
                "task": task_summary.chars().take(600).collect::<String>(),
                "candidates": candidates_json.chars().take(6_000).collect::<String>(),
                "policy": policy,
            },
            "questions": {
                "route_recommendation": {
                    "type": "choice",
                    "instructions": "Given this task and the candidate model profiles in state.candidates, which single model should handle this request? Consider capability match, cost efficiency, reliability track record, context window fit, and quota availability. Reply with exactly one candidate id from the criteria list.",
                    "criteria": criteria,
                }
            }
        });
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let api_key = self.api_key.clone();
        let _model = self.model.clone();
        let call_once = |payload: serde_json::Value| {
            let client = client.clone();
            let endpoint = endpoint.clone();
            let api_key = api_key.clone();
            async move {
                let resp = client
                    .post(&endpoint)
                    .bearer_auth(&api_key)
                    .timeout(Duration::from_secs(5))
                    .json(&payload)
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                let status = resp.status();
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(format!("HTTP {status}: {body}"));
                }
                resp.json::<serde_json::Value>()
                    .await
                    .map_err(|e| e.to_string())
            }
        };
        // 451/5xx 间歇性（边缘风控），退避后重试一次
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::try_current().map(|h| {
                h.block_on(async {
                    let first = call_once(payload.clone()).await;
                    let retriable = |e: &str| {
                        [" 451", " 500", " 502", " 503", " 504", " 429"]
                            .iter()
                            .any(|c| e.contains(c))
                    };
                    if first.as_ref().err().map_or(false, |e| retriable(e)) {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        call_once(payload).await
                    } else {
                        first
                    }
                })
            })
        });
        match result {
            Err(_) => {
                tracing::warn!("route_advisor: no tokio runtime handle for recommend call");
                None
            }
            Ok(Err(e)) => {
                if e.contains(" 451") || e.contains(" 429") || e.contains(" 5") {
                    ADVISOR_COOLDOWN_UNTIL.store(
                        now_millis() + 60_000,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
                tracing::warn!(error = %e, "route_advisor: recommend call failed");
                None
            }
            Ok(Ok(v)) => {
                let parsed = (|| {
                    let choice = v.pointer("/answers/route_recommendation/choice")?.as_str()?.to_string();
                    let conf = v.pointer("/answers/route_recommendation/confidence")
                        .and_then(|c| c.as_f64()).unwrap_or(0.5) as f32;
                    Some((choice, conf))
                })();
                if parsed.is_none() {
                    tracing::warn!(resp = %v, "route_advisor: recommend response missing choice");
                }
                parsed
            }
        }
    }

    /// env 读取：DECISION_MODEL_* 为标准名，TYPESAFE_* 为历史名（回退兼容，
    /// 已部署环境无需改动）。
    fn env_var(names: &[&str]) -> Option<String> {
        for n in names {
            if let Ok(v) = std::env::var(n) {
                return Some(v);
            }
        }
        None
    }

    pub fn from_env() -> Option<Self> {
        let api_key = Self::env_var(&["DECISION_MODEL_API_KEY", "TYPESAFE_API_KEY"])
            .filter(|k| !k.is_empty())?;
        Some(Self {
            client: reqwest::Client::new(),
            api_key,
            model: Self::env_var(&["DECISION_MODEL_MODEL", "TYPESAFE_MODEL"])
                .unwrap_or_else(|| "jev-latest".into()),
            // A1：endpoint 可指向本地开源决策模型（System One 协议兼容——
            // Intern-Decision / StartLux-Decision 等，vLLM/ollama 服务化后
            // 改 DECISION_MODEL_ENDPOINT 即切换；默认仍为 TypeSafe 云端）
            endpoint: Self::env_var(&["DECISION_MODEL_ENDPOINT", "TYPESAFE_ENDPOINT"])
                .unwrap_or_else(|| "https://api.typesafe.ai/v1/systemone".into()),
            // privacy default: only bucketed features cross the wire
            redact: Self::env_var(&["DECISION_MODEL_REDACT", "TYPESAFE_REDACT"])
                .map(|v| v != "0")
                .unwrap_or(true),
        })
    }

    /// ⑧ 能力卡蒸馏（FlyRoute）：观测统计达标后由 Jev 重估模型 tiers。
    /// 返回 [reasoning, coding, vision, agentic]（0-1）。失败返回 None——
    /// 静默跳过，下个调度周期再试；失败永不影响路由主链路。
    pub async fn distill_tiers(
        &self,
        model_id: &str,
        stats_summary: &serde_json::Value,
    ) -> Option<[f32; 4]> {
        let criteria = ["weak", "moderate", "strong", "elite"];
        let tier_q = |dim: &str| {
            json!({
                "type": "score",
                "instructions": format!(
                    "Rate this model's {} capability based on the observed production statistics in state.observations.",
                    dim
                ),
                "criteria": criteria,
            })
        };
        let payload = json!({
            "model": &self.model,
            "state": {
                "task": format!(
                    "Re-estimate the capability tiers of model '{}' from its observed production statistics. Statistics reflect real traffic outcomes.",
                    model_id
                ),
                "observations": stats_summary,
            },
            "questions": {
                "reasoning_tier": tier_q("reasoning"),
                "coding_tier": tier_q("coding"),
                "vision_tier": tier_q("vision"),
                "agentic_tier": tier_q("agentic tool use"),
            }
        });
        let resp = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .timeout(Duration::from_secs(20))
            .json(&payload)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json::<serde_json::Value>()
            .await
            .ok()?;
        // score 类型 answer 形状：answers.<key>.score，0-3 刻度（同 session_depth）
        let tier = |k: &str| -> f32 {
            resp.pointer(&format!("/answers/{k}/score"))
                .and_then(|s| s.as_f64())
                .map(|s| (s as f32 / 3.0).clamp(0.0, 1.0))
                .unwrap_or(0.5)
        };
        Some([
            tier("reasoning_tier"),
            tier("coding_tier"),
            tier("vision_tier"),
            tier("agentic_tier"),
        ])
    }

    /// A3b LLM-as-a-Judge：point-wise 充分性评分 [0,1]（backtest 采样用）。
    /// 单一维度（是否充分且正确回答）——防风格偏见；judge 与被评模型异构
    /// ——防自我偏好。失败返回 None。
    pub async fn judge_response_quality(&self, query: &str, response: &str) -> Option<f32> {
        let payload = json!({
            "model": &self.model,
            "state": {
                "task": query.chars().take(600).collect::<String>(),
                "response": response.chars().take(1200).collect::<String>(),
            },
            "questions": {
                "sufficiency": {
                    "type": "noul",
                    "instructions": "Does this response adequately and correctly answer the query? Judge only sufficiency and correctness — ignore style, tone, and wording.",
                }
            }
        });
        let resp = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .timeout(Duration::from_secs(15))
            .json(&payload)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json::<serde_json::Value>()
            .await
            .ok()?;
        resp.pointer("/answers/sufficiency/noul")
            .and_then(|v| v.as_f64())
            .map(|v| v as f32)
    }

    fn build_payload(&self, features: &RequestFeatures, digest: &DigestSignals, candidates_hint: &str) -> serde_json::Value {
        let (first_head, current_head) = if self.redact {
            // bucketed placeholder: no user text leaves the machine
            ("[text]".to_string(), "[text]".to_string())
        } else {
            (head(&digest.first_user_text, 200), head(&digest.last_user_text, 400))
        };
        let state = json!({
            "task": {
                "length_bucket": bucket(features.user_text_chars, &[30, 100, 400, 1500]),
                "code_density": (features.code_density * 100.0).round() / 100.0,
                "tool_count": features.tool_count,
                "has_images": features.has_images,
                "turn_count": features.turn_count.min(999),
                "est_tokens": features.est_input_tokens,
            },
            "session": {
                "first_task_head": first_head,
                "current_head": current_head,
                "continues_topic": digest.has_deixis || digest.overlap_ratio > 0.15,
                "topic_shift": digest.topic_shift_marker,
                "tools_seen": digest.session_tools_seen.min(999),
            },
        });
        let mut payload = json!({
            "model": self.model,
            "state": state,
            "questions": {
                "task_domain": {
                    "type": "choice",
                    "instructions": "What is the primary domain of the user's current request?",
                    "criteria": {
                        "code": "software engineering, refactoring, debugging, architecture, tests",
                        "math_logic": "mathematics, proofs, calculations, logic puzzles",
                        "writing": "creative or professional writing, emails, translation, copy",
                        "factual_lookup": "facts, definitions, how-to questions",
                        "data_analysis": "statistics, SQL, spreadsheets, metrics",
                        "chitchat": "greetings, small talk, pleasantries",
                        "agent_ops": "running commands, managing services, shell work",
                        "other": "none of the above"
                    }
                },
                "difficulty": {
                    "type": "choice",
                    "instructions": "How hard is this request for a language model? Pick the HIGHEST level that applies — anchor on the examples, not on a vague feeling.",
                    "criteria": {
                        "l1": "L1 trivial: greetings/small talk, single fact lookup, one-sentence translation, rename or reformat — zero reasoning chains",
                        "l2": "L2 moderate: single-file edit, one focused function, straightforward debugging, explain a known concept, careful multi-step but well-trodden work",
                        "l3": "L3 complex: multi-file refactor, architecture trade-offs, novel algorithm design, cross-module debugging, long multi-step plans",
                        "l4": "L4 critical: production changes touching money/auth/security/migrations, distributed consistency, performance-critical paths, frontier research problems"
                    }
                },
                "needs_vision": {"type": "noul", "instructions": "Does answering require seeing images?"},
                "is_trivial": {"type": "noul", "instructions": "Is this answerable in one short sentence with no tools?"},
                "tool_heavy": {"type": "noul", "instructions": "Is this request tool-intensive (many tool invocations needed)?"},
                "high_stakes": {"type": "noul", "instructions": "Could this request cause money, legal, medical, safety or production damage?"},
                "session_relevance": {"type": "noul", "instructions": "Does the current message continue the session's main task rather than switch topics?"},
                "session_depth": {
                    "type": "score",
                    "instructions": "How complex is the ongoing session task?",
                    "criteria": ["simple ongoing task", "moderate ongoing task", "complex ongoing task"]
                }
            }
        });
        // A2 Speculative Fan-Out：candidates hint（静态粗排 top-12 占位符画像，
        // 原生数组嵌入——JSON-in-string 形式曾触发上游 WAF 451）存在时，
        // 同一次请求附带 route_recommendation 问题：判定与举荐一次往返。
        if !candidates_hint.is_empty() {
            if let Ok(arr) = serde_json::from_str::<serde_json::Value>(candidates_hint) {
                payload["state"]["candidates"] = arr.clone();
                let mut criteria = serde_json::Map::new();
                if let Some(list) = arr.as_array() {
                    for x in list {
                        if let Some(id) = x.get("id").and_then(|i| i.as_str()) {
                            criteria.insert(
                                id.to_string(),
                                json!("candidate profile listed in state.candidates at this position"),
                            );
                        }
                    }
                }
                if !criteria.is_empty() {
                    payload["questions"]["route_recommendation"] = json!({
                        "type": "choice",
                        "instructions": "Given this task and the candidate model profiles in state.candidates, which single model should handle this request? Consider capability match, cost efficiency, reliability track record, context window fit, and quota availability. Reply with exactly one candidate id from the criteria list.",
                        "criteria": criteria,
                    });
                }
            }
        }
        payload
    }
}

fn bucket(v: usize, edges: &[usize]) -> usize {
    edges.iter().filter(|&&e| v > e).count()
}

fn head(s: &str, chars: usize) -> String {
    s.chars().take(chars).collect()
}

pub(crate) fn parse_judgment(v: &serde_json::Value) -> Option<JudgmentSet> {
    let a = v.get("answers")?;
    let domain = match a.get("task_domain")?.get("choice")?.as_str()? {
        "code" => Domain::Code,
        "math_logic" => Domain::MathLogic,
        "writing" => Domain::Writing,
        "factual_lookup" => Domain::Lookup,
        "data_analysis" => Domain::Data,
        "chitchat" => Domain::Chitchat,
        "agent_ops" => Domain::AgentOps,
        _ => Domain::Other,
    };
    let noul = |k: &str| a.get(k).and_then(|x| x.get("noul")).and_then(|x| x.as_f64()).unwrap_or(0.5) as f32;
    let score = |k: &str| a.get(k).and_then(|x| x.get("score")).and_then(|x| x.as_f64()).unwrap_or(1.0) as f32;
    // 锚定量表：L1-L4 级别映射到数值（消双峰——模型选级别而非凭空打分）
    let level = a.get("difficulty").and_then(|x| x.get("choice")).and_then(|x| x.as_str()).map(|s| s.to_string());
    let difficulty = match level.as_deref() {
        Some("l1") => 0.5,
        Some("l2") => 1.4,
        Some("l3") => 2.2,
        Some("l4") => 3.0,
        _ => score("difficulty").clamp(0.0, 3.0), // 兼容旧 score 型应答
    };
    Some(JudgmentSet {
        domain,
        domain_confidence: a.get("task_domain").and_then(|x| x.get("confidence")).and_then(|x| x.as_f64()).unwrap_or(0.5) as f32,
        difficulty,
        difficulty_confidence: a.get("difficulty").and_then(|x| x.get("confidence")).and_then(|x| x.as_f64()).unwrap_or(0.5) as f32,
        needs_vision: noul("needs_vision"),
        is_trivial: noul("is_trivial"),
        tool_heavy: noul("tool_heavy"),
        high_stakes: noul("high_stakes"),
        session_relevance: noul("session_relevance"),
        session_depth: score("session_depth").clamp(0.0, 3.0),
        route_recommendation: a.get("route_recommendation")
            .and_then(|x| x.get("choice"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        route_recommendation_confidence: a.get("route_recommendation")
            .and_then(|x| x.get("confidence"))
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0) as f32,
        judge_source: "decision_model",
    })
}

/// v2 直接路由：决策模型看到候选全画像 + 飞轮经验，直接推荐模型
pub struct RouteAdvisor {
    backend: DecisionModelBackend,
}

impl RouteAdvisor {
    pub fn new(backend: DecisionModelBackend) -> Self {
        Self { backend }
    }

    pub fn recommend(
        &self,
        task_text: &str,
        candidates_json: &str,
        session_summary: &str,
        policy: &str,
    ) -> Option<(String, String, f32)> {
        let payload = serde_json::json!({
            "model": &self.backend.model,
            "state": {
                "task": task_text.chars().take(600).collect::<String>(),
                "candidates": candidates_json.chars().take(6000).collect::<String>(),
                "session": session_summary.chars().take(300).collect::<String>(),
                "policy": policy,
            },
            "questions": {
                "route_recommendation": {
                    "type": "choice",
                    "instructions": {
                        "question": "Given this task and the candidate model profiles, which single model should handle this request?",
                        "note": "Consider capability match, cost efficiency, reliability track record, context window fit, and quota availability. Recommend exactly one candidate id.",
                    },
                }
            }
        });
        let resp = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::try_current().ok().and_then(|h| {
                h.block_on(async {
                    self.backend
                        .client
                        .post(&self.backend.endpoint)
                        .bearer_auth(&self.backend.api_key)
                        .timeout(Duration::from_secs(5))
                        .json(&payload)
                        .send()
                        .await
                        .ok()?
                        .error_for_status()
                        .ok()?
                        .json::<serde_json::Value>()
                        .await
                        .ok()
                })
            })
        })?;
        let choice = resp
            .pointer("/answers/route_recommendation/choice")
            .and_then(|c| c.as_str())?
            .to_string();
        let confidence = resp
            .pointer("/answers/route_recommendation/confidence")
            .and_then(|c| c.as_f64())
            .unwrap_or(0.5) as f32;
        let reasoning = resp
            .pointer("/answers/route_recommendation/reasoning")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        Some((choice, reasoning, confidence))
    }
}

impl evolve_core::types::RouteAdvisor for DecisionModelBackend {
    fn recommend(
        &self,
        task_summary: &str,
        candidates_json: &str,
        policy: &str,
    ) -> Option<(String, f32)> {
        self.recommend_route(task_summary, candidates_json, policy)
    }
}
