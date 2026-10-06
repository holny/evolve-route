use futures::stream::{Stream, StreamExt};
use mr_core::types::ResponseQuality;
use mr_memory::{EventLog, Flywheel, SessionStore};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

pub struct Telemetry {
    pub decision_id: String,
    pub session: String,
    pub chosen: String,
    pub upstream_model: String,
    pub est_tokens: u64,
    pub sticky: bool,
    pub stream: bool,
    pub started: Instant,
    pub ttft_ms: Option<u128>,
    pub bytes: u64,
    pub status: u16,
    pub usage: Option<Value>,
    pub est_cost_usd: Option<f64>,
    pub translated: Option<String>,
    pub extra: Option<Value>,
    /// 来源客户端：优先 x-mr-client，回退 User-Agent（截断），供面板展示
    pub agent: Option<String>,
}

impl Telemetry {
    pub fn event_value(&self) -> Value {
        let total_ms = self.started.elapsed().as_millis() as u64;
        let mut v = json!({
            "kind": "chat",
            "decision_id": self.decision_id,
            "session": self.session,
            "chosen": self.chosen,
            "upstream_model": self.upstream_model,
            "est_tokens": self.est_tokens,
            "sticky": self.sticky,
            "stream": self.stream,
            "status": self.status,
            "ttft_ms": self.ttft_ms.map(|t| t as u64),
            "total_ms": total_ms,
            "bytes": self.bytes,
            "usage": self.usage,
            "agent": self.agent,
        });
        if let Some(u) = &self.usage {
            let (p, c, cached, cwrite) = extract_usage_fields(u);
            v["prompt_tokens"] = json!(p);
            v["completion_tokens"] = json!(c);
            v["cached_tokens"] = json!(cached);
            v["cache_write_tokens"] = json!(cwrite);
        }
        if let Some(extra) = &self.extra
            && let (Some(obj), Some(ex)) = (v.as_object_mut(), extra.as_object()) {
                for (k, val) in ex {
                    obj.insert(k.clone(), val.clone());
                }
            }
        v
    }
}

type SharedTelem = Arc<Mutex<Telemetry>>;

/// Finalize: write event, feed flywheel, broadcast to dashboard subscribers.
pub fn finalize_event(events: &EventLog, flywheel: &Flywheel, bus: &tokio::sync::broadcast::Sender<Value>, telem: &Telemetry) {
    let event = telem.event_value();
    events.record(event.clone());
    flywheel.observe(&event);
    let _ = bus.send(event);
}

/// Wraps an upstream byte stream, capturing TTFT/bytes/usage/content/tool
/// calls while forwarding chunks untouched. At stream end (or drop) runs
/// response-quality analysis, feeds the flywheel, and records pending tool
/// calls for L3 session-loop matching.
#[allow(clippy::too_many_arguments)]
pub fn telemetry_body<E>(
    inner: impl Stream<Item = Result<bytes::Bytes, E>> + Send + 'static,
    telem: SharedTelem,
    events: EventLog,
    flywheel: Flywheel,
    sessions: SessionStore,
    bus: tokio::sync::broadcast::Sender<Value>,
    request: Value,
    anthropic_mode: bool,
) -> futures::stream::BoxStream<'static, Result<bytes::Bytes, axum::Error>>
where
    E: std::error::Error + Send + Sync + 'static,
{
    let inner = futures::TryStreamExt::map_err(inner, |e: E| axum::Error::new(e));
    let pinned: StateInner = Box::pin(inner);
    let state = (
        pinned,
        Some(Finalizer {
            telem: telem.clone(),
            events: Some(events),
            flywheel: Some(flywheel),
            sessions: Some(sessions),
            bus: Some(bus),
            request,
            acc: StreamAcc::new(anthropic_mode),
        }),
    );
    TelemetryStream { inner: Box::new(state) }.boxed()
}

#[derive(Default)]
struct StreamAcc {
    content: String,
    content_capped: bool,
    tool_calls: BTreeMap<u32, (String, String, String)>, // idx -> (id, name, args)
    finish_reason: Option<String>,
    usage: Option<Value>,
    // byte-level line buffer: decode only complete lines so multi-byte
    // UTF-8 chars split across HTTP chunks survive (BUG-6)
    byte_buf: Vec<u8>,
    /// true when accumulating upstream anthropic SSE (cross-protocol)
    anthropic_mode: bool,
    // anthropic accumulation
    ant_tool_ids: BTreeMap<u32, String>,
    ant_tool_names: BTreeMap<u32, String>,
    ant_tool_args: BTreeMap<u32, String>,
}

const CONTENT_CAP: usize = 64 * 1024;

impl StreamAcc {
    fn new(anthropic_mode: bool) -> Self {
        Self {
            anthropic_mode,
            ..Default::default()
        }
    }

    fn push_chunk(&mut self, chunk: &[u8]) {
        self.byte_buf.extend_from_slice(chunk);
        while let Some(pos) = self.byte_buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.byte_buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            self.process_line(line.trim_end());
        }
    }

    fn process_line(&mut self, line: &str) {
        if self.anthropic_mode {
            self.process_anthropic_line(line);
        } else {
            self.process_openai_line(line);
        }
    }

    fn process_anthropic_line(&mut self, line: &str) {
        let Some(data) = line.strip_prefix("data:") else { return };
        let data = data.trim();
        if data.is_empty() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { return };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => {
                if let Some(u) = v.pointer("/message/usage").filter(|u| u.is_object()) {
                    self.usage = Some(u.clone());
                }
            }
            Some("content_block_start") => {
                let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                if let Some(block) = v.get("content_block")
                    && block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        if let Some(id) = block.get("id").and_then(|i| i.as_str()) {
                            self.ant_tool_ids.insert(idx, id.to_string());
                        }
                        if let Some(n) = block.get("name").and_then(|n| n.as_str()) {
                            self.ant_tool_names.insert(idx, n.to_string());
                        }
                    }
            }
            Some("content_block_delta") => {
                let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                if let Some(delta) = v.get("delta") {
                    match delta.get("type").and_then(|t| t.as_str()) {
                        Some("text_delta") => {
                            if let Some(t) = delta.get("text").and_then(|t| t.as_str())
                                && self.content.len() < CONTENT_CAP
                            {
                                self.content.push_str(t);
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(a) = delta.get("partial_json").and_then(|a| a.as_str())
                                && self.ant_tool_args.entry(idx).or_default().len() < CONTENT_CAP
                            {
                                self.ant_tool_args.get_mut(&idx).unwrap().push_str(a);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("message_delta") => {
                if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                    let merged = match &mut self.usage {
                        Some(prev) => {
                            if let (Some(p), Some(n)) = (prev.as_object_mut(), u.as_object()) {
                                for (k, val) in n {
                                    p.insert(k.clone(), val.clone());
                                }
                            }
                            prev.clone()
                        }
                        None => u.clone(),
                    };
                    self.usage = Some(merged);
                }
                if let Some(sr) = v.pointer("/delta/stop_reason").and_then(|s| s.as_str()) {
                    self.finish_reason = Some(match sr {
                        "end_turn" => "stop".into(),
                        "max_tokens" => "length".into(),
                        "tool_use" => "tool_calls".into(),
                        other => other.to_string(),
                    });
                }
            }
            _ => {}
        }
    }

    fn process_openai_line(&mut self, line: &str) {
        let Some(data) = line.strip_prefix("data:") else { return };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { return };
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(u.clone());
        }
        let Some(choice) = v.get("choices").and_then(|c| c.as_array()).and_then(|c| c.first()) else {
            return;
        };
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str())
            && !fr.is_empty() && fr != "null" {
                self.finish_reason = Some(fr.to_string());
            }
        if let Some(delta) = choice.get("delta") {
            if let Some(t) = delta.get("content").and_then(|c| c.as_str()) {
                if self.content.len() < CONTENT_CAP {
                    self.content.push_str(t);
                } else {
                    self.content_capped = true;
                }
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                    let entry = self.tool_calls.entry(idx).or_default();
                    if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                        entry.0 = id.to_string();
                    }
                    if let Some(name) = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                    {
                        entry.1.push_str(name);
                    }
                    if let Some(args) = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                        && entry.2.len() < CONTENT_CAP {
                            entry.2.push_str(args);
                        }
                }
            }
        }
    }

    fn build_response(&self) -> Value {
        let mut source: Vec<(String, String, String)> = self
            .tool_calls
            .values()
            .cloned()
            .collect();
        for (idx, id) in &self.ant_tool_ids {
            source.push((
                id.clone(),
                self.ant_tool_names.get(idx).cloned().unwrap_or_default(),
                self.ant_tool_args.get(idx).cloned().unwrap_or_default(),
            ));
        }
        let tool_calls: Vec<Value> = source
            .iter()
            .map(|(id, name, args)| {
                json!({"id": id, "type": "function",
                       "function": {"name": name, "arguments": args}})
            })
            .collect();
        let mut message = Map::new();
        message.insert("role".into(), json!("assistant"));
        message.insert("content".into(), json!(self.content));
        if !tool_calls.is_empty() {
            message.insert("tool_calls".into(), json!(tool_calls));
        }
        json!({
            "choices": [{
                "finish_reason": self.finish_reason.clone().unwrap_or_else(|| "stop".into()),
                "message": Value::Object(message),
            }]
        })
    }
}

struct Finalizer {
    telem: SharedTelem,
    events: Option<EventLog>,
    flywheel: Option<Flywheel>,
    sessions: Option<SessionStore>,
    bus: Option<tokio::sync::broadcast::Sender<Value>>,
    request: Value,
    acc: StreamAcc,
}

const TAIL_CAP: usize = 16 * 1024;

impl Finalizer {
    fn push_chunk(&mut self, chunk: &[u8]) {
        self.acc.push_chunk(chunk);
        if self.acc.byte_buf.len() > TAIL_CAP {
            self.acc.byte_buf.clear();
        }
    }
}

impl Drop for Finalizer {
    fn drop(&mut self) {
        let Some(events) = self.events.take() else { return };
        let Ok(mut t) = self.telem.lock() else { return };
        t.usage = self.acc.usage.clone();
        if self.acc.finish_reason.is_some() || !self.acc.content.is_empty() || !self.acc.tool_calls.is_empty() {
            let pseudo = self.acc.build_response();
            let mut q: ResponseQuality = crate::quality::analyze_response(&self.request, &pseudo);
            if self.acc.content_capped {
                q.flags.push("content_capped".into());
            }
            // L3: remember pending tool calls for next-request matching
            if let Some(sessions) = &self.sessions {
                let ids: Vec<String> =
                    self.acc.tool_calls.values().map(|(id, _, _)| id.clone()).collect();
                // 与 relay 的 sticky_key 同构：agent 前缀隔离跨 agent 同名会话
                let pkey = format!("{}\u{1f}{}", t.agent.clone().unwrap_or_default(), t.session);
                sessions.set_pending(&pkey, &t.chosen, ids);
            }
            t.extra = match t.extra.take() {
                Some(mut ex) => {
                    ex["quality"] = serde_json::to_value(&q).unwrap_or_default();
                    let mut names: Vec<String> =
                        self.acc.tool_calls.values().map(|(_, n, _)| n.clone()).collect();
                    names.sort();
                    names.dedup();
                    names.truncate(5);
                    if !names.is_empty() {
                        ex["tools"] = serde_json::json!(names);
                    }
                    Some(ex)
                }
                None => Some(json!({"quality": q})),
            };
        }
        let event = t.event_value();
        events.record(event.clone());
        if let Some(fw) = &self.flywheel {
            fw.observe(&event);
        }
        if let Some(bus) = &self.bus {
            let _ = bus.send(event);
        }
    }
}

type StateInner =
    Pin<Box<dyn Stream<Item = Result<bytes::Bytes, axum::Error>> + Send>>;

struct TelemetryStream {
    inner: Box<(StateInner, Option<Finalizer>)>,
}

impl Stream for TelemetryStream {
    type Item = Result<bytes::Bytes, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self.inner;
        match this.0.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if let Some(f) = this.1.as_mut() {
                    f.push_chunk(&chunk);
                    if let Ok(mut t) = f.telem.lock() {
                        if t.ttft_ms.is_none() {
                            t.ttft_ms = Some(t.started.elapsed().as_millis());
                        }
                        t.bytes += chunk.len() as u64;
                    }
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                if let Some(f) = this.1.as_mut()
                    && let Ok(mut t) = f.telem.lock() {
                        t.status = 599;
                    }
                Poll::Ready(Some(Err(axum::Error::new(e))))
            }
            Poll::Ready(None) => {
                this.1 = None; // triggers Finalizer::drop
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub fn extract_usage_fields(usage: &Value) -> (Option<u64>, Option<u64>, Option<u64>, Option<u64>) {
    let prompt = usage.get("prompt_tokens").and_then(|v| v.as_u64());
    let completion = usage.get("completion_tokens").and_then(|v| v.as_u64());
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(|v| v.as_u64()))
        .or_else(|| usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()));
    // 缓存写：zhipu cache_write_tokens / anthropic cache_creation_input_tokens
    let cache_write = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cache_write_tokens"))
        .and_then(|v| v.as_u64())
        .or_else(|| usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()));
    (prompt, completion, cached, cache_write)
}
