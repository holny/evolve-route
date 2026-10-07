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
    // advertised window for auto = largest context in the eligible pool;
    // dynamic-catalog agents (hermes/openrouter-style) read this directly
    let max_window = st
        .engine
        .catalog
        .models
        .iter()
        .filter_map(|m| m.context_window)
        .max()
        .unwrap_or(0);
    let mut data = vec![
        json!({"id": "auto", "object": "model", "owned_by": "modelroute", "source": "router",
               "description": "intelligent routing (default policy)",
               "context_length": max_window}),
        json!({"id": "auto:cost", "object": "model", "owned_by": "modelroute", "source": "router",
               "description": "intelligent routing, cost-optimized policy",
               "context_length": max_window}),
        json!({"id": "auto:quality", "object": "model", "owned_by": "modelroute", "source": "router",
               "description": "intelligent routing, quality-optimized policy",
               "context_length": max_window}),
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

/// Flywheel aggregates + learned telemetry + link status + dynamic score.
pub async fn api_stats(State(st): State<AppState>) -> Response {
    let stats = st.flywheel.stats();
    let telemetry = st.flywheel.telemetry_snapshot();
    let overlays = st.engine.tier_overrides();
    let health_snap = st.health.snapshot();
    let now = mr_memory::health::now();
    let mut models = serde_json::Map::new();
    // iterate the CATALOG (not just traffic stats) so every known model
    // shows up, ranked by dynamic priority; top 50 returned
    let mut ranked: Vec<(String, serde_json::Value)> = Vec::new();
    for m in &st.engine.catalog.models {
        let id = &m.id;
        let empty = Default::default();
        let s = stats.get(id).unwrap_or(&empty);
        let t = telemetry.get(id).cloned().unwrap_or_default();
        // link status from health ledger (any key ok = up)
        let key_ids: Vec<String> = {
            let keys = m.key_values();
            if keys.is_empty() {
                vec![id.clone()]
            } else {
                (0..keys.len()).map(|i| format!("{id}\u{1f}{i}")).collect()
            }
        };
        let link = {
            let any_ok = key_ids.iter().any(|k| {
                health_snap.get(k).map(|h| h.available(now)).unwrap_or(true)
            });
            let any_entry = key_ids.iter().any(|k| health_snap.contains_key(k));
            if !any_entry { "unknown" } else if any_ok { "up" } else { "down" }
        };
        // dynamic priority: benchmark tiers avg x reliability/speed x weight
        let (tiers, conf) = overlays.get(id).cloned().unwrap_or({
            (m.tiers, 0.0)
        });
        let bench_avg = (tiers.coding + tiers.reasoning + tiers.agentic) / 3.0;
        // 性价比优先度（用户修正）：能力不是全部——按量计费模型看每次请求
        // 的真实花费；coding plan 订阅模型看 5h/7d 窗口余量（好钢用在刀刃上）
        let dyn_score = {
            let rel = t.reliability.unwrap_or(0.7);
            let spd = t.speed_obs.unwrap_or(0.5);
            let cost_eff = if m.plan {
                // 订阅套餐：配额内边际成本≈0，用掉才值
                1.0_f32
            } else {
            match m.cost {
                // 按量：以一次典型 4k-in/0.5k-out 请求为基准，与目录最便宜
                // 模型比价；无价模型（订阅摊薄）按 0.8 中性偏优
                Some(c) => {
                    let typical = (4_000.0 / 1e6) * c.input as f64 + (500.0 / 1e6) * c.output as f64;
                    let cheapest = st
                        .engine
                        .catalog
                        .models
                        .iter()
                        .filter_map(|x| {
                            x.cost.map(|cc| {
                                (4_000.0 / 1e6) * cc.input as f64
                                    + (500.0 / 1e6) * cc.output as f64
                            })
                        })
                        .fold(f64::MAX, f64::min);
                    ((cheapest / typical) as f32).clamp(0.05, 1.0)
                }
                None => 0.8,
            }
            };
            let quota_factor = st
                .quota
                .model_remaining_tokens(id)
                .map(|rem| ((rem as f32) / 4_000.0).min(1.0))
                .unwrap_or(1.0);
            let raw = 0.40 * bench_avg + 0.25 * rel + 0.15 * spd + 0.20 * cost_eff;
            let w = m.weight.unwrap_or(1.0).clamp(0.2, 3.0);
            let bias = t.learned_bias.unwrap_or(1.0).clamp(0.7, 1.3);
            (raw * (w * bias).sqrt().clamp(0.4, 1.8) * quota_factor).clamp(0.0, 1.0) * 100.0
        };
        let success_rate = (s.requests > 0).then(|| s.success as f32 / s.requests as f32);
        let cache_hit_rate = (s.prompt_tokens > 0)
            .then(|| s.cached_tokens as f32 / s.prompt_tokens as f32);
        let cat_model = st.engine.catalog.get(id);
        let currency = cat_model.map(|m| m.currency.clone()).unwrap_or_default();
        let tiers = cat_model.map(|m| {
            json!({
                "coding": ((m.tiers.coding as f64) * 100.0).round() / 100.0,
                "reasoning": ((m.tiers.reasoning as f64) * 100.0).round() / 100.0,
                "agentic": ((m.tiers.agentic as f64) * 100.0).round() / 100.0,
            })
        });
        let source = cat_model.map(|m| m.source.label());
        let est_cost: Option<f32> = match cat_model.map(|m| (m.plan, m.cost)) {
            Some((true, _)) | Some((false, None)) => Some(0.0),
            Some((false, Some(c))) => Some(
                (s.prompt_tokens as f32 / 1e6) * c.input
                    + (s.completion_tokens as f32 / 1e6) * c.output,
            ),
            _ => None,
        };
        ranked.push((
            id.clone(),
            json!({
                "link": link,
                "dynamic_score": (dyn_score * 10.0).round() / 10.0,
                "bench_confidence": conf,
                "requests": s.requests,
                "success": s.success,
                "failures": s.failures,
                "avg_ttft_ms": (s.ttft_n > 0).then(|| s.ttft_ms_sum / s.ttft_n),
                "avg_total_ms": (s.total_ms_n > 0).then(|| s.total_ms_sum / s.total_ms_n),
                "prompt_tokens": s.prompt_tokens,
                "completion_tokens": s.completion_tokens,
                "cached_tokens": s.cached_tokens,
                "cache_write_tokens": s.cache_write_tokens,
                "tool_calls": {"total": s.tc_total, "valid_json": s.tc_valid_json,
                               "known_name": s.tc_known_name, "schema_ok": s.tc_schema_ok},
                "truncations": s.truncations,
                "degenerate": s.degenerate,
                "empty_responses": s.empty_responses,
                "semantic": {"matched": s.sem_matched, "total": s.sem_total},
                "feedback": {"ok": s.fb_ok, "total": s.fb_total},
                "success_rate": success_rate,
                "cache_hit_rate": cache_hit_rate,
                "est_cost": est_cost,
                "currency": currency,
                "tiers": tiers,
                "source": source,
                "samples": s.requests,
                "last_seen_ms": (s.last_seen_ms > 0).then_some(s.last_seen_ms),
                "last_total_ms": (s.last_total_ms > 0).then_some(s.last_total_ms),
                "last_ttft_ms": (s.last_ttft_ms > 0).then_some(s.last_ttft_ms),
                // 平均吐字速率：完成 tokens ÷ 生成窗口（总耗时-TTFT 累计）；
                // f64 域取整避免 f32 序列化毛刺
                "avg_rate_tok_s": (s.total_ms_sum > s.ttft_ms_sum).then(|| {
                    let gen_s = (s.total_ms_sum - s.ttft_ms_sum) as f32 / 1000.0;
                    ((s.completion_tokens as f64 / gen_s as f64) * 10.0).round() / 10.0
                }),
                "last_rate_tok_s": (s.last_rate_tok_s > 0).then_some(s.last_rate_tok_s),
                "telemetry": t,
                "user_weight": st
                    .engine
                    .weight_override(id)
                    .or(m.weight)
                    .map(|w| ((w as f64) * 100.0).round() / 100.0),
            }),
        ));
    }
    // 模型动态排序（用户规则）：有流量的固定前排，内部按 流量(请求数) 降序、
    // 再按路由优先度降序；无流量的殿后，纯按路由优先度降序
    ranked.sort_by(|a, b| {
        let get = |v: &serde_json::Value, k: &str| {
            v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0)
        };
        let (ra, rb) = (get(&a.1, "requests"), get(&b.1, "requests"));
        let (da, db) = (get(&a.1, "dynamic_score"), get(&b.1, "dynamic_score"));
        let ta = if ra > 0.0 { 0 } else { 1 };
        let tb = if rb > 0.0 { 0 } else { 1 };
        ta.cmp(&tb)
            .then(rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal))
            .then(db.partial_cmp(&da).unwrap_or(std::cmp::Ordering::Equal))
    });
    ranked.truncate(15);
    for (id, entry) in ranked {
        models.insert(id, entry);
    }
    (axum::Json(json!({"models": models}))).into_response()
}

/// Benchmark feed status + currently applied tier overlay.
pub async fn api_benchmarks(State(st): State<AppState>) -> Response {
    let overlay = st.engine.tier_overrides();
    let dir = if st.config.data.dir.starts_with("~/") {
        std::env::var("HOME")
            .map(|h| std::path::PathBuf::from(h).join(&st.config.data.dir[2..]))
            .unwrap_or_else(|_| std::path::PathBuf::from(&st.config.data.dir))
    } else {
        std::path::PathBuf::from(&st.config.data.dir)
    };
    let snapshot = std::fs::read_to_string(dir.join("benchmarks.json"))
    .ok()
    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    .unwrap_or(serde_json::json!({}));
    let mut models = serde_json::Map::new();
    for (id, (tiers, conf)) in overlay {
        models.insert(id, json!({ "tiers": tiers, "confidence": conf }));
    }
    (axum::Json(json!({ "sources": snapshot, "applied": models }))).into_response()
}

/// Recent routing events for the dashboard decision feed.
pub async fn api_events(
    State(st): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let dir = shellexpand_dir(&st.config.data.dir);
    let path = dir.join("events.jsonl");
    let mut events = Vec::new();
    if let Ok(content) = tokio::fs::read_to_string(&path).await {
        for line in content.lines().rev() {
            if events.len() >= limit { break; }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                events.push(v);
            }
        }
        events.reverse(); // oldest first, so prepend works for feed
    }
    (axum::Json(serde_json::Value::Array(events))).into_response()
}

fn shellexpand_dir(dir: &str) -> std::path::PathBuf {
    if dir.starts_with("~/") {
        std::env::var("HOME")
            .map(|h| std::path::PathBuf::from(h).join(&dir[2..]))
            .unwrap_or_else(|_| std::path::PathBuf::from(dir))
    } else {
        std::path::PathBuf::from(dir)
    }
}

/// Quota windows learned from upstream rate-limit headers.
pub async fn api_quota(State(st): State<AppState>) -> Response {
    (axum::Json(json!({ "windows": st.quota.snapshot() }))).into_response()
}

/// Plugin-reported explicit feedback (L4 ground truth from agents that
/// run the modelroute adapter).
pub async fn api_feedback(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let ok = body.get("ok").and_then(|o| o.as_bool());
    let session = body.get("session").and_then(|s| s.as_str()).unwrap_or("");
    let agent = body
        .get("agent")
        .and_then(|a| a.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "-".into());
    // plugins don't know which model served the session; the gateway does
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .or_else(|| st.sessions.get_by_session(&[agent], session).map(|s| s.chosen))
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

/// 面板权重调控：热生效（engine overlay）+ overrides.json 持久化
pub async fn api_weight(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let Some(model) = body.get("model").and_then(|m| m.as_str()).map(|s| s.to_string()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "model is required"}}))).into_response();
    };
    let Some(w) = body.get("weight").and_then(|w| w.as_f64()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "weight (number 0.2-3.0) is required"}}))).into_response();
    };
    let w = (w as f32).clamp(0.2, 3.0);
    st.engine.set_weight_override(&model, Some(w));
    let dir = &st.config.data.dir;
    let mut ov = crate::state::load_overrides(dir);
    ov.models.insert(model.clone(), w);
    crate::state::save_overrides(dir, &ov);
    (axum::Json(json!({"status": "ok", "model": model, "weight": w}))).into_response()
}

/// 当前生效的公式权重（override > 默认 profile）
fn effective_weights(st: &AppState) -> mr_core::types::PolicyWeights {
    st.engine.weights_override().unwrap_or_else(|| {
        mr_core::types::PolicyProfile::parse(&st.config.policy.default)
            .unwrap_or(mr_core::types::PolicyProfile::Balanced)
            .weights()
    })
}

pub async fn api_policy_weights_get(State(st): State<AppState>) -> Response {
    let w = effective_weights(&st);
    let over = st.engine.weights_override().is_some();
    (
        axum::Json(json!({
            "weights": {"quality": w.quality, "speed": w.speed, "cost": w.cost,
                        "stability": w.stability, "headroom": w.headroom},
            "overridden": over,
            "profile": st.config.policy.default,
        })),
    )
        .into_response()
}

/// 公式权重调控：任意子集，缺省沿用当前值；归一化后热生效并持久化
pub async fn api_policy_weights_set(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let cur = effective_weights(&st);
    let pick = |k: &str, fallback: f32| -> Result<f32, (StatusCode, serde_json::Value)> {
        match body.get(k) {
            Some(v) => {
                let f = v.as_f64().unwrap_or(-1.0) as f32;
                if !(0.0..=2.0).contains(&f) || !f.is_finite() {
                    Err((StatusCode::BAD_REQUEST, json!({"error": {"message": format!("{k} must be within 0.0-2.0")}})))
                } else {
                    Ok(f)
                }
            }
            None => Ok(fallback),
        }
    };
    let quality = match pick("quality", cur.quality) { Ok(v) => v, Err((code, msg)) => return (code, axum::Json(msg)).into_response() };
    let speed = match pick("speed", cur.speed) { Ok(v) => v, Err((code, msg)) => return (code, axum::Json(msg)).into_response() };
    let cost = match pick("cost", cur.cost) { Ok(v) => v, Err((code, msg)) => return (code, axum::Json(msg)).into_response() };
    let stability = match pick("stability", cur.stability) { Ok(v) => v, Err((code, msg)) => return (code, axum::Json(msg)).into_response() };
    let headroom = match pick("headroom", cur.headroom) { Ok(v) => v, Err((code, msg)) => return (code, axum::Json(msg)).into_response() };
    let w = mr_core::types::PolicyWeights { quality, speed, cost, stability, headroom };
    let sum = w.quality + w.speed + w.cost + w.stability + w.headroom;
    if sum <= 0.01 {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "weights sum must be > 0"}}))).into_response();
    }
    st.engine.set_weights_override(Some(w.clone()));
    let dir = &st.config.data.dir;
    let mut ov = crate::state::load_overrides(dir);
    ov.weights = Some([
        w.quality, w.speed, w.cost, w.stability, w.headroom,
    ]);
    let normalized = w.normalized();
    crate::state::save_overrides(dir, &ov);
    (
        axum::Json(json!({
            "status": "ok",
            "normalized": {"quality": normalized[0], "speed": normalized[1], "cost": normalized[2], "stability": normalized[3], "headroom": normalized[4]},
        })),
    )
        .into_response()
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
