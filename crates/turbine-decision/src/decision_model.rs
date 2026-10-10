//! 通用决策模型接入层：任何 HTTP 端点只要实现「8 问判定协议」即可成为
//! 决策模型——通过配置声明 URL/请求模板/响应映射，核心路由零改动。
//! 内置实现：heuristic / typesafe(Jev) / laya，均适配同一 Judge 接口。

use async_trait::async_trait;
use turbine_core::types::{DigestSignals, Judge, JudgmentSet, RequestFeatures};
use serde_json::Value;

/// 通用判定请求：state 由各后端按自身协议组装
#[derive(Debug, Clone)]
pub struct JudgmentRequest {
    pub features: RequestFeatures,
    pub digest: DigestSignals,
    pub lang_hint: String,
}

pub struct GenericHttpModel {
    pub id: String,
    pub endpoint: String,
    pub headers: Vec<(String, String)>,
    pub timeout_ms: u64,
    pub client: reqwest::Client,
}

#[async_trait]
impl Judge for GenericHttpModel {
    fn judge(&self, features: &RequestFeatures, digest: &DigestSignals) -> JudgmentSet {
        // 兜底值：任何解析失败都落到 heuristic 同构结果，永不阻塞
        let fallback = turbine_core::heuristic::HeuristicJudge.judge(features, digest);
        let payload = serde_json::json!({
            "state": {
                "task": {
                    "length_bucket": (features.user_text_chars > 1000).to_string(),
                    "code_density": features.code_density,
                    "tool_count": features.tool_count,
                    "has_images": features.has_images,
                    "est_tokens": features.est_input_tokens,
                },
                "session": {
                    "current_head": digest.last_user_text.chars().take(400).collect::<String>(),
                    "continues_topic": digest.has_deixis || digest.overlap_ratio > 0.15,
                },
            }
        });
        let mut req = self.client.post(&self.endpoint).json(&payload);
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::try_current().ok().and_then(|h| {
                h.block_on(async { req.send().await.ok()?.error_for_status().ok()?.json::<Value>().await.ok() })
            })
        });
        let Some(v) = resp else { return fallback };
        parse_judgment_response(&v).unwrap_or(fallback)
    }
}

/// 从标准 answers 形状（TypeSafe/laya/generic 共用）解析判定集
pub fn parse_judgment_response(v: &Value) -> Option<JudgmentSet> {
    let a = v.get("answers")?;
    let domain = match a.get("task_domain")?.get("choice")?.as_str()? {
        "code" => turbine_core::types::Domain::Code,
        "math_logic" => turbine_core::types::Domain::MathLogic,
        "writing" => turbine_core::types::Domain::Writing,
        "factual_lookup" => turbine_core::types::Domain::Lookup,
        "data_analysis" => turbine_core::types::Domain::Data,
        "chitchat" => turbine_core::types::Domain::Chitchat,
        "agent_ops" => turbine_core::types::Domain::AgentOps,
        _ => turbine_core::types::Domain::Other,
    };
    let noul = |k: &str| {
        a.get(k)
            .and_then(|x| x.get("noul"))
            .and_then(|x| x.as_f64())
            .unwrap_or(0.5) as f32
    };
    let score = |k: &str| a.get(k).and_then(|x| x.get("score")).and_then(|x| x.as_f64()).unwrap_or(1.0) as f32;
    let conf = |k: &str| a.get(k).and_then(|x| x.get("confidence")).and_then(|x| x.as_f64()).unwrap_or(0.5) as f32;
    Some(JudgmentSet {
        domain,
        domain_confidence: conf("task_domain"),
        difficulty: score("difficulty").clamp(0.0, 3.0),
        difficulty_confidence: conf("difficulty"),
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
