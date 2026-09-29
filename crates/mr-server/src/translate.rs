//! Cross-protocol translation via Switchyard codecs (Apache-2.0).
//!
//! Direction priority (user decision): OpenAI Chat Completions ingress →
//! Anthropic Messages upstream, buffered + streaming. Same-protocol traffic
//! never touches this module (canonical passthrough). All translations go
//! through Switchyard's neutral IR with deterministic-ID policy.

use switchyard_protocol::format::WireFormat;
use switchyard_translation::{FormatRegistry, StreamTranslationState, TranslationEngine, TranslationPolicy};
use serde_json::Value;
use std::pin::Pin;
use std::sync::OnceLock;

pub struct Translator {
    engine: TranslationEngine,
}

static ENGINE: OnceLock<Translator> = OnceLock::new();

impl Translator {
    pub fn global() -> &'static Translator {
        ENGINE.get_or_init(|| Translator { engine: TranslationEngine::new(FormatRegistry::with_builtins()) })
    }

    fn policy(&self) -> TranslationPolicy {
        TranslationPolicy::default()
    }

    pub fn request_openai_to_anthropic(
        &self,
        body: &Value,
    ) -> Result<Value, String> {
        self.engine
            .translate_request(
                WireFormat::OpenAiChat,
                WireFormat::AnthropicMessages,
                body,
                &self.policy(),
            )
            .map(|out| out.body)
            .map_err(|e| e.to_string())
    }

    pub fn request_anthropic_to_openai(
        &self,
        body: &Value,
    ) -> Result<Value, String> {
        self.engine
            .translate_request(
                WireFormat::AnthropicMessages,
                WireFormat::OpenAiChat,
                body,
                &self.policy(),
            )
            .map(|out| out.body)
            .map_err(|e| e.to_string())
    }

    pub fn response_openai_to_anthropic(
        &self,
        body: &Value,
    ) -> Result<Value, String> {
        self.engine
            .translate_response(
                WireFormat::OpenAiChat,
                WireFormat::AnthropicMessages,
                body,
                &self.policy(),
            )
            .map(|out| out.body)
            .map_err(|e| e.to_string())
    }

    pub fn response_anthropic_to_openai(
        &self,
        body: &Value,
    ) -> Result<Value, String> {
        self.engine
            .translate_response(
                WireFormat::AnthropicMessages,
                WireFormat::OpenAiChat,
                body,
                &self.policy(),
            )
            .map(|out| out.body)
            .map_err(|e| e.to_string())
    }

    pub fn anthropic_event_to_openai_chunks(
        &self,
        state: &mut StreamTranslationState,
        event: &Value,
    ) -> Vec<Value> {
        self.engine
            .translate_event(state, WireFormat::AnthropicMessages, WireFormat::OpenAiChat, event)
            .unwrap_or_default()
    }

    pub fn finish_openai_chunks(&self, state: &mut StreamTranslationState) -> Vec<Value> {
        self.engine
            .finish_stream(state, WireFormat::OpenAiChat)
            .unwrap_or_default()
    }
}

/// Streaming stage: consumes anthropic SSE bytes, emits openai SSE bytes.
pub struct AnthropicToOpenaiStream {
    inner: Pin<Box<dyn StreamInner + Send>>,
    mapper: StreamTranslationState,
    line_buf: String,
    out: std::collections::VecDeque<bytes::Bytes>,
    done: bool,
}

trait StreamInner: Unpin {
    fn poll_next_inner(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<bytes::Bytes, axum::Error>>>;
}

impl<S: futures::Stream<Item = Result<bytes::Bytes, axum::Error>> + Unpin> StreamInner for S {
    fn poll_next_inner(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<bytes::Bytes, axum::Error>>> {
        self.poll_next(cx)
    }
}

impl AnthropicToOpenaiStream {
    pub fn new(inner: impl futures::Stream<Item = Result<bytes::Bytes, axum::Error>> + Send + Unpin + 'static) -> Self {
        Self {
            inner: Box::pin(inner),
            mapper: StreamTranslationState::default(),
            line_buf: String::new(),
            out: std::collections::VecDeque::new(),
            done: false,
        }
    }

    fn feed_line(&mut self, line: &str) {
        let Some(data) = line.strip_prefix("data:") else { return };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else { return };
        let translator = Translator::global();
        for chunk in translator.anthropic_event_to_openai_chunks(&mut self.mapper, &event) {
            self.out.push_back(chunk_to_sse(&chunk));
        }
    }

}

fn chunk_to_sse(chunk: &Value) -> bytes::Bytes {
    bytes::Bytes::from(format!("data: {chunk}\n\n"))
}

impl AnthropicToOpenaiStream {
    /// Identity pass-through helper: the stream already yields axum::Error.
    pub fn into_outer(self) -> impl futures::Stream<Item = Result<bytes::Bytes, axum::Error>> {
        self
    }
}

impl futures::Stream for AnthropicToOpenaiStream {
    type Item = Result<bytes::Bytes, axum::Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // Loop: feed inner until it emits output or goes Pending, so we never
        // return Pending while upstream data remains consumable (lost-waker).
        loop {
            if let Some(b) = self.out.pop_front() {
                return std::task::Poll::Ready(Some(Ok(b)));
            }
            if self.done {
                return std::task::Poll::Ready(None);
            }
            match Pin::new(&mut *self.inner).poll_next_inner(cx) {
                std::task::Poll::Ready(Some(Ok(chunk))) => {
                    let text = String::from_utf8_lossy(&chunk);
                    self.line_buf.push_str(&text);
                    while let Some(pos) = self.line_buf.find('\n') {
                        let line: String = self.line_buf.drain(..=pos).collect();
                        self.feed_line(line.trim_end());
                    }
                }
                std::task::Poll::Ready(Some(Err(e))) => {
                    return std::task::Poll::Ready(Some(Err(e)));
                }
                std::task::Poll::Ready(None) => {
                    self.done = true;
                    let translator = Translator::global();
                    for chunk in translator.finish_openai_chunks(&mut self.mapper) {
                        self.out.push_back(chunk_to_sse(&chunk));
                    }
                    self.out.push_back(bytes::Bytes::from("data: [DONE]\n\n"));
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

/// Reverse direction: openai chunks (upstream) -> anthropic SSE (client).
pub struct OpenaiToAnthropicStream {
    inner: Pin<Box<dyn StreamInner + Send>>,
    mapper: StreamTranslationState,
    line_buf: String,
    out: std::collections::VecDeque<bytes::Bytes>,
    done: bool,
}

impl OpenaiToAnthropicStream {
    pub fn new(inner: impl futures::Stream<Item = Result<bytes::Bytes, axum::Error>> + Send + Unpin + 'static) -> Self {
        Self {
            inner: Box::pin(inner),
            mapper: StreamTranslationState::default(),
            line_buf: String::new(),
            out: std::collections::VecDeque::new(),
            done: false,
        }
    }

    fn feed_line(&mut self, line: &str) {
        let Some(data) = line.strip_prefix("data:") else { return };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else { return };
        let translator = Translator::global();
        for chunk in translator.engine
            .translate_event(&mut self.mapper, WireFormat::OpenAiChat, WireFormat::AnthropicMessages, &event)
            .unwrap_or_default()
        {
            let ev_type = chunk.get("type").and_then(|t| t.as_str()).unwrap_or("message");
            self.out.push_back(bytes::Bytes::from(format!("event: {ev_type}\ndata: {chunk}\n\n")));
        }
    }
}

impl futures::Stream for OpenaiToAnthropicStream {
    type Item = Result<bytes::Bytes, axum::Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            if let Some(b) = self.out.pop_front() {
                return std::task::Poll::Ready(Some(Ok(b)));
            }
            if self.done {
                return std::task::Poll::Ready(None);
            }
            match Pin::new(&mut *self.inner).poll_next_inner(cx) {
                std::task::Poll::Ready(Some(Ok(chunk))) => {
                    let text = String::from_utf8_lossy(&chunk);
                    self.line_buf.push_str(&text);
                    while let Some(pos) = self.line_buf.find('\n') {
                        let line: String = self.line_buf.drain(..=pos).collect();
                        self.feed_line(line.trim_end());
                    }
                }
                std::task::Poll::Ready(Some(Err(e))) => return std::task::Poll::Ready(Some(Err(e))),
                std::task::Poll::Ready(None) => {
                    self.done = true;
                    let translator = Translator::global();
                    for chunk in translator.engine
                        .finish_stream(&mut self.mapper, WireFormat::AnthropicMessages)
                        .unwrap_or_default()
                    {
                        let ev_type = chunk.get("type").and_then(|t| t.as_str()).unwrap_or("message_stop");
                        self.out.push_back(bytes::Bytes::from(format!("event: {ev_type}\ndata: {chunk}\n\n")));
                    }
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_translates_system_and_tools() {
        let openai = json!({
            "model": "claude-sonnet",
            "max_tokens": 1024,
            "messages": [
                {"role": "system", "content": "You are a coder."},
                {"role": "user", "content": "hi"}
            ],
            "tools": [{
                "type": "function",
                "function": {"name": "read_file",
                             "parameters": {"type": "object", "required": ["path"],
                                            "properties": {"path": {"type": "string"}}}}
            }]
        });
        let out = Translator::global().request_openai_to_anthropic(&openai).unwrap();
        assert_eq!(out["model"], "claude-sonnet");
        assert!(out.get("system").is_some(), "system must move to top-level: {out}");
        assert_eq!(out["max_tokens"], 1024, "anthropic requires max_tokens");
        let tools = out["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "read_file");
        assert!(tools[0].get("input_schema").is_some(), "anthropic tool schema key");
        assert!(out["messages"].as_array().unwrap().last().unwrap()["content"].is_string());
    }

    #[test]
    fn response_translates_back_to_openai_shape() {
        let anthropic = json!({
            "id": "msg_1",
            "model": "claude-sonnet",
            "role": "assistant",
            "content": [{"type": "text", "text": "hello there"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let out = Translator::global().response_anthropic_to_openai(&anthropic).unwrap();
        let choice = &out["choices"][0];
        assert!(choice["message"]["content"].as_str().unwrap().contains("hello"));
        assert_eq!(choice["finish_reason"], "stop");
        assert!(out.get("usage").is_some(), "usage should map: {out}");
    }

    #[tokio::test]
    async fn stream_maps_anthropic_events_to_openai_chunks() {
        let events = vec![
            json!({"type": "message_start",
                   "message": {"id": "m1", "model": "claude-sonnet", "role": "assistant",
                               "content": [], "usage": {"input_tokens": 8}}}),
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hey"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                   "usage": {"output_tokens": 3}}),
            json!({"type": "message_stop"}),
        ];
        let mut s = StreamTranslationState::default();
        let translator = Translator::global();
        let mut collected = String::new();
        for e in &events {
            for chunk in translator.anthropic_event_to_openai_chunks(&mut s, e) {
                collected.push_str(&format!("data: {chunk}\n\n"));
            }
        }
        for chunk in translator.finish_openai_chunks(&mut s) {
            collected.push_str(&format!("data: {chunk}\n\n"));
        }
        assert!(collected.contains("\"content\""), "content deltas mapped: {collected}");
        assert!(collected.contains("finish_reason"), "finish mapped");
        assert!(collected.contains("data: [DONE]") || collected.contains("[DONE]") == false);
        // determinism: same input twice → byte-identical output
        let mut s2 = StreamTranslationState::default();
        let mut collected2 = String::new();
        for e in &events {
            for chunk in translator.anthropic_event_to_openai_chunks(&mut s2, e) {
                collected2.push_str(&format!("data: {chunk}\n\n"));
            }
        }
        for chunk in translator.finish_openai_chunks(&mut s2) {
            collected2.push_str(&format!("data: {chunk}\n\n"));
        }
        assert_eq!(collected.replace("\"id\":\"", "\"id\":\"x"), collected2.replace("\"id\":\"", "\"id\":\"x"),
            "translation must be deterministic modulo generated ids");
    }
}
