//! 通用决策模型抽象（决策记录 #25）。
//!
//! 任何能对同一组类型化问题给出同构 JudgmentSet 的系统都是一个决策模型
//! ——Jev / laya / heuristic 只是三个内置实现，后续新决策模型
//! （GLM-as-judge、自研微调模型、其他厂商 API）通过实现本接口或
//! generic-http 配置接入，核心路由零改动。

use serde::Serialize;
use crate::types::{JudgmentSet, RequestFeatures, DigestSignals};

/// 决策模型能力描述：用于按任务特征自动选择最合适的判定引擎。
#[derive(Debug, Clone, Serialize)]
pub struct DecisionModelCaps {
    pub id: String,
    /// 语言能力标记（如 ["en", "zh", "multi"]），供语言感知选择
    pub languages: Vec<String>,
    /// 延迟等级：local(<1ms) / fast(~50ms) / network(100ms+)
    pub latency_class: String,
    /// 是否需要网络出站（隐私考量）
    pub requires_network: bool,
    /// 每次判定的边际成本（美元，0 表示本地/免费）
    pub cost_per_judgment: f64,
    /// 上下文预算（state 可承载的 tokens）
    pub state_budget_tokens: u32,
}

/// 通用决策模型接口：所有判定后端的统一抽象。
pub trait DecisionModel: Send + Sync {
    fn caps(&self) -> DecisionModelCaps;
    fn judge(&self, features: &RequestFeatures, digest: &DigestSignals, candidates_hint: &str) -> JudgmentSet;
}
