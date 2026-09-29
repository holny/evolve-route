//! TypeSafe Jev decision backend (System One: state + typed questions).
//!
//! Context management: the 8 question definitions are the stable part of
//! every request; only `state` varies. State is budget-capped (extractive
//! digest + bucketized features, no raw content under redact=true) so each
//! decision costs a bounded number of input tokens. Sticky fast-path and
//! judgment reuse keep the call frequency low; on any backend failure we
//! fall back to the heuristic judge so routing never blocks.

use async_trait::async_trait;
use mr_core::types::*;
use serde_json::json;
use std::time::Duration;

pub const STATE_TOKEN_BUDGET: usize = 2048;

pub struct TypesafeBackend {
    client: reqwest::Client,
    api_key: String,
    model: String,
    endpoint: String,
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
                mr_core::heuristic::HeuristicJudge.judge(features, digest)
            }
        }
    }
}

impl TypesafeBackend {
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("TYPESAFE_API_KEY").ok().filter(|k| !k.is_empty())?;
        Some(Self {
            client: reqwest::Client::new(),
            api_key,
            model: std::env::var("TYPESAFE_MODEL").unwrap_or_else(|_| "jev-latest".into()),
            endpoint: "https://api.typesafe.ai/v1/systemone".into(),
        })
    }

    fn build_payload(&self, features: &RequestFeatures, digest: &DigestSignals) -> serde_json::Value {
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
                "first_task_head": head(&digest.first_user_text, 200),
                "current_head": head(&digest.last_user_text, 400),
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
                    "type": "score",
                    "instructions": "How hard is this request for a language model?",
                    "criteria": [
                        "trivial: a lookup or one-liner",
                        "easy: short answer, no reasoning",
                        "moderate: several steps or careful editing",
                        "hard: long multi-step reasoning or specialist knowledge"
                    ]
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
    Some(JudgmentSet {
        domain,
        domain_confidence: a.get("task_domain").and_then(|x| x.get("confidence")).and_then(|x| x.as_f64()).unwrap_or(0.5) as f32,
        difficulty: score("difficulty").clamp(0.0, 3.0),
        difficulty_confidence: a.get("difficulty").and_then(|x| x.get("confidence")).and_then(|x| x.as_f64()).unwrap_or(0.5) as f32,
        needs_vision: noul("needs_vision"),
        is_trivial: noul("is_trivial"),
        tool_heavy: noul("tool_heavy"),
        high_stakes: noul("high_stakes"),
        session_relevance: noul("session_relevance"),
        session_depth: score("session_depth").clamp(0.0, 3.0),
    })
}
