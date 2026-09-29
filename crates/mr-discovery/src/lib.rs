//! Agent configuration discovery.
//!
//! Ground rule: connection facts (baseUrl / providerId / modelId / credentials)
//! always come from user configuration. External catalogs (models.dev) are
//! reference-only metadata and never override user values.

pub mod codex;
pub mod opencode;

pub const SUPPORTED_AGENTS: &[&str] =
    &["opencode", "codex", "claude-code", "pi", "dsh", "openclaw", "hermes"];

/// Discover models from the configured agent sources.
pub fn discover(agents: &[String]) -> Vec<mr_core::types::ModelRecord> {
    let mut out = Vec::new();
    for a in agents {
        match a.as_str() {
            "opencode" => out.extend(opencode::discover_default()),
            "codex" => out.extend(codex::discover_default()),
            other => tracing::debug!(agent = other, "discovery not implemented yet"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn supported_list_matches_plan() {
        assert_eq!(super::SUPPORTED_AGENTS.len(), 7);
    }
}
