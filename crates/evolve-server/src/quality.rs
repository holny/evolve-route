//! Gateway-side response quality analysis (flywheel L2 signals).
//! The gateway owns the full request (tool schemas included) and the full
//! response, so it can judge tool-call syntax, hallucinated tool names,
//! schema conformance, truncation and degenerate output without any
//! plugin-side help.

use evolve_core::types::ResponseQuality;
use serde_json::Value;

pub fn analyze_response(request: &Value, response: &Value) -> ResponseQuality {
    let mut q = ResponseQuality::default();
    let Some(choices) = response.get("choices").and_then(|c| c.as_array()) else {
        q.flags.push("no_choices".into());
        return q;
    };
    let choice = choices.first().cloned().unwrap_or(Value::Null);
    q.finish_reason = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .map(|s| s.to_string());
    q.truncated = q.finish_reason.as_deref() == Some("length");

    let message = choice.get("message").cloned().unwrap_or(Value::Null);
    let content = message.get("content").and_then(|c| c.as_str()).unwrap_or("");
    q.content_chars = content.chars().count();

    // tool schema registry from the request
    let tools = request.get("tools").and_then(|t| t.as_array()).cloned().unwrap_or_default();

    if let Some(tcs) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tcs {
            q.tool_calls_total += 1;
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let args_raw = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("");
            let parsed: Option<Value> = serde_json::from_str(args_raw).ok();
            if parsed.is_some() {
                q.tool_calls_valid_json += 1;
            }
            let schema: Option<Option<Value>> = tools.iter().find_map(|t| {
                let fname = t.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str())?;
                (fname == name).then(|| t.get("function").and_then(|f| f.get("parameters")).cloned())
            });
            if schema.clone().flatten().is_some() {
                q.tool_calls_known_name += 1;
            } else if !tools.is_empty() {
                // hallucinated tool name
                continue;
            }
            if let (Some(args), Some(Some(schema))) = (&parsed, &schema)
                && schema_conforms(args, schema) {
                    q.tool_calls_schema_ok += 1;
                }
        }
    } else if content.trim().is_empty() && q.finish_reason.as_deref() == Some("stop") {
        q.empty_response = true;
        q.flags.push("empty_stop".into());
    }

    if q.content_chars > 0 {
        if looks_degenerate(content) {
            q.degenerate = true;
            q.flags.push("degenerate".into());
        }
        if looks_refusal(content) {
            q.flags.push("refusal".into());
        }
    }
    q
}

/// Basic JSON-schema conformance: required fields present + top-level types.
fn schema_conforms(args: &Value, schema: &Value) -> bool {
    let Some(required) = schema.get("required").and_then(|r| r.as_array()) else {
        return true;
    };
    let Some(obj) = args.as_object() else { return false };
    for req in required {
        let Some(key) = req.as_str() else { continue };
        if !obj.contains_key(key) {
            return false;
        }
    }
    if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
        let type_ok = |v: &Value, ty: &str| match ty {
            "string" => v.is_string(),
            "number" | "integer" => v.is_number(),
            "boolean" => v.is_boolean(),
            "array" => v.is_array(),
            "object" => v.is_object(),
            _ => true,
        };
        for (k, v) in obj {
            if let Some(p) = props.get(k)
                && let Some(ty) = p.get("type").and_then(|t| t.as_str())
                    && !type_ok(v, ty) {
                        return false;
                    }
        }
    }
    true
}

const REFUSAL_MARKERS: &[&str] = &[
    "I can't help", "I cannot help", "I'm unable to", "无法协助", "无法帮助", "我无法",
    "against my", "违反",
];

fn looks_refusal(content: &str) -> bool {
    let lower = content.to_lowercase();
    content.chars().count() < 600 && REFUSAL_MARKERS.iter().any(|m| lower.contains(&m.to_lowercase()))
}

fn looks_degenerate(content: &str) -> bool {
    let chars: Vec<char> = content.chars().collect();
    if chars.len() < 200 {
        return false;
    }
    // repetition loop: a 40-char window repeating 4+ times consecutively
    let win = 40;
    if chars.len() > win * 4 {
        for start in 0..(chars.len() - win * 4).min(4000) {
            let seg: String = chars[start..start + win].iter().collect();
            if seg.chars().all(|c| c.is_whitespace()) {
                continue;
            }
            let repeats = (1..=4)
                .take_while(|n| {
                    let s = start + win * n;
                    s + win <= chars.len() && chars[s..s + win].iter().collect::<String>() == seg
                })
                .count();
            if repeats >= 3 {
                return true;
            }
        }
    }
    // replacement-char garbage
    let bad = chars.iter().filter(|&&c| c == '\u{FFFD}').count();
    bad * 20 > chars.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req_with_tools() -> Value {
        json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "read_file",
                    "parameters": {
                        "type": "object",
                        "required": ["path"],
                        "properties": {"path": {"type": "string"}, "line": {"type": "integer"}}
                    }
                }
            }]
        })
    }

    #[test]
    fn valid_tool_call_scores_full() {
        let resp = json!({
            "choices": [{"finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{"id": "c1", "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\",\"line\":3}"}}]
            }}]
        });
        let q = analyze_response(&req_with_tools(), &resp);
        assert_eq!(q.tool_calls_total, 1);
        assert_eq!(q.tool_calls_valid_json, 1);
        assert_eq!(q.tool_calls_known_name, 1);
        assert_eq!(q.tool_calls_schema_ok, 1);
        assert!(q.syntactic_ok());
    }

    #[test]
    fn hallucinated_tool_name_detected() {
        let resp = json!({
            "choices": [{"finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{"id": "c1", "type": "function",
                    "function": {"name": "delete_everything", "arguments": "{}"}}]
            }}]
        });
        let q = analyze_response(&req_with_tools(), &resp);
        assert_eq!(q.tool_calls_total, 1);
        assert_eq!(q.tool_calls_known_name, 0);
        assert!(!q.syntactic_ok());
    }

    #[test]
    fn broken_json_arguments_detected() {
        let broken = r#"{"path": }}"#;
        let resp = json!({
            "choices": [{"finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{"id": "c1", "type": "function",
                    "function": {"name": "read_file", "arguments": broken}}]
            }}]
        });
        let q = analyze_response(&req_with_tools(), &resp);
        assert_eq!(q.tool_calls_valid_json, 0);
        assert!(!q.syntactic_ok());
    }

    #[test]
    fn missing_required_field_fails_schema() {
        let resp = json!({
            "choices": [{"finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{"id": "c1", "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"line\": 3}"}}]
            }}]
        });
        let q = analyze_response(&req_with_tools(), &resp);
        assert_eq!(q.tool_calls_valid_json, 1);
        assert_eq!(q.tool_calls_schema_ok, 0);
    }

    #[test]
    fn empty_stop_flagged() {
        let resp = json!({"choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": ""}}]});
        let q = analyze_response(&req_with_tools(), &resp);
        assert!(q.empty_response);
    }

    #[test]
    fn length_truncation_flagged() {
        let resp = json!({"choices": [{"finish_reason": "length", "message": {"role": "assistant", "content": "partial..."}}]});
        let q = analyze_response(&req_with_tools(), &resp);
        assert!(q.truncated);
    }
}
