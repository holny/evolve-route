//! Laya decision backend via the local Python sidecar (scripts/laya_server.py).
//!
//! State is structure-first with short text heads only — laya-multilingual
//! carries 1024 context, so the state must stay tiny. Fully local: zero
//! per-call cost, no data leaves the machine. Any sidecar failure falls
//! back to the heuristic judge.

use async_trait::async_trait;
use turbine_core::types::*;
use serde_json::json;
use std::time::Duration;

pub const LAYA_STATE_HEAD_CHARS: usize = 160;

pub struct LayaBackend {
    client: reqwest::Client,
    endpoint: String,
}

#[async_trait]
impl Judge for LayaBackend {
    fn judge(&self, features: &RequestFeatures, digest: &DigestSignals) -> JudgmentSet {
        let payload = self.build_payload(features, digest);
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let call = async move {
            client
                .post(&endpoint)
                .timeout(Duration::from_millis(1500))
                .json(&payload)
                .send()
                .await?
                .error_for_status()?
                .json::<serde_json::Value>()
                .await
        };
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::try_current().ok().and_then(|h| h.block_on(async { call.await.ok() }))
        });
        match result.as_ref().and_then(crate::typesafe::parse_judgment) {
            Some(j) => j,
            None => {
                tracing::warn!("laya judgment failed, falling back to heuristic");
                turbine_core::heuristic::HeuristicJudge.judge(features, digest)
            }
        }
    }
}

impl LayaBackend {
    pub fn from_env() -> Option<Self> {
        Some(Self {
            client: reqwest::Client::new(),
            endpoint: std::env::var("LAYA_URL").unwrap_or_else(|_| "http://127.0.0.1:8321/v1/judge".into()),
        })
    }

    fn build_payload(&self, features: &RequestFeatures, digest: &DigestSignals) -> serde_json::Value {
        let cjk = features.cjk_ratio;
        let lang_hint = if cjk > 0.15 { "multilingual" } else { "typed-decisions" };
        json!({
            "lang_hint": lang_hint,
            "state": {
                "task": {
                    "length_bucket": bucket(features.user_text_chars, &[30, 100, 400, 1500]),
                    "code_density": (features.code_density * 100.0).round() / 100.0,
                    "tool_count": features.tool_count,
                    "has_images": features.has_images,
                    "est_tokens": features.est_input_tokens,
                },
                "session": {
                    "first_task_head": head(&digest.first_user_text, LAYA_STATE_HEAD_CHARS / 2),
                    "current_head": head(&digest.last_user_text, LAYA_STATE_HEAD_CHARS),
                    "continues_topic": digest.has_deixis || digest.overlap_ratio > 0.15,
                    "topic_shift": digest.topic_shift_marker,
                    "tools_seen": digest.session_tools_seen.min(999),
                },
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
