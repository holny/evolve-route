use mr_core::types::{DigestSignals, Judge, JudgmentSet, RequestFeatures};

/// M2 will add typesafe/laya HTTP backends behind this same constructor.
pub enum DecisionBackend {
    Heuristic,
}

pub struct HeuristicBackend;

impl Judge for HeuristicBackend {
    fn judge(&self, features: &RequestFeatures, digest: &DigestSignals) -> JudgmentSet {
        mr_core::heuristic::HeuristicJudge.judge(features, digest)
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
                    tracing::info!("decision backend: typesafe jev (auto-detected)");
                    Box::new(b)
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
