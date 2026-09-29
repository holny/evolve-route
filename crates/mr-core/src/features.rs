use crate::tokens;
use crate::types::RequestFeatures;
use serde_json::Value;

pub struct Extracted {
    pub messages: Vec<Value>,
    pub tools_json: Option<String>,
    pub tool_count: usize,
    pub last_user_text: String,
    pub first_user_text: String,
    pub system_text: String,
}

pub fn extract(body: &Value) -> Extracted {
    let messages: Vec<Value> = body
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let tools_json = body.get("tools").map(|t| t.to_string());
    let tool_count = body.get("tools").and_then(|t| t.as_array()).map(|a| a.len()).unwrap_or(0);

    let mut last_user_text = String::new();
    let mut first_user_text = String::new();
    let mut system_text = String::new();
    for m in &messages {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "system" => {
                if let Some(t) = tokens::content_text(m) {
                    system_text.push_str(&t);
                }
            }
            "user" => {
                if let Some(t) = tokens::content_text(m) {
                    if first_user_text.is_empty() {
                        first_user_text = t.clone();
                    }
                    last_user_text = t;
                }
            }
            _ => {}
        }
    }

    Extracted { messages, tools_json, tool_count, last_user_text, first_user_text, system_text }
}

pub fn features(extracted: &Extracted, est_input_tokens: u64) -> RequestFeatures {
    let sample: String = extracted
        .last_user_text
        .chars()
        .chain(extracted.system_text.chars().take(2000))
        .collect();
    let total = sample.chars().count().max(1) as f32;
    let mut code = 0f32;
    let mut cjk = 0f32;
    for ch in sample.chars() {
        if tokens::is_code_char_pub(ch) {
            code += 1.0;
        }
        if tokens::is_cjk_pub(ch) {
            cjk += 1.0;
        }
    }
    let mut has_images = false;
    for m in &extracted.messages {
        if let Some(Value::Array(parts)) = m.get("content") {
            for p in parts {
                if p.get("type").and_then(|t| t.as_str()) == Some("image_url")
                    || p.get("type").and_then(|t| t.as_str()) == Some("image")
                {
                    has_images = true;
                }
            }
        }
    }
    let user_turns = extracted
        .messages
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .count();

    RequestFeatures {
        est_input_tokens,
        user_text_chars: extracted.last_user_text.chars().count(),
        code_density: (code / total).min(1.0),
        tool_count: extracted.tool_count,
        tool_ratio: (extracted.tool_count as f32 / 20.0).min(1.0),
        has_images,
        turn_count: user_turns,
        cjk_ratio: (cjk / total).min(1.0),
    }
}
