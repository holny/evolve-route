use crate::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::stream::Stream;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;
use futures::StreamExt;

pub const DASHBOARD_HTML: &str = include_str!("dashboard.html");

pub async fn dashboard() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        DASHBOARD_HTML,
    )
}

pub async fn list_models(State(st): State<AppState>) -> Response {
    let mut data = vec![
        json!({"id": "auto", "object": "model", "owned_by": "modelroute", "source": "router",
               "description": "intelligent routing (default policy)"}),
        json!({"id": "auto:cost", "object": "model", "owned_by": "modelroute", "source": "router",
               "description": "intelligent routing, cost-optimized policy"}),
        json!({"id": "auto:quality", "object": "model", "owned_by": "modelroute", "source": "router",
               "description": "intelligent routing, quality-optimized policy"}),
    ];
    for m in &st.engine.catalog.models {
        data.push(json!({
            "id": m.id,
            "object": "model",
            "owned_by": m.provider,
            "source": m.source.label(),
            "context_window": m.context_window,
            "cost_per_mtok": m.cost.as_ref().map(|c| json!({"input": c.input, "output": c.output})),
            "tiers": m.tiers,
            "weight": m.weight,
            "note": m.source_note,
        }));
    }
    (axum::Json(json!({"object": "list", "data": data}))).into_response()
}

pub async fn healthz() -> &'static str {
    "ok"
}

/// Live health status of every model seen by the router.
pub async fn api_health(State(st): State<AppState>) -> Response {
    let snap = st.health.snapshot();
    let mut models = serde_json::Map::new();
    for (id, e) in snap {
        models.insert(
            id.clone(),
            json!({
                "kind": e.kind,
                "available": e.available(mr_memory::health::now()),
                "cooldown_remaining_ms": e.cooldown_remaining_ms(mr_memory::health::now()),
                "hits": e.hits,
                "message": e.message,
            }),
        );
    }
    (axum::Json(json!({ "models": models }))).into_response()
}

/// Flywheel aggregates + learned telemetry per model.
pub async fn api_stats(State(st): State<AppState>) -> Response {
    let stats = st.flywheel.stats();
    let telemetry = st.flywheel.telemetry_snapshot();
    let mut models = serde_json::Map::new();
    for (id, s) in &stats {
        let t = telemetry.get(id).cloned().unwrap_or_default();
        models.insert(
            id.clone(),
            json!({
                "requests": s.requests,
                "success": s.success,
                "failures": s.failures,
                "avg_ttft_ms": (s.ttft_n > 0).then(|| s.ttft_ms_sum / s.ttft_n),
                "avg_total_ms": (s.total_ms_n > 0).then(|| s.total_ms_sum / s.total_ms_n),
                "prompt_tokens": s.prompt_tokens,
                "completion_tokens": s.completion_tokens,
                "cached_tokens": s.cached_tokens,
                "tool_calls": {"total": s.tc_total, "valid_json": s.tc_valid_json,
                               "known_name": s.tc_known_name, "schema_ok": s.tc_schema_ok},
                "truncations": s.truncations,
                "degenerate": s.degenerate,
                "empty_responses": s.empty_responses,
                "semantic": {"matched": s.sem_matched, "total": s.sem_total},
                "feedback": {"ok": s.fb_ok, "total": s.fb_total},
                "telemetry": t,
                "user_weight": st.engine.catalog.get(id).and_then(|m| m.weight),
            }),
        );
    }
    (axum::Json(json!({"models": models}))).into_response()
}

/// Plugin-reported explicit feedback (L4 ground truth from agents that
/// run the modelroute adapter).
pub async fn api_feedback(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let ok = body.get("ok").and_then(|o| o.as_bool());
    let session = body.get("session").and_then(|s| s.as_str()).unwrap_or("");
    // plugins don't know which model served the session; the gateway does
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .or_else(|| st.sessions.get(session).map(|s| s.chosen))
        .unwrap_or_default();
    if model.is_empty() || ok.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": "model (or a known session) and ok (bool) are required"}})),
        )
            .into_response();
    }
    st.flywheel.observe_feedback(&model, ok.unwrap());
    st.events.record(json!({
        "kind": "feedback",
        "model": model,
        "ok": ok.unwrap(),
        "tool": body.get("tool").cloned().unwrap_or(Value::Null),
        "call_id": body.get("call_id").cloned().unwrap_or(Value::Null),
        "detail": body.get("detail").cloned().unwrap_or(Value::Null),
        "session": session,
    }));
    (axum::Json(json!({"status": "ok"}))).into_response()
}

/// SSE stream of routing events for the dashboard.
pub async fn api_stream(
    State(st): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = st.bus.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|e| async move {
        match e {
            Ok(v) => Some(Ok(Event::default().data(v.to_string()))),
            Err(_) => None,
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

pub async fn messages_stub() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        axum::Json(json!({
            "error": {
                "type": "modelroute_not_implemented",
                "message": "Anthropic /v1/messages ingress lands in M3; use the OpenAI-compatible /v1/chat/completions for now"
            }
        })),
    )
        .into_response()
}
