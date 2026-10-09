//! Model alias matching across benchmark boards and catalog entries
//! (decision record #24). Suffix IS identity (flash/pro/mini/thinking are
//! different models); version tokens must co-exist; case/separators are
//! irrelevant. Matching is confidence-tiered:
//!   L1 exact normalized key      -> 1.0  (apply as-is)
//!   L2 token-set equality        -> 0.85 (blend 50/50 with current tiers)
//!   L3 umbrella aggregation      -> 0.5  (family mean for latest/auto)
//! Explicit user tiers always win: benchmark blend is capped at 0.3 for
//! entries that declare their own tiers.

pub const CONF_EXACT: f32 = 1.0;
pub const CONF_TOKEN_SET: f32 = 0.85;
pub const CONF_LATEST: f32 = 0.7;
pub const CONF_UMBRELLA: f32 = 0.5;
pub const CONF_EXPLICIT_TIERS: f32 = 0.3;
pub const CONF_EXPLICIT_TIERS_RATIO: f32 = 0.3;

/// Extract the primary version number from a model alias: "glm-5.3" -> 5.3,
/// "kimi-k3" -> 3.0, "gpt-4o" -> 4.0. Used to pick the newest family member
/// for "latest"-style dynamic aliases.
pub fn version_num(name: &str) -> Option<f64> {
    let key = match_key(name);
    let mut best: Option<f64> = None;
    let mut num = String::new();
    let chars: Vec<char> = key.chars().chain(std::iter::once(' ')).collect();
    for c in chars {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else if !num.is_empty() {
            if let Ok(v) = num.parse::<f64>()
                && best.map(|b| v > b).unwrap_or(true) {
                    best = Some(v);
                }
            num.clear();
        }
    }
    best
}

/// Dynamic aliases: the provider routes internally, the exact model is
/// unknown. Checked on the RAW name (before noise stripping, which would
/// eat "-latest"): last segment token ∈ {auto, latest, default}.
pub fn is_dynamic(name: &str) -> bool {
    let raw = match name.rfind('/') {
        Some(pos) => &name[pos + 1..],
        None => name,
    };
    let lower = raw.to_lowercase();
    matches!(
        lower.rsplit(['-', '_', '.', ' ']).next().unwrap_or(lower.as_str()),
        "auto" | "latest" | "default"
    )
}

/// L1 key: lowercase alphanumerics only, provider prefix and known noise
/// suffixes removed. "GLM-5.2" / "glm-5.2" / "glm5.2" all -> "glm52".
pub fn match_key(name: &str) -> String {
    let mut s = name.to_lowercase();
    if let Some(pos) = s.find('/') {
        s = s[pos + 1..].to_string();
    }
    for pat in [
        "-20241120", "-20241119", "-20241217", "-20250101", "-20250514",
        "-2024-11-20", "-2024-12-17", "-2025-01-01", "-2025-05-14",
        "-20240229", "-20240306", "-20240409", "-20240620", "-20240827",
        "-20240919", "-20241022",
    ] {
        s = s.replace(pat, "");
    }
    for pat in ["-preview", "-latest", "-instruct", "-it", "-turbo"] {
        s = s.replace(pat, "");
    }
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

/// Token view for L2/L3: lowercase segments split on separators.
pub fn tokens(name: &str) -> Vec<String> {
    match_key_full(name)
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

fn match_key_full(name: &str) -> String {
    let mut s = name.to_lowercase();
    if let Some(pos) = s.find('/') {
        s = s[pos + 1..].to_string();
    }
    for pat in ["-preview", "-latest", "-instruct", "-it", "-turbo"] {
        s = s.replace(pat, "");
    }
    s
}

/// Does a versioned model name carry real version tokens (digits)?
/// "glm" alone is an umbrella; "glm-5.3" is versioned.
fn is_umbrella(name: &str) -> bool {
    tokens(name).iter().all(|t| !t.chars().any(|c| c.is_ascii_digit()))
}

/// Confidence-tiered match between a benchmark board name and a catalog
/// model name. Returns None when they must be treated as different models.
pub fn match_confidence(board_name: &str, catalog_name: &str) -> Option<f32> {
    // L1: exact normalized key
    let (ka, kb) = (match_key(board_name), match_key(catalog_name));
    if !ka.is_empty() && ka == kb {
        return Some(CONF_EXACT);
    }
    // dynamic aliases are handled by the caller via umbrella aggregation
    if is_dynamic(catalog_name) || is_dynamic(board_name) {
        return None;
    }
    // a bare family name must not absorb versioned models
    if is_umbrella(board_name) || is_umbrella(catalog_name) {
        return None;
    }
    // L2: token-set equality (separator/format variations)
    let (ta, tb) = (tokens(board_name), tokens(catalog_name));
    if ta.len() == tb.len() {
        let (mut sa, mut sb) = (ta.clone(), tb.clone());
        sa.sort();
        sb.sort();
        if sa == sb {
            return Some(CONF_TOKEN_SET);
        }
    }
    None
}

/// Umbrella aggregation: family key for "latest"-style aliases. "glm-latest"
/// -> family "glm"; used to average all versioned board entries in family.
pub fn family_key(name: &str) -> Option<String> {
    let ts = tokens(name);
    if is_dynamic(name) || ts.is_empty() {
        return ts.first().cloned();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_model_different_spellings_match_exactly() {
        assert_eq!(match_confidence("GLM-5.2", "glm-5.2"), Some(CONF_EXACT));
        assert_eq!(match_confidence("glm5.2", "glm-5.2"), Some(CONF_EXACT));
        assert_eq!(match_confidence("openai/gpt-4o-2024-11-20", "gpt-4o"), Some(CONF_EXACT));
    }

    #[test]
    fn suffix_is_identity() {
        // flash vs pro are different models
        assert_eq!(match_confidence("deepseek-v4-flash", "deepseek-v4-pro"), None);
        assert_eq!(match_confidence("glm-5.2", "glm-5.3"), None);
        // mini/tiny/base variants differ too
        assert_eq!(match_confidence("qwen3-mini", "qwen3-base"), None);
    }

    #[test]
    fn umbrella_never_absorbs_versioned() {
        assert_eq!(match_confidence("glm-5.3", "glm"), None);
        assert_eq!(match_confidence("gpt-4o", "gpt"), None);
    }

    #[test]
    fn dynamic_detected() {
        assert!(is_dynamic("auto"));
        assert!(is_dynamic("glm-latest"));
        assert!(!is_dynamic("glm-5.3"));
        assert_eq!(family_key("glm-latest"), Some("glm".into()));
    }

    #[test]
    fn token_set_matches_spacing_variants() {
        // separators removed in key -> same; token set path covered too
        assert_eq!(match_confidence("qwen 3 max", "qwen3-max"), Some(CONF_EXACT));
    }
}
