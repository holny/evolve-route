//! Per-request client identity: which agent is calling and which session it
//! belongs to. Extraction order is configurable (custom header first), then
//! builtin logic derived from each agent's actual wire behavior:
//! - codex: `originator` header (codex_cli_rs…) + `session_id` header (uuid)
//! - claude code: UA `claude-cli/x.y.z` + body `metadata.user_id` tail after `_session_`
//! - opencode: UA `opencode/x.y.z` + our adapter stamps `x-tr-session`
//! - pi: UA `pi/…` (no stable session header → hash fallback)
use axum::http::HeaderMap;
use serde_json::Value;

fn header_of(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
}

fn trunc(s: String, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Canonical agent name from known User-Agent shapes, else raw UA truncated.
pub fn agent_from_ua(ua: &str) -> String {
    let l = ua.to_lowercase();
    if l.starts_with("claude-cli") || l.contains("claude-code") {
        "claude-code".into()
    } else if l.contains("codex") {
        "codex".into()
    } else if l.contains("opencode") {
        "opencode".into()
    } else if l.starts_with("pi/") || l.contains("earendil") {
        "pi".into()
    } else {
        trunc(ua.to_string(), 40)
    }
}

pub fn agent_identity(headers: &HeaderMap, agent_header: &str) -> String {
    if let Some(v) = header_of(headers, agent_header).filter(|v| !v.is_empty()) {
        return trunc(v, 40);
    }
    if let Some(o) = header_of(headers, "originator") {
        let l = o.to_lowercase();
        if l.contains("codex") {
            return "codex".into();
        }
        return trunc(o, 40);
    }
    match header_of(headers, "user-agent") {
        Some(ua) if !ua.is_empty() => agent_from_ua(&ua),
        _ => "-".into(),
    }
}

/// Session id from custom/known headers, then claude code's body metadata.
pub fn session_identity(
    headers: &HeaderMap,
    parsed: &Value,
    session_header: &str,
) -> Option<String> {
    if let Some(v) = header_of(headers, session_header).filter(|v| !v.is_empty()) {
        return Some(v);
    }
    if let Some(v) = header_of(headers, "session_id").filter(|v| !v.is_empty()) {
        return Some(v);
    }
    // opencode 原生会话头（opencode 官方要求客户端发送）——无需 drop-in 插件
    if let Some(v) = header_of(headers, "x-opencode-session").filter(|v| !v.is_empty()) {
        return Some(v);
    }
    if let Some(v) = header_of(headers, "x-session-id")
        .or_else(|| header_of(headers, "x-session"))
        .filter(|v| !v.is_empty())
    {
        return Some(v);
    }
    // claude code: metadata.user_id = "user_<hash>_account_<uuid>_session_<uuid>"
    if let Some(uid) = parsed
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|v| v.as_str())
        && let Some(pos) = uid.find("_session_")
    {
        let tail = &uid[pos + "_session_".len()..];
        if !tail.is_empty() {
            return Some(tail.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn h(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        m
    }

    #[test]
    fn codex_via_originator_and_session_header() {
        let headers = h(&[("originator", "codex_cli_rs"), ("session_id", "abc-123")]);
        assert_eq!(agent_identity(&headers, "x-tr-client"), "codex");
        assert_eq!(
            session_identity(&headers, &json!({}), "x-tr-session"),
            Some("abc-123".into())
        );
    }

    #[test]
    fn claude_code_via_ua_and_body_metadata() {
        let headers = h(&[("user-agent", "claude-cli/2.1.0 (external, cli)")]);
        assert_eq!(agent_identity(&headers, "x-tr-client"), "claude-code");
        let body = json!({"metadata": {"user_id": "user_abc123_account_def-uuid_session_9f8e7d6c"}});
        assert_eq!(
            session_identity(&headers, &body, "x-tr-session"),
            Some("9f8e7d6c".into())
        );
    }

    #[test]
    fn opencode_ua_and_custom_session_header() {
        let headers = h(&[("user-agent", "opencode/1.2.3"), ("x-tr-session", "ses_x")]);
        assert_eq!(agent_identity(&headers, "x-tr-client"), "opencode");
        assert_eq!(
            session_identity(&headers, &json!({}), "x-tr-session"),
            Some("ses_x".into())
        );
    }

    #[test]
    fn custom_header_overrides_builtin() {
        let headers = h(&[("x-my-sess", "custom-1"), ("session_id", "codex-2")]);
        assert_eq!(
            session_identity(&headers, &json!({}), "x-my-sess"),
            Some("custom-1".into())
        );
        assert_eq!(
            session_identity(&headers, &json!({}), "x-tr-session"),
            Some("codex-2".into())
        );
    }

    #[test]
    fn unknown_ua_passes_through() {
        let headers = h(&[("user-agent", "curl/8.4.0")]);
        assert_eq!(agent_identity(&headers, "x-tr-client"), "curl/8.4.0");
        assert_eq!(session_identity(&headers, &json!({}), "x-tr-session"), None);
    }
}
