use turbine_core::types::{DigestSignals, Judge, JudgmentSet, RequestFeatures};

/// M2 will add typesafe/laya HTTP backends behind this same constructor.
pub enum DecisionBackend {
    Heuristic,
}

pub struct HeuristicBackend;

impl Judge for HeuristicBackend {
    fn judge(&self, features: &RequestFeatures, digest: &DigestSignals) -> JudgmentSet {
        turbine_core::heuristic::HeuristicJudge.judge(features, digest)
    }
}

/// 双判官取严融合（决策记录 #24 扩展）：Jev 语义判定为主，heuristic 为
/// 独立第二意见；冲突时取更保守值。专治 Jev 的 CJK 弱点——中文任务被
/// 误判为 Other/简单时，heuristic 关键词（中英词表）把域和难度救回来。
pub struct HybridBackend {
    primary: Box<dyn Judge>,
    secondary: turbine_core::heuristic::HeuristicJudge,
}

impl Judge for HybridBackend {
    fn judge(&self, features: &RequestFeatures, digest: &DigestSignals) -> JudgmentSet {
        let j = self.primary.judge(features, digest);
        let h = self.secondary.judge(features, digest);
        turbine_core::types::fuse_judgments(&j, &h)
    }
}

impl DecisionBackend {
    /// `auto`: upgrade to TypeSafe Jev when TYPESAFE_API_KEY is present,
    /// otherwise heuristic. Explicit "heuristic"/"typesafe" force a choice;
    /// any backend failure degrades to heuristic inside the judge itself.
    pub fn build(kind: &str) -> Box<dyn Judge> {
        match kind {
            "heuristic" => Box::new(HeuristicBackend),
            "typesafe" => match crate::typesafe::TypesafeBackend::from_env() {
                Some(b) => {
                    tracing::info!("decision backend: typesafe jev");
                    Box::new(b)
                }
                None => {
                    tracing::warn!("TYPESAFE_API_KEY missing, decision backend falls back to heuristic");
                    Box::new(HeuristicBackend)
                }
            },
            "laya" => match crate::laya::LayaBackend::from_env() {
                Some(b) => {
                    tracing::info!("decision backend: laya sidecar");
                    Box::new(b)
                }
                None => Box::new(HeuristicBackend),
            },
            "auto" => match crate::typesafe::TypesafeBackend::from_env() {
                Some(b) => {
                    tracing::info!("decision backend: hybrid (jev primary + heuristic rescue)");
                    Box::new(HybridBackend { primary: Box::new(b), secondary: turbine_core::heuristic::HeuristicJudge })
                }
                None => Box::new(HeuristicBackend),
            },
            other => {
                tracing::warn!(backend = other, "unknown decision backend, falling back to heuristic");
                Box::new(HeuristicBackend)
            }
        }
    }
}
