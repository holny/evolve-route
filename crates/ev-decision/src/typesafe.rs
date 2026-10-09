//! TypeSafe Jev decision backend (System One: state + typed questions).
//!
//! Context management: the 8 question definitions are the stable part of
//! every request; only `state` varies. State is budget-capped (extractive
//! digest + bucketized features, no raw content under redact=true) so each
//! decision costs a bounded number of input tokens. Sticky fast-path and
//! judgment reuse keep the call frequency low; on any backend failure we
//! fall back to the heuristic judge so routing never blocks.

use async_trait::async_trait;
use ev_core::types::*;
use serde_json::json;
use std::time::Duration;

pub const STATE_TOKEN_BUDGET: usize = 2048;

pub struct TypesafeBackend {
    pub client: reqwest::Client,
    pub api_key: String,
    pub model: String,
    pub endpoint: String,
    pub redact: bool,
}

#[async_trait]
impl Judge for TypesafeBackend {
    fn judge(&self, features: &RequestFeatures, digest: &DigestSignals) -> JudgmentSet {
        let payload = self.build_payload(features, digest);
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
                ev_core::heuristic::HeuristicJudge.judge(features, digest)
            }
        }
    }
}

impl TypesafeBackend {
    /// v2 直接路由推荐：决策模型看到任务文本+候选全画像，直接推荐哪个模型。
    /// 返回 (model_id, confidence)。调用失败返回 None（调用方走公式回退）。
    pub fn recommend_route(
        &self,
        task_summary: &str,
        candidates_json: &str,
        policy: &str,
    ) -> Option<(String, f32)> {
        let payload = serde_json::json!({
            "model": &self.model,
            "state": {
                "task": task_summary.chars().take(600).collect::<String>(),
                "candidates": candidates_json.chars().take(30_000).collect::<String>(),
                "policy": policy,
            },
            "questions": {
                "route_recommendation": {
                    "type": "choice",
                    "instructions": {
                        "question": "Given this task and the candidate model profiles, which single model should handle this request? Consider capability match, cost efficiency, reliability track record, context window fit, and quota availability.",
                        "candidates_note": "Each option is a candidate model id from the routing catalog.",
                    },
                }
            }
        });
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let api_key = self.api_key.clone();
        let _model = self.model.clone();
        let call = async move {
            client
                .post(&endpoint)
                .bearer_auth(&api_key)
                .timeout(Duration::from_secs(5))
                .json(&payload)
                .send()
                .await?
                .error_for_status()?
                .json::<serde_json::Value>()
                .await
        };
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::try_current().ok().and_then(|h| {
                h.block_on(async { call.await.ok() })
            })
        })
        .as_ref()
        .and_then(|v| {
            let choice = v.pointer("/answers/route_recommendation/choice")?.as_str()?.to_string();
            let conf = v.pointer("/answers/route_recommendation/confidence")
                .and_then(|c| c.as_f64()).unwrap_or(0.5) as f32;
            Some((choice, conf))
        })
    }

    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("TYPESAFE_API_KEY").ok().filter(|k| !k.is_empty())?;
        Some(Self {
            client: reqwest::Client::new(),
            api_key,
            model: std::env::var("TYPESAFE_MODEL").unwrap_or_else(|_| "jev-latest".into()),
            endpoint: "https://api.typesafe.ai/v1/systemone".into(),
            // privacy default: only bucketed features cross the wire
            redact: std::env::var("TYPESAFE_REDACT").map(|v| v != "0").unwrap_or(true),
        })
    }

    fn build_payload(&self, features: &RequestFeatures, digest: &DigestSignals) -> serde_json::Value {
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
        json!({
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
        })
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
        judge_source: "decision_model",
    })
}

/// v2 直接路由：决策模型看到候选全画像 + 飞轮经验，直接推荐模型
pub struct RouteAdvisor {
    backend: TypesafeBackend,
}

impl RouteAdvisor {
    pub fn new(backend: TypesafeBackend) -> Self {
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
                "candidates": candidates_json.chars().take(30000).collect::<String>(),
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

impl ev_core::types::RouteAdvisor for TypesafeBackend {
    fn recommend(
        &self,
        task_summary: &str,
        candidates_json: &str,
        policy: &str,
    ) -> Option<(String, f32)> {
        self.recommend_route(task_summary, candidates_json, policy)
    }
}
