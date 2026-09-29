/// Surgical rewrite of the top-level "model" field on raw request bytes.
/// All other bytes are preserved verbatim (canonical passthrough guarantee).
/// Falls back to a preserve-order re-serialization only when scanning fails.
pub fn rewrite_model_field(body: &[u8], new_model: &str) -> Vec<u8> {
    if let Some(span) = find_top_level_model_value(body) {
        let mut out = Vec::with_capacity(body.len() + new_model.len() + 2);
        out.extend_from_slice(&body[..span.start]);
        out.push(b'"');
        out.extend_from_slice(escape_json_string(new_model).as_bytes());
        out.push(b'"');
        out.extend_from_slice(&body[span.end..]);
        return out;
    }
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(mut v) => {
            if let Ok(obj) = v.as_object_mut().ok_or(()) {
                obj.insert("model".into(), serde_json::Value::String(new_model.into()));
            }
            v.to_string().into_bytes()
        }
        Err(_) => body.to_vec(),
    }
}

#[derive(Debug, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// Returns the byte span (including quotes for string values) of the
/// top-level "model" member's value, or None when absent/unscannable.
pub fn find_top_level_model_value(body: &[u8]) -> Option<Span> {
    let mut i = 0usize;
    skip_ws(body, &mut i);
    if i >= body.len() || body[i] != b'{' {
        return None;
    }
    i += 1;
    loop {
        skip_ws(body, &mut i);
        if i >= body.len() {
            return None;
        }
        match body[i] {
            b'}' => return None,
            b',' => {
                i += 1;
                continue;
            }
            b'"' => {}
            _ => return None,
        }
        let key = read_string(body, &mut i)?;
        skip_ws(body, &mut i);
        if i >= body.len() || body[i] != b':' {
            return None;
        }
        i += 1;
        skip_ws(body, &mut i);
        if i >= body.len() {
            return None;
        }
        if key == "model" {
            let start = i;
            let end = skip_value(body, i)?;
            return Some(Span { start, end });
        }
        i = skip_value(body, i)?;
        skip_ws(body, &mut i);
    }
}

fn skip_ws(body: &[u8], i: &mut usize) {
    while *i < body.len() && matches!(body[*i], b' ' | b'\t' | b'\n' | b'\r') {
        *i += 1;
    }
}

/// Reads a JSON string starting at body[i] == '"'; advances i past the
/// closing quote; returns the decoded key content (bytes are sufficient
/// for comparison, so we compare raw slices instead of decoding).
fn read_string(body: &[u8], i: &mut usize) -> Option<String> {
    if body[*i] != b'"' {
        return None;
    }
    *i += 1;
    let start = *i;
    while *i < body.len() {
        match body[*i] {
            b'\\' => *i += 2,
            b'"' => {
                let raw = &body[start..*i];
                *i += 1;
                return Some(String::from_utf8_lossy(raw).into_owned());
            }
            _ => *i += 1,
        }
    }
    None
}

/// Skips one JSON value (any type) starting at i; returns index just past it.
fn skip_value(body: &[u8], mut i: usize) -> Option<usize> {
    match body[i] {
        b'"' => {
            let _ = read_string(body, &mut i)?;
            Some(i)
        }
        b'{' | b'[' => {
            let mut depth = 0usize;
            while i < body.len() {
                match body[i] {
                    b'"' => {
                        let _ = read_string(body, &mut i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            None
        }
        _ => {
            while i < body.len() && !matches!(body[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                i += 1;
            }
            Some(i)
        }
    }
}

pub fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn model_of(bytes: &[u8]) -> String {
        serde_json::from_slice::<Value>(bytes).unwrap()["model"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn messages_value_untouched(bytes: &[u8]) -> bool {
        let v = serde_json::from_slice::<Value>(bytes).unwrap();
        v["messages"][0]["model"] == "inner-model"
    }

    #[test]
    fn rewrites_compact() {
        let src = br#"{"model":"auto","messages":[{"role":"user","content":"hi"}],"stream":false}"#;
        let out = rewrite_model_field(src, "mock-mini");
        assert_eq!(model_of(&out), "mock-mini");
        assert_eq!(out.len(), src.len() - 4 + 9);
    }

    #[test]
    fn rewrites_pretty_with_newlines() {
        let src = b"{\n  \"model\": \"auto\",\n  \"messages\": [\n    {\"role\": \"user\", \"content\": \"hi\"}\n  ]\n}";
        let out = rewrite_model_field(src, "mock-standard");
        assert_eq!(model_of(&out), "mock-standard");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\n  \"messages\""));
    }

    #[test]
    fn model_at_end_before_closing_brace() {
        let src = br#"{"messages":[{"role":"user","content":"x"}],"model":"auto"}"#;
        let out = rewrite_model_field(src, "m2");
        assert_eq!(model_of(&out), "m2");
    }

    #[test]
    fn nested_model_key_untouched() {
        let src = br#"{"messages":[{"role":"user","content":"x","model":"inner-model"}],"model":"auto"}"#;
        let out = rewrite_model_field(src, "top");
        assert_eq!(model_of(&out), "top");
        assert!(messages_value_untouched(&out));
    }

    #[test]
    fn unicode_and_escapes_in_other_values_survive() {
        let src = r#"{"model":"auto","messages":[{"role":"user","content":"你好 \"quoted\" \n done"}]}"#;
        let out = rewrite_model_field(src.as_bytes(), "модель-3");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["messages"][0]["content"], "你好 \"quoted\" \n done");
        assert_eq!(v["model"], "модель-3");
    }

    #[test]
    fn missing_model_falls_back_to_insert() {
        let src = br#"{"messages":[{"role":"user","content":"x"}]}"#;
        let out = rewrite_model_field(src, "fallback");
        assert_eq!(model_of(&out), "fallback");
    }

    #[test]
    fn null_model_value_replaced() {
        let src = br#"{"model":null,"messages":[]}"#;
        let out = rewrite_model_field(src, "m");
        assert_eq!(model_of(&out), "m");
    }

    #[test]
    fn byte_preservation_outside_model() {
        let src = br#"{"a":1,   "model" :   "auto" ,"b":[1,2,3]}"#;
        let out = rewrite_model_field(src, "x");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"a\":1,   \"model\" :   \"x\" ,\"b\":[1,2,3]"), "got: {text}");
    }
}
