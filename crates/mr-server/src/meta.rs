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
/// 窗口聚合：把模型近期样本环按窗口过滤后折算成与累计口径同形的聚合值
fn windowed_view(
    s: &mr_core::types::ModelTelemetry,
    window: &str,
) -> Option<serde_json::Value> {
    use mr_core::types::ReqSample;
    let samples: Vec<ReqSample> = s.recent.clone().unwrap_or_default();
    if samples.is_empty() {
        return None;
    }
    let now = mr_memory::health::now();
    let cutoff_ms: Option<u64> = match window {
        "7d" => Some(7 * 24 * 3600 * 1000),
        "24h" => Some(24 * 3600 * 1000),
        "1h" => Some(3600 * 1000),
        "10m" => Some(10 * 60 * 1000),
        _ => None,
    };
    let take_n: Option<usize> = match window {
        "last30" => Some(30),
        "last10" => Some(10),
        _ => None,
    };
    let mut sel: Vec<ReqSample> = samples
        .iter()
        .copied()
        .filter(|r| match cutoff_ms {
            Some(ms) => now.saturating_sub(r.ts) <= ms,
            None => true,
        })
        .collect();
    if let Some(n) = take_n {
        sel = sel.split_off(sel.len().saturating_sub(n));
    }

    if sel.is_empty() {
        return None;
    }
    fn avg_of(sel: &[ReqSample], f: impl Fn(&ReqSample) -> u64) -> u64 {
        (sel.iter().map(|r| f(r) as f64).sum::<f64>() / sel.len() as f64).round() as u64
    }
    let avg = |f: fn(&ReqSample) -> u64| avg_of(&sel, f);
    let ttft = avg(|r| r.ttft_ms);
    let total = avg(|r| r.total_ms);
    let in_tok: u64 = sel.iter().map(|r| r.in_tok).sum();
    let cached: u64 = sel.iter().map(|r| r.cached_tok).sum();
    let out_tok: u64 = sel.iter().map(|r| r.out_tok).sum();
    let ok = sel.iter().filter(|r| r.ok).count();
    let gen_ms: u64 = sel.iter().map(|r| r.total_ms.saturating_sub(r.ttft_ms)).sum();
    let rate = if gen_ms > 0 { (out_tok as f64 * 1000.0 / gen_ms as f64).round() as u64 } else { 0 };
    let tools_total: u32 = sel.iter().map(|r| r.tools_total).sum();
    let tools_ok: u32 = sel.iter().map(|r| r.tools_ok).sum();
    Some(json!({
        "requests": sel.len(),
        "success": ok,
        "avg_ttft_ms": ttft,
        "avg_total_ms": total,
        "avg_rate_tok_s": rate,
        "in_tok": in_tok,
        "cached_tok": cached,
        "cache_hit_rate": if in_tok > 0 { Some(cached as f32 / in_tok as f32) } else { None },
        "tool_calls": {"total": tools_total, "valid_json": tools_ok},
        "last_total_ms": sel.last().map(|r| r.total_ms),
        "last_ttft_ms": sel.last().map(|r| r.ttft_ms),
        "last_rate_tok_s": rate,
        "window": window,
    }))
}

/// 窗口聚合覆盖：把窗口视图写回 ModelStats 聚合字段（仪表盘渲染口径不变）
fn apply_windowed(s: &mut mr_memory::flywheel::ModelStats, view: &serde_json::Value) {
    let g = |k: &str| view.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    s.requests = g("requests");
    s.success = g("success");
    if let Some(v) = view.get("avg_ttft_ms").and_then(|v| v.as_u64()) {
        s.ttft_ms_sum = v;
        s.ttft_n = 1;
    }
    if let Some(v) = view.get("avg_total_ms").and_then(|v| v.as_u64()) {
        s.total_ms_sum = v;
        s.total_ms_n = 1;
    }
    if let Some(v) = view.get("avg_rate_tok_s").and_then(|v| v.as_u64()) {
        s.last_rate_tok_s = v;
    }
    s.cached_tokens = g("cached_tok");
    s.prompt_tokens = g("in_tok");
    s.completion_tokens = view.get("avg_rate_tok_s").and_then(|v| v.as_u64()).unwrap_or(0);
    s.tc_total = view
        .get("tool_calls")
        .and_then(|t| t.get("total"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    s.tc_valid_json = view
        .get("tool_calls")
        .and_then(|t| t.get("valid_json"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
}

/// 请求处理耗时分段（流水线瀑布）：预处理/决策/上游首字/流式传输
pub async fn api_stats_query(
    State(st): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let window = params.get("window").cloned().unwrap_or_else(|| "overview".into());
    api_stats_inner(&st, &window).await
}

pub async fn api_stats(State(st): State<AppState>) -> Response {
    api_stats_inner(&st, "overview").await
}

async fn api_stats_inner(st: &AppState, window: &str) -> Response {
    let mut stats = st.flywheel.stats();
    let telemetry = st.flywheel.telemetry_snapshot();
    // 窗口模式：用近期样本环折算聚合，覆盖累计口径（总览保持原样）
    if window != "overview" {
        for (id, tm) in telemetry.iter() {
            if let Some(view) = windowed_view(tm, window)
                && let Some(s) = stats.get_mut(id)
            {
                apply_windowed(s, &view);
            }
        }
    }
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
        let plan_kind = cat_model
            .and_then(|m| mr_core::plans::plan_for(&m.base_url))
            .map(|p| p.plan_kind.to_string());
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
                "plan_kind": plan_kind,
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

/// 趋势图数据源：每模型近期请求样本序列（ts/ttft/total/速率/缓存/工具）
pub async fn api_trends(
    State(st): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let window_ms: u64 = match params.get("window").map(|s| s.as_str()) {
        Some("7d") => 7 * 24 * 3600 * 1000,
        Some("1h") => 3600 * 1000,
        Some("10m") => 10 * 60 * 1000,
        _ => 24 * 3600 * 1000, // 默认 24h
    };
    let limit: usize = params
        .get("models")
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);
    let now = mr_memory::health::now();
    let telemetry = st.flywheel.telemetry_snapshot();
    // 只取有流量的模型，按样本数排序
    let mut entries: Vec<(String, Vec<mr_core::types::ReqSample>)> = telemetry
        .iter()
        .filter_map(|(id, tm)| {
            let rec: Vec<mr_core::types::ReqSample> = tm
                .recent
                .clone()
                .unwrap_or_default()
                .into_iter()
                .filter(|r| now.saturating_sub(r.ts) <= window_ms)
                .collect();
            if rec.is_empty() {
                None
            } else {
                Some((id.clone(), rec))
            }
        })
        .collect();
    entries.sort_by_key(|(_, rec)| std::cmp::Reverse(rec.len()));
    entries.truncate(limit);
    let series: Vec<serde_json::Value> = entries
        .iter()
        .map(|(id, rec)| {
            json!({
                "model": id,
                "points": rec.iter().map(|r| json!({
                    "ts": r.ts,
                    "ttft": r.ttft_ms,
                    "total": r.total_ms,
                    "tok_s": if r.total_ms > r.ttft_ms {
                        (r.out_tok as f64 * 1000.0 / (r.total_ms - r.ttft_ms) as f64).round() as u64
                    } else { 0 },
                    "cache_pct": (r.cached_tok * 100).checked_div(r.in_tok).unwrap_or(0) as u8,
                    "tools_pct": (r.tools_ok * 100).checked_div(r.tools_total).unwrap_or(0) as u8,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    (axum::Json(json!({"window_ms": window_ms, "series": series}))).into_response()
}

/// 全局扫描：手动触发 agent 配置 + 远程 /models 全量重扫
pub async fn api_providers_scan(State(st): State<AppState>) -> Response {
    let before = st.engine.catalog.models.len();
    st.rescan_discovery();
    st.refresh_remote_models();
    let after = st.engine.catalog.models.len();
    // 按 provider 统计扫描结果
    let mut providers: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for m in &st.engine.catalog.models {
        *providers.entry(m.provider.clone()).or_insert(0) += 1;
    }
    let providers_scanned: Vec<Value> = providers.iter()
        .map(|(name, count)| json!({"name": name, "models": count}))
        .collect();
    let new_models = after.saturating_sub(before);
    (axum::Json(json!({
        "status": "ok",
        "catalog_size": after,
        "new_models": new_models,
        "providers_scanned": providers_scanned,
    }))).into_response()
}

/// Provider 实时状态（每 5s 轮询）：health、最后成功时间、当前使用率
pub async fn api_providers_status(State(st): State<AppState>) -> Response {
    let now_ms = mr_memory::health::now();
    let health_snap = st.health.snapshot();
    let sums = st.quota.plan_usage_sums(now_ms, 5 * 3600 * 1000);
    let soft_pct = st.config.policy.plan_soft_pct;
    let ov = crate::state::load_overrides(&st.config.data.dir);
    let mut out: Vec<Value> = Vec::new();
    let manual_bases: std::collections::HashSet<String> = ov.providers.values().map(|p| p.base_url.clone()).collect();
    let mut used_by_url: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for m in &st.engine.catalog.models {
        if !m.plan { continue; }
        if let Some((i, c, o)) = sums.get(&m.id) {
            let credits = mr_core::plans::plan_credits_used(&m.base_url, &m.id, *i, *c, *o);
            *used_by_url.entry(m.base_url.clone()).or_insert(0.0) += credits;
        }
    }
    for (key, pd) in &ov.providers {
        if pd.disabled { continue; }
        let used = used_by_url.get(&pd.base_url).copied().unwrap_or(0.0);
        let allowance = pd.window_5h;
        let pressure = allowance.filter(|a| *a > 0.0).map(|a| used / a);
        let cool = health_snap.iter()
            .filter(|(k, h)| k.split('\u{1f}').next().map(|m| st.engine.catalog.models.iter().any(|mm| mm.id == m && mm.base_url == pd.base_url)).unwrap_or(false))
            .map(|(_, h)| h.cooldown_remaining_ms(now_ms))
            .max();
        out.push(json!({
            "key": key, "source": "manual",
            "base_url": pd.base_url, "tier": pd.tier, "plan_kind": pd.plan_kind,
            "allowance_5h": allowance,
            "used_5h": (used * 100.0).round() / 100.0,
            "pressure": pressure,
            "soft_pct": soft_pct,
            "cooldown_remaining_ms": cool,
            "available": cool.is_none(),
            "fetched_models_count": pd.fetched_models.len(),
        }));
    }
    use std::collections::HashMap;
    let mut grouped: HashMap<String, Value> = HashMap::new();
    for m in &st.engine.catalog.models {
        if manual_bases.contains(&m.base_url) { continue; }
        let profile = mr_core::plans::plan_for(&m.base_url);
        let plan_key = profile.map(|p| p.key.to_string()).unwrap_or_default();
        let entry = grouped.entry(m.provider.clone()).or_insert_with(|| json!({
            "key": m.provider.clone(),
            "source": "scanned",
            "plan_kind": profile.map(|p| p.plan_kind).unwrap_or("api"),
            "docs_url": profile.map(|p| p.docs_url).unwrap_or(""),
            "model_count": 0u64,
            "models": Vec::<Value>::new(),
            "allowance_5h": ov.providers.get(&plan_key).and_then(|p| p.window_5h),
        }));
        if let Some(arr) = entry.get_mut("models").and_then(|v| v.as_array_mut()) {
            arr.push(serde_json::Value::String(m.id.clone()));
        }
        if let Some(o) = entry.as_object_mut() {
            let cur = o.get("model_count").and_then(|v| v.as_u64()).unwrap_or(0);
            o.insert("model_count".into(), serde_json::json!(cur + 1));
        }
    }
    for (provider, mut e) in grouped {
        let used: f64 = e.get("models").and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|id| id.as_str()).filter_map(|id| {
                let mm = st.engine.catalog.models.iter().find(|m| m.id == id)?;
                sums.get(id).map(|(i, c, o)| mr_core::plans::plan_credits_used(&mm.base_url, &mm.id, *i, *c, *o))
            }).sum())
            .unwrap_or(0.0);
        let allowance = e.get("allowance_5h").and_then(|v| v.as_f64());
        let pressure = allowance.filter(|a| *a > 0.0).map(|a| used / a);
        let cool = health_snap.iter()
            .filter(|(k, _)| k.split('\u{1f}').next().map(|m| st.engine.catalog.models.iter().any(|mm| mm.id == m && mm.provider == provider)).unwrap_or(false))
            .map(|(_, h)| h.cooldown_remaining_ms(now_ms))
            .max();
        e.as_object_mut().map(|o| {
            o.insert("used_5h".into(), json!((used * 100.0).round() / 100.0));
            o.insert("pressure".into(), json!(pressure));
            o.insert("cooldown_remaining_ms".into(), json!(cool));
            o.insert("available".into(), json!(cool.is_none()));
        });
        out.push(e);
    }
    (axum::Json(json!({"providers": out, "soft_pct": soft_pct, "now": now_ms}))).into_response()
}

/// Provider 管理列表：内置注册表 + 扫描发现 + 手动定义，合并展示
pub async fn api_providers(State(st): State<AppState>) -> Response {
    let ov = crate::state::load_overrides(&st.config.data.dir);
    let budgets = st.plan_budgets.lock().map(|b| b.clone()).unwrap_or_default();
    let soft_pct = st.config.policy.plan_soft_pct;
    let sums = st.quota.plan_usage_sums(mr_memory::health::now(), 5 * 3600 * 1000);
    let health_snap = st.health.snapshot();
    let now_ms = mr_memory::health::now();
    let mut out: Vec<Value> = Vec::new();
    // 1. 手动定义的（最优先——用户自己加的）
    for (key, pd) in &ov.providers {
        if pd.disabled { continue; }
        // 手动也查用量（base_url 匹配的模型消耗合计）
        let used = st.engine.catalog.models.iter()
            .filter(|m| m.base_url == pd.base_url)
            .filter_map(|m| sums.get(&m.id))
            .map(|(i, c, o)| mr_core::plans::plan_credits_used(&pd.base_url, "", *i, *c, *o))
        .sum::<f64>();
        out.push(json!({
            "key": key, "source": "manual",
            "base_url": pd.base_url, "protocol": pd.protocol,
            "plan_kind": pd.plan_kind, "docs_url": pd.docs_url,
            "tier": pd.tier,
            "windows": {"5h": pd.window_5h, "weekly": pd.window_weekly, "monthly": pd.window_monthly},
            "model_rates": pd.model_rates,
            "fetched_models": pd.fetched_models,
            "has_key": pd.api_key.is_some(),
            "used_5h": (used * 100.0).round() / 100.0,
            "soft_pct": soft_pct,
        }));
    }
    // 2. 目录里实际存在的（扫描+内置合并，去重手动已有的 base_url）
    let manual_urls: Vec<&str> = ov.providers.values().map(|p| p.base_url.as_str()).collect();
    let mut seen_providers = std::collections::HashSet::new();
    for m in &st.engine.catalog.models {
        if manual_urls.iter().any(|u| *u == m.base_url) { continue; }
        // 按 provider 名去重（不同 provider 同 base_url 各自展示，如 opencode-go 与 opencode-go-github）
        if !seen_providers.insert(m.provider.clone()) { continue; }
        let profile = mr_core::plans::plan_for(&m.base_url);
        let plan_key = profile.map(|p| p.key.to_string()).unwrap_or_default();
        let pov = ov.plans.get(&plan_key);
        let tier = pov.and_then(|p| p.tier.clone())
            .unwrap_or_default();
        let allowance_5h = budgets.get(&plan_key).copied();
        let tier_str = pov.and_then(|p| p.tier.clone()).unwrap_or_default();
        let allowance_weekly = mr_core::plans::tier_allowance_by_key(&plan_key, &tier_str, 1);
        let allowance_monthly = mr_core::plans::tier_allowance_by_key(&plan_key, &tier_str, 2);
        // 用量：该 base_url 下所有模型的 5h 积分合计（逐模型按各自系数折算）
        let models_at_url: Vec<&mr_core::types::ModelRecord> = st.engine.catalog.models.iter()
            .filter(|x| x.base_url == m.base_url)
            .collect();
        let used: f64 = models_at_url.iter()
            .filter_map(|mm| {
                sums.get(&mm.id).map(|(i, c, o)| {
                    mr_core::plans::plan_credits_used(&mm.base_url, &mm.id, *i, *c, *o)
                })
            })
            .sum();
        // 下次窗口重置：组内模型的健康冷却取最近
        let next_reset = health_snap.iter()
            .filter(|(k, h)| {
                let km = k.split('\u{1f}').next().unwrap_or(k);
                models_at_url.iter().any(|mm| mm.id == *km)
            })
            .filter(|(_, h)| matches!(h.kind, mr_core::types::HealthKind::QuotaExhausted | mr_core::types::HealthKind::RateLimited))
            .filter_map(|(_, h)| h.until_epoch_ms.filter(|u| *u > now_ms))
            .min();
        let model_ids: Vec<&str> = models_at_url.iter().map(|mm| mm.id.as_str()).collect();
        out.push(json!({
            "key": m.provider.clone(), "source": "discovered",
            "base_url": m.base_url, "protocol": if m.protocol == mr_core::types::Protocol::Anthropic { "anthropic" } else { "openai" },
            "plan_kind": profile.map(|p| p.plan_kind).unwrap_or("api"),
            "docs_url": profile.map(|p| p.docs_url).unwrap_or(""),
            "tier": tier,
            "windows": {
                "5h": allowance_5h,
                "weekly": allowance_weekly,
                "monthly": allowance_monthly,
            },
            "model_rates": profile.map(|p| p.model_rates).unwrap_or(""),
            "model_count": model_ids.len(),
            "models": model_ids,
            "has_key": m.has_credential(),
            "used_5h": if used > 0.0 { (used * 100.0).round() / 100.0 } else { 0.0 },
            "soft_pct": soft_pct,
            "next_reset_epoch_ms": next_reset,
        }));
    }
    out.sort_by(|a, b| {
        let ka = a.get("key").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("key").and_then(|v| v.as_str()).unwrap_or("");
        ka.cmp(kb)
    });
    (axum::Json(json!({"providers": out}))).into_response()
}

/// 新增/修改 provider（手动定义）：baseUrl 变更触发 /models 重拉
pub async fn api_providers_set(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let Some(key) = body.get("key").and_then(|k| k.as_str()).map(|s| s.to_string()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "key is required"}}))).into_response();
    };
    let text = |k: &str| body.get(k).and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let num = |k: &str| body.get(k).and_then(|v| v.as_f64());
    let dir = &st.config.data.dir;
    let mut ov = crate::state::load_overrides(dir);
    let mut base_url_changed = false;
    {
        let entry = ov.providers.entry(key.clone()).or_default();
        if let Some(u) = text("base_url") {
            base_url_changed = entry.base_url != u;
            entry.base_url = u;
        }
        if let Some(k) = text("api_key") { entry.api_key = Some(k); }
        if let Some(v) = text("protocol") { entry.protocol = v; }
        if let Some(v) = text("plan_kind") { entry.plan_kind = v; }
        if let Some(v) = text("docs_url") { entry.docs_url = v; }
        if let Some(v) = text("tier") { entry.tier = v; }
        if let Some(v) = text("model_rates") { entry.model_rates = v; }
        if let Some(v) = num("window_5h") { entry.window_5h = Some(v); }
        if let Some(v) = num("window_weekly") { entry.window_weekly = Some(v); }
        if let Some(v) = num("window_monthly") { entry.window_monthly = Some(v); }
        if let Some(v) = body.get("disabled").and_then(|v| v.as_bool()) { entry.disabled = v; }
    }
    crate::state::save_overrides(dir, &ov);
    // baseUrl 新增/变更 → 立即拉取 /models
    let mut fetched = 0;
    let needs_fetch = base_url_changed
        || ov.providers.get(&key).map(|e| e.fetched_models.is_empty()).unwrap_or(true);
    if needs_fetch {
        let (base, api_key) = {
            let e = ov.providers.get(&key);
            (
                format!("{}{}", e.map(|e| e.base_url.trim_end_matches('/')).unwrap_or(""), "/models"),
                e.and_then(|e| e.api_key.clone()),
            )
        };
        let mut req = st.http.get(&base);
        if let Some(k) = &api_key { req = req.bearer_auth(k); }
        if let Ok(resp) = req.timeout(std::time::Duration::from_secs(5)).send().await
            && let Ok(v) = resp.json::<Value>().await
        {
                let ids: Vec<String> = v.get("data")
                    .and_then(|d| d.as_array())
                    .map(|arr| arr.iter()
                        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()))
                        .collect())
                    .unwrap_or_default();
                fetched = ids.len();
                if let Some(e) = ov.providers.get_mut(&key) { e.fetched_models = ids; }
                crate::state::save_overrides(dir, &ov);
        }
    }
    (axum::Json(json!({"status": "ok", "key": key, "fetched_models": fetched}))).into_response()
}

/// 删除 provider（软删除——disabled=true，面板可恢复）
pub async fn api_providers_delete(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let Some(key) = body.get("key").and_then(|k| k.as_str()).map(|s| s.to_string()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "key is required"}}))).into_response();
    };
    let dir = &st.config.data.dir;
    let mut ov = crate::state::load_overrides(dir);
    let remove_models = body.get("remove_models").and_then(|v| v.as_bool()).unwrap_or(false);
    let base_url = body.get("base_url").and_then(|v| v.as_str()).unwrap_or("");
    // 手动 provider：软删除（disabled）
    if let Some(e) = ov.providers.get_mut(&key) {
        e.disabled = true;
        crate::state::save_overrides(dir, &ov);
        (axum::Json(json!({"status": "ok", "key": key, "disabled": true}))).into_response()
    } else if remove_models && !base_url.is_empty() {
        // 发现 provider：从 catalog_models 移除该 base_url 下的全部模型
        let removed = st.catalog_models.lock()
            .map(|mut m| {
                let before = m.len();
                m.retain(|x| x.base_url != base_url);
                before - m.len()
            })
            .unwrap_or(0);
        (axum::Json(json!({"status": "ok", "key": key, "removed_models": removed}))).into_response()
    } else {
        (StatusCode::NOT_FOUND, axum::Json(json!({"error": {"message": "provider not found"}}))).into_response()
    }
}

/// 手动重拉 provider 的 /models
pub async fn api_providers_refresh(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let Some(key) = body.get("key").and_then(|k| k.as_str()).map(|s| s.to_string()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "key is required"}}))).into_response();
    };
    let dir = &st.config.data.dir;
    let mut ov = crate::state::load_overrides(dir);
    let Some(entry) = ov.providers.get(&key) else {
        return (StatusCode::NOT_FOUND, axum::Json(json!({"error": {"message": "provider not found"}}))).into_response();
    };
    let base = format!("{}{}", entry.base_url.trim_end_matches('/'), "/models");
    let mut req = st.http.get(&base);
    if let Some(k) = &entry.api_key { req = req.bearer_auth(k); }
    match req.timeout(std::time::Duration::from_secs(8)).send().await {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(v) = resp.json::<Value>().await {
                let ids: Vec<String> = v.get("data")
                    .and_then(|d| d.as_array())
                    .map(|arr| arr.iter()
                        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()))
                        .collect())
                    .unwrap_or_default();
                let count = ids.len();
                if let Some(e) = ov.providers.get_mut(&key) { e.fetched_models = ids; }
                crate::state::save_overrides(dir, &ov);
                return (axum::Json(json!({"status": "ok", "fetched": count}))).into_response();
            }
        }
        Ok(resp) => {
            return (StatusCode::BAD_GATEWAY, axum::Json(json!({"error": {"message": format!("upstream {}", resp.status())}}))).into_response();
        }
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, axum::Json(json!({"error": {"message": e.to_string()}}))).into_response();
        }
    }
    (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": {"message": "parse failed"}}))).into_response()
}

/// 配额方案注册表：按 baseUrl 匹配 provider 官方接入方案 + 用户订正
pub async fn api_plans(State(st): State<AppState>) -> Response {
    let ov = crate::state::load_overrides(&st.config.data.dir);
    let health = st.health.snapshot();
    let now_ms = mr_memory::health::now();
    let budgets = st
        .plan_budgets
        .lock()
        .map(|b| b.clone())
        .unwrap_or_default();
    let soft_pct = st.config.policy.plan_soft_pct;
    let sums = st.quota.plan_usage_sums(now_ms, 5 * 3600 * 1000);
    let mut groups: std::collections::BTreeMap<String, serde_json::Value> = std::collections::BTreeMap::new();
    for m in &st.engine.catalog.models {
        let profile = mr_core::plans::plan_for(&m.base_url);
        let key = profile.map(|p| p.key.to_string()).unwrap_or_else(|| format!("payg:{}", m.provider));
        let e = groups.entry(key.clone()).or_insert_with(|| {
            let p = profile.unwrap_or(&mr_core::plans::PAYG);
            let pov = ov.plans.get(&key);
            let allowance = budgets.get(&key).copied();
            json!({
                "key": key,
                "provider": if p.key.is_empty() { m.provider.clone() } else { p.provider.to_string() },
                "plan_kind": p.plan_kind.to_string(),
                "scheme": pov.and_then(|o| o.scheme.clone()).unwrap_or_else(|| p.scheme_key.to_string()),
                "windows": pov.and_then(|o| o.windows.clone()).unwrap_or_else(|| p.windows.to_string()),
                "models_note": p.models_note.to_string(),
                "model_rates": p.model_rates.to_string(),
                "rates_kind": p.rates_kind.to_string(),
                "rates_note": p.rates_note.to_string(),
                "docs_url": p.docs_url.to_string(),
                "tiers": p.tiers.to_string(),
                "tier": pov.and_then(|o| o.tier.clone()).unwrap_or_default(),
                "used_5h": 0.0,
                "allowance": allowance,
                "soft_pct": soft_pct,
                "models": [],
            })
        });
        if let Some(arr) = e.get_mut("models").and_then(|m| m.as_array_mut()) {
            arr.push(json!(m.id));
        }
    }
    // 组内消耗(5h)：Σ 该方案全部模型的积分折算（修正只算首模型的问题）
    let mut used_by_group: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for (key, e) in groups.iter_mut() {
        let mut used = 0.0f64;
        if let Some(arr) = e.get("models").and_then(|m| m.as_array()) {
            for mid in arr.iter().filter_map(|x| x.as_str()) {
                if let Some((i, c, o)) = sums.get(mid) {
                    used += mr_core::plans::plan_credits_used_by_id(mid, *i, *c, *o);
                }
            }
        }
        e["used_5h"] = json!((used * 100.0).round() / 100.0);
        used_by_group.insert(key.clone(), used);
    }
    // 下次窗口重置：组内模型的健康冷却（配额/限流）取最近者
    for e in groups.values_mut() {
        let mut min_until: Option<u64> = None;
        for (hid, h) in &health {
            let hid_model = hid.split('\u{1f}').next().unwrap_or(hid);
            let in_group = e.get("models").and_then(|m| m.as_array()).map(|arr| {
                arr.iter().filter_map(|x| x.as_str()).any(|id| id == hid_model)
            }).unwrap_or(false);
            if !in_group {
                continue;
            }
            if matches!(h.kind, mr_core::types::HealthKind::QuotaExhausted | mr_core::types::HealthKind::RateLimited)
                && let Some(until) = h.until_epoch_ms
                && until > now_ms
            {
                min_until = Some(min_until.map_or(until, |u| u.min(until)));
            }
        }
        e["next_reset_epoch_ms"] = json!(min_until);
    }
    (axum::Json(json!({"providers": groups.values().collect::<Vec<_>>()}))).into_response()
}

/// 配额方案订正：档位/方案/窗口说明（持久化，重启重放；档位同时驱动预算额度）
pub async fn api_plans_set(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let Some(key) = body.get("key").and_then(|k| k.as_str()).map(|s| s.to_string()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "key is required"}}))).into_response();
    };
    let text = |k: &str| body.get(k).and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let dir = &st.config.data.dir;
    let mut ov = crate::state::load_overrides(dir);
    {
        let e = ov.plans.entry(key.clone()).or_default();
        for (k, slot) in [("tier", &mut e.tier), ("scheme", &mut e.scheme), ("windows", &mut e.windows)] {
            if let Some(v) = text(k) {
                *slot = Some(v);
            }
        }
    }
    crate::state::save_overrides(dir, &ov);
    // 档位变化 → 预算额度即时更新（registry 档位表驱动）
    // provider 名不一定等于套餐注册表 key（如 opencode-go-github → opencode-go），
    // 通过 base_url 匹配注册表找到正确的 plan_key
    let base_url = ov.providers.get(&key).map(|p| p.base_url.clone()).unwrap_or_default();
    let plan_key = mr_core::plans::plan_for(&base_url)
        .map(|p| p.key.to_string())
        .unwrap_or(key.clone());
    let tier: String = ov.providers.get(&key)
        .map(|p| p.tier.clone())
        .filter(|t| !t.is_empty())
        .or_else(|| text("tier"))
        .unwrap_or_default();
    let allowance_5h = if tier.is_empty() { None } else { mr_core::plans::tier_allowance_by_key(&plan_key, &tier, 0) };
    let allowance_weekly = if tier.is_empty() { None } else { mr_core::plans::tier_allowance_by_key(&plan_key, &tier, 1) };
    let allowance_monthly = if tier.is_empty() { None } else { mr_core::plans::tier_allowance_by_key(&plan_key, &tier, 2) };
    // 预算写入用 plan_key（路由按 plan_key 查预算）
    if let Some(a) = allowance_5h {
        if let Ok(mut b) = st.plan_budgets.lock() {
            b.insert(plan_key.clone(), a);
        }
    }
    let e = ov.providers.get(&key).cloned().unwrap_or_default();
    (axum::Json(json!({"status": "ok", "key": key, "plan_key": plan_key, "override": e, "allowance_5h": allowance_5h, "allowance_weekly": allowance_weekly, "allowance_monthly": allowance_monthly}))).into_response()
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
