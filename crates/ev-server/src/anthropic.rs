//! Anthropic Messages ingress (/v1/messages) — claude-code and
//! Anthropic-SDK clients. Same-protocol traffic relays near-verbatim
//! (surgical model rewrite); anthropic→openai upstream goes through the
//! Switchyard codecs in both directions.

use crate::rewrite::rewrite_model_field;
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use ev_core::engine::RoutingInput;
use ev_core::tokens as tok;
use ev_core::types::*;
use futures::{StreamExt, TryStreamExt};
use ev_memory::health::{classify_failure, Failure};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Instant;

const MAX_ERROR_BODY: usize = 8 * 1024;

pub async fn messages(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let started = Instant::now();
    let agent_hdr = crate::identity::agent_identity(&headers, &st.config.telemetry.agent_header);
    let Ok(parsed) = serde_json::from_slice::<Value>(&body) else {
        return mr_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let model_field = parsed.get("model").and_then(|v| v.as_str()).unwrap_or("auto").to_string();
    let target = match resolve_target(&st, &model_field) {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    let session_key = session_key_of(&st, &headers, &parsed);
    let sticky_key = format!("{}\u{1f}{}", agent_hdr, session_key);
    let header_policy = headers
        .get("x-ev-policy")
        .and_then(|v| v.to_str().ok())
        .and_then(PolicyProfile::parse);

    // Anthropic max_tokens is mandatory per API spec
    let max_output_req = parsed.get("max_tokens").and_then(|v| v.as_u64());
    let est = estimate_anthropic(&parsed);
    let features = features_anthropic(&parsed, est);
    let tools_sig = tools_signature(&parsed);
    let digest = build_digest(&parsed);
    let tool_heavy = features.tool_ratio;

    let decision_started = std::time::Instant::now();
    let mut decision = match &target {
        Target::Auto(alias_policy) => {
            let sticky = st.sessions.get(&sticky_key);
            let health = st.health.snapshot();
            let telemetry = st.flywheel.telemetry_snapshot();
            let quota_view = st.quota.best_remaining_by_models();
            let plan_pressure = st.plan_pressure_map();
            if let Some(pending) = st.sessions.take_pending(&sticky_key) {
                let returned = returned_tool_result_ids(&parsed);
                let total = pending.call_ids.len() as u64;
                let matched = pending.call_ids.iter().filter(|id| returned.contains(id)).count() as u64;
                if total > 0 {
                    st.flywheel.observe_semantic(&pending.model, matched, total);
                    st.events.record(json!({
                        "kind": "semantic", "model": pending.model,
                        "matched": matched, "total": total, "session": session_key,
                    }));
                }
            }
            let input = RoutingInput {
                session_key: &session_key,
                features,
                digest: &digest,
                tools_sig,
                max_output_req,
                policy: header_policy.or(*alias_policy),
                sticky,
                health: &health,
                telemetry: &telemetry,
                quota: &quota_view,
                plan_pressure: &plan_pressure,
            };
            let d = st.engine.decide(input);
            // 粘性的度：仅非粘性的重新判定重置轮次；粘性延续扣减——
            // 否则每轮 put 满额轮次，turns_left 永不触 0，粘性无度
            let turns_left = if d.sticky {
                st.sessions
                    .get(&sticky_key)
                    .map(|s| s.turns_left.saturating_sub(1))
                    .unwrap_or(st.config.policy.sticky_turns)
            } else {
                st.config.policy.sticky_turns
            };
            st.sessions.put(
                &sticky_key,
                StickyState {
                    chosen: d.chosen.clone(),
                    est_tokens_band: tok::tokens_band(est),
                    turns_left,
                    tools_sig,
                    domain: d.judgment.domain,
                    difficulty: d.difficulty_eff,
                    est_tokens: est,
                },
            );
            d
        }
        Target::Direct(dec) => dec.clone(),
    };
    decision.est_input_tokens = est;
    let decision_ms = decision_started.elapsed().as_millis() as u64;
    let preprocess_ms = decision_started
        .saturating_duration_since(started)
        .as_millis() as u64;
    let _ = tool_heavy;

    let original_choice = decision.chosen.clone();
    let is_stream = parsed.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    let (attempts, sweep_from): (Vec<String>, usize) = match &target {
        Target::Auto(_) => {
            let mut seen = std::collections::HashSet::new();
            let mut list: Vec<String> = decision
                .chain
                .iter()
                .filter(|c| seen.insert((*c).clone()))
                .cloned()
                .collect();
            let depth = st.config.policy.fallback_depth.max(3) as usize;
            if list.len() < depth && !decision.scores.is_empty() {
                let mut ranked: Vec<(String, f32)> =
                    decision.scores.iter().map(|(k, v)| (k.clone(), *v)).collect();
                ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                for (m, _) in ranked {
                    if list.len() >= depth {
                        break;
                    }
                    if seen.insert(m.clone()) {
                        list.push(m);
                    }
                }
            }
            // 链外兜底（用户裁决，与 relay 同规则）：主链全灭时目录里其余
            // provider 的模型队尾一试（含冷却中/未评分的），上限 6 个
            let sweep_from = list.len();
            let cat = st.engine.catalog_snapshot();
            let self_addr = format!("{}:{}", st.config.server.host, st.config.server.port);
            for m in &cat {
                if list.len() - sweep_from >= 6 {
                    break;
                }
                if m.base_url.contains(&self_addr) {
                    continue;
                }
                if seen.insert(m.id.clone()) {
                    list.push(m.id.clone());
                }
            }
            (list, sweep_from)
        }
        Target::Direct(_) => (vec![decision.chosen.clone()], usize::MAX),
    };

    let mut skipped: Vec<String> = Vec::new();
    let mut last_error: Option<(StatusCode, Bytes)> = None;
    let mut min_context_needed: Option<u64> = None;

    // provider 账户级熔断（与 relay 同规则）
    let mut dead_providers: std::collections::HashSet<String> = std::collections::HashSet::new();
    // 网络级熔断（与 relay 同规则）：transport 错误按 baseUrl 熔断
    let mut dead_routes: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (cand_idx, cand) in attempts.iter().enumerate() {
        let Some(record) = st.catalog_get(cand) else { continue };
        if dead_providers.contains(&record.provider) {
            skipped.push(format!("{cand}(provider {} account-level failure)", record.provider));
            continue;
        }
        if dead_routes.contains(&record.base_url) {
            skipped.push(format!("{cand}(route {} network failure)", record.base_url));
            continue;
        }
        if let Some(need) = min_context_needed
            && record.context_window.map(|w| w < need).unwrap_or(true)
        {
            skipped.push(format!("{cand}(window < {need})"));
            continue;
        }
        let to_openai = record.protocol == Protocol::OpenAI;
        // multi-key pool: rotate through keys (mirrors relay.rs); on auth/
        // quota failure the health ledger marks model⟨sep⟩keyidx and the
        // next key — then the next candidate — gets tried
        let keys = record.key_values();
        let key_len = keys.len().max(1);
        let key_start = st.key_start(cand, key_len);
        let health_snap = st.health.snapshot();
        let now_ms_v = ev_memory::health::now();
        let _key_exhausted = false;

        for key_off in 0..key_len {
        let key_idx = (key_start + key_off) % key_len;
        let key_value = keys.get(key_idx).cloned();
        let health_id = if keys.is_empty() {
            cand.clone()
        } else {
            format!("{cand}\u{1f}{key_idx}")
        };
        if let Some(h) = health_snap.get(&health_id)
            && !h.available(now_ms_v)
        {
            // 链外兜底段冷却放行（与 relay 同规则）：全灭好过硬报错
            if cand_idx < sweep_from {
                skipped.push(format!("{cand}[{key_idx}] cooldown"));
                continue;
            }
            skipped.push(format!("{cand}[{key_idx}] last-resort (cooling)"));
        }

        let (fwd_body, url) = if to_openai {
            let translated = crate::translate::Translator::global()
                .request_anthropic_to_openai(&parsed);
            match translated {
                Ok(out) => {
                    let mut v = out;
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("model".into(), json!(record.upstream_model));
                    }
                    (v.to_string().into_bytes(), format!("{}/chat/completions", record.base_url))
                }
                Err(e) => {
                    tracing::warn!(model = %cand, error = %e, "anthropic->openai translation failed");
                    skipped.push(format!("{cand}(translate)"));
                    last_error = Some((StatusCode::BAD_GATEWAY, Bytes::new()));
                    continue;
                }
            }
        } else {
            (rewrite_model_field(&body, &record.upstream_model), format!("{}/messages", record.base_url))
        };

        let mut req = st.http.post(&url).header("content-type", "application/json");
        if to_openai {
            if record.base_url.contains("opencode.ai/zen") {
                req = req.header("x-opencode-session", &session_key);
            }
            if let Some(key) = key_value.as_deref() {
                req = req.bearer_auth(key);
            }
        } else {
            req = req.header("anthropic-version", "2023-06-01");
            if let Some(key) = key_value.as_deref() {
                req = req.header("x-api-key", key);
            }
        }

        let upstream = req.body(fwd_body).send().await;
        let resp = match upstream {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(model = %cand, error = %e, "upstream transport failure");
                st.health.mark_failure(
                    cand,
                    Failure { kind: HealthKind::Transient, message: "transport error".into(), until_epoch_ms: None },
                );
                dead_routes.insert(record.base_url.clone());
                skipped.push(format!("{cand}(transport)"));
                last_error = Some((StatusCode::BAD_GATEWAY, Bytes::new()));
                continue;
            }
        };

        let status = resp.status();
        if fallback_eligible(status.as_u16()) {
            let retry_after_ms = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|s| s * 1000);
            let err_body = resp.bytes().await.unwrap_or_default();
            let snippet =
                String::from_utf8_lossy(&err_body.slice(..err_body.len().min(MAX_ERROR_BODY))).into_owned();
            let failure = classify_failure(status.as_u16(), &snippet, retry_after_ms);
            st.events.record(json!({
                "kind": "upstream_error", "model": cand,
                "upstream_model": record.upstream_model,
                "status": status.as_u16(), "health": failure.kind.label(),
                "message": failure.message, "session": session_key,
            }));
            if failure.kind == HealthKind::ContextOverflow {
                min_context_needed = Some(est.max(1));
            }
            if matches!(
                failure.kind,
                HealthKind::QuotaExhausted
                    | HealthKind::PaymentRequired
                    | HealthKind::AuthFailed
                    | HealthKind::RateLimited
            ) {
                dead_providers.insert(record.provider.clone());
            }
            st.health.mark_failure(&health_id, failure);
            let kind_label = {
                let snap = st.health.snapshot();
                snap.get(&health_id).map(|h| h.kind.label().to_string()).unwrap_or_default()
            };
            skipped.push(format!("{cand}[{key_idx}]({kind_label})"));
            last_error = Some((map_status(status), err_body));
            continue; // next key in pool, then next candidate
        }

        // success: mark per-key when pooled
        let success_health_id = if keys.is_empty() {
            cand.clone()
        } else {
            format!("{cand}\u{1f}{key_idx}")
        };
        st.health.mark_ok(&success_health_id);
        // quota-window learning from success headers (pre-body consumption)
        let windows = ev_memory::QuotaLedger::parse_headers(resp.headers());
        st.quota.observe(&success_health_id, windows.clone());
        // provider 实时配额（panel 配额展示以此为准）
        if let Some(pk) = ev_core::plans::plan_key_for(&record.base_url) {
            st.quota.observe_provider(pk, &windows);
        }
        decision.chosen = cand.clone();
        decision.upstream_model = record.upstream_model.clone();
        decision.id = format!("{}-{}", decision.id, skipped.len());

        let telem = std::sync::Arc::new(std::sync::Mutex::new(crate::stream::Telemetry {
            decision_id: decision.id.clone(),
            session: session_key.clone(),
            chosen: decision.chosen.clone(),
            upstream_model: record.upstream_model.clone(),
            est_tokens: est,
            sticky: decision.sticky,
            stream: is_stream,
            started,
            ttft_ms: None,
            bytes: 0,
            status: status.as_u16(),
            usage: None,
            est_cost_usd: None,
            translated: to_openai.then(|| "openai->anthropic".to_string()),
            agent: Some(agent_hdr),
            plan_key: ev_core::plans::plan_key_for(&record.base_url).map(|k| k.to_string()),
            preprocess_ms: Some(preprocess_ms),
            decision_ms: Some(decision_ms),
            judge_ms: Some(decision.judge_ms),
            extra: Some(json!({
                "reason": decision.reason,
                "scores": decision.scores,
                "chain": decision.chain,
                "judge": decision.judgment.judge_source,
                "judgment": decision.judgment,
                "scoreboard": decision.scored,
                "weights": decision.weights,
                "difficulty_eff": decision.difficulty_eff,
                "filtered": decision.filtered,
                "funnel": decision.funnel,
            })),
        }));

        let mut out = Response::builder().status(map_status(status));
        let h = out.headers_mut().unwrap();
        append_headers(h, &decision, est);
        if !skipped.is_empty() {
            insert_header(h, "x-ev-fallback-from", &original_choice);
            insert_header(h, "x-ev-skipped", &skipped.join(","));
        }

        if !status.is_success() {
            let err_body = resp.bytes().await.unwrap_or_default();
            crate::stream::finalize_event(&st.events, &st.flywheel.clone(), &st.quota, &st.bus, &telem.lock().unwrap());
            insert_header(h, "x-ev-upstream-status", status.as_str());
            return out.body(axum::body::Body::from(err_body)).unwrap();
        }

        if is_stream {
            let ct = resp
                .headers()
                .get("content-type")
                .cloned()
                .unwrap_or_else(|| "text/event-stream".parse().unwrap());
            h.insert("content-type", ct);
            let raw = resp.bytes_stream().map_err(axum::Error::new).boxed();
            // telemetry sees the RAW upstream (openai chunks parse natively);
            // translation wraps telemetry output for the claude-code client
            let telem_body = crate::stream::telemetry_body(
                raw,
                telem,
                st.events.clone(),
                st.flywheel.clone(),
                st.sessions.clone(),
                st.quota.clone(),
                st.bus.clone(),
                parsed.clone(),
                false,
            );
            let client_stream: futures::stream::BoxStream<'static, Result<bytes::Bytes, axum::Error>> =
                if to_openai {
                    crate::translate::OpenaiToAnthropicStream::new(telem_body).boxed()
                } else {
                    telem_body
                };
            return out.body(axum::body::Body::from_stream(client_stream)).unwrap();
        }

        let bytes = resp.bytes().await.unwrap_or_default();
        let upstream_value: Option<Value> = serde_json::from_slice::<Value>(&bytes).ok();
        let mut client_value: Option<Value> = upstream_value.clone();
        if to_openai {
            // upstream spoke OpenAI; claude-code must receive anthropic shape
            if let Some(v) = &upstream_value {
                match crate::translate::Translator::global().response_openai_to_anthropic(v) {
                    Ok(av) => {
                        client_value = Some(av);
                        insert_header(h, "x-ev-translated", "openai->anthropic");
                    }
                    Err(e) => tracing::warn!(error = %e, "openai->anthropic response translation failed"),
                }
            }
        }
        {
            let Ok(mut t) = telem.lock() else {
                return mr_error(StatusCode::INTERNAL_SERVER_ERROR, "telemetry lock poisoned");
            };
            t.bytes = bytes.len() as u64;
            if t.ttft_ms.is_none() {
                t.ttft_ms = Some(started.elapsed().as_millis());
            }
            if let Some(v) = &upstream_value {
                if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                    t.usage = Some(u.clone());
                }
                // analyzer IR is openai-shaped: translate for analysis only
                let analysis_openai: Option<Value> = if to_openai {
                    upstream_value.clone()
                } else {
                    crate::translate::Translator::global()
                        .response_anthropic_to_openai(v)
                        .ok()
                };
                if let Some(openai_v) = &analysis_openai {
                    let q = crate::quality::analyze_response(&to_openai_ir_request(&parsed), openai_v);
                    if let Some(ex) = t.extra.as_mut() {
                        ex["quality"] = serde_json::to_value(&q).unwrap_or_default();
                    } else {
                        t.extra = Some(json!({"quality": q}));
                    }
                }
                let ids: Vec<String> = if to_openai {
                    v.get("choices")
                        .and_then(|c| c.as_array())
                        .and_then(|c| c.first())
                        .and_then(|c| c.get("message"))
                        .and_then(|m| m.get("tool_calls"))
                        .and_then(|t| t.as_array())
                        .map(|tcs| {
                            tcs.iter()
                                .filter_map(|tc| tc.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    v.get("content")
                        .and_then(|c| c.as_array())
                        .map(|blocks| {
                            blocks
                                .iter()
                                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                                .filter_map(|b| b.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let chosen = t.chosen.clone();
                st.sessions.set_pending(&sticky_key, &chosen, ids);
            }
        } // guard dropped

        let client_body = match &client_value {
            Some(v) => serde_json::to_vec(v).unwrap_or_else(|_| bytes.to_vec()),
            None => bytes.to_vec(),
        };
        crate::stream::finalize_event(&st.events, &st.flywheel.clone(), &st.quota, &st.bus, &telem.lock().unwrap());
        h.insert("content-type", "application/json".parse().unwrap());
        return out.body(axum::body::Body::from(client_body)).unwrap();
        } // key_off loop (keys exhausted for this candidate)
    }

    let direct_raw = matches!(target, Target::Direct(_)) && last_error.is_some();
    let (status, err_body) = last_error.unwrap_or((StatusCode::BAD_GATEWAY, Bytes::new()));
    let mut resp = if direct_raw {
        let mut r = Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(err_body))
            .unwrap();
        insert_header(r.headers_mut(), "x-ev-model", &original_choice);
        insert_header(r.headers_mut(), "x-ev-decision-id", &decision.id);
        r
    } else {
        {
            let mut resp = mr_error(map_status(status), "all routed upstreams failed; see x-ev-skipped for per-model reasons");
            if let Some(sec) = skipped.iter()
                .filter_map(|s| s.split('(').last().and_then(|t| t.split('s').next()).and_then(|n| n.parse::<u64>().ok()))
                .max()
            {
                if let Ok(hv) = axum::http::HeaderValue::from_str(&sec.to_string()) {
                    resp.headers_mut().insert("retry-after", hv);
                }
            }
            resp
        }
    };
    if !skipped.is_empty() {
        insert_header(resp.headers_mut(), "x-ev-skipped", &skipped.join(","));
    }
    resp
}

/// Anthropic count_tokens: local estimation (no upstream call).
pub async fn count_tokens(State(_st): State<AppState>, body: Bytes) -> Response {
    let Ok(parsed) = serde_json::from_slice::<Value>(&body) else {
        return mr_error(StatusCode::BAD_REQUEST, "invalid json body");
    };
    let n = estimate_anthropic(&parsed);
    (axum::Json(json!({ "input_tokens": n }))).into_response()
}

// ---------------------------------------------------------------- helpers

#[allow(clippy::large_enum_variant)]
enum Target {
    Auto(Option<PolicyProfile>),
    Direct(Decision),
}

#[allow(clippy::result_large_err)]
fn resolve_target(st: &AppState, model_field: &str) -> Result<Target, Response> {
    if let Some(alias) = model_field.strip_prefix("auto:") {
        let policy = PolicyProfile::parse(alias).ok_or_else(|| {
            mr_error(StatusCode::NOT_FOUND, &format!("unknown policy alias '{alias}'"))
        })?;
        return Ok(Target::Auto(Some(policy)));
    }
    if model_field == "auto" {
        return Ok(Target::Auto(None));
    }
    if let Some(m) = st.catalog_get(model_field) {
        let dec = Decision {
            id: format!("direct-{}", ev_memory::now_millis().unwrap_or(0)),
            chosen: m.id.clone(),
            upstream_model: m.upstream_model.clone(),
            chain: vec![m.id.clone()],
            reason: format!("explicit model selection: {}", m.id),
            scores: Default::default(),
            judgment: JudgmentSet {
                domain: Domain::Other,
                domain_confidence: 1.0,
                difficulty: 0.0,
                difficulty_confidence: 1.0,
                needs_vision: 0.0,
                is_trivial: 0.0,
                tool_heavy: 0.0,
                high_stakes: 0.0,
                session_relevance: 0.0,
                session_depth: 0.0,
                judge_source: "explicit",
            },
            filtered: vec![],
            funnel: [0, 0, 0, 0],
            scored: vec![],
            weights: [0.35, 0.15, 0.25, 0.15, 0.10],
            judge_ms: 0,
            sticky: false,
            est_input_tokens: 0,
            difficulty_eff: 0.0,
        };
        return Ok(Target::Direct(dec));
    }
    Err(mr_error(
        StatusCode::NOT_FOUND,
        &format!("model '{model_field}' is neither 'auto' nor a configured catalog id"),
    ))
}

fn fallback_eligible(status: u16) -> bool {
    matches!(status, 400 | 401 | 402 | 403 | 404 | 408 | 429) || status >= 500
}

fn map_status(s: reqwest::StatusCode) -> StatusCode {
    StatusCode::from_u16(s.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY)
}

fn insert_header(h: &mut axum::http::HeaderMap, k: &'static str, v: &str) {
    if let Ok(val) = axum::http::HeaderValue::from_str(
        &v.chars().map(|c| if c.is_ascii() { c } else { ' ' }).collect::<String>(),
    ) {
        h.insert(k, val);
    }
}

fn append_headers(h: &mut axum::http::HeaderMap, decision: &Decision, est: u64) {
    insert_header(h, "x-ev-model", &decision.chosen);
    insert_header(h, "x-ev-decision-id", &decision.id);
    insert_header(h, "x-ev-sticky", &decision.sticky.to_string());
    insert_header(h, "x-ev-est-tokens", &est.to_string());
    insert_header(h, "x-ev-reason", &decision.header_reason());
}

fn mr_error(status: StatusCode, msg: &str) -> Response {
    (
        status,
        axum::Json(json!({
            "type": "error",
            "error": { "type": "evolveroute_error", "message": msg }
        })),
    )
        .into_response()
}

fn session_key_of(st: &AppState, headers: &HeaderMap, parsed: &Value) -> String {
    if let Some(v) = crate::identity::session_identity(headers, parsed, &st.config.telemetry.session_header) {
        return v;
    }
    let mut hasher = Sha256::new();
    if let Some(system) = parsed.get("system") {
        hasher.update(system.to_string().as_bytes());
    }
    if let Some(msgs) = parsed.get("messages").and_then(|v| v.as_array())
        && let Some(first) = msgs.first() {
            hasher.update(first.to_string().as_bytes());
        }
    format!("h{}", hex::encode(&hasher.finalize()[..8]))
}

fn tools_signature(parsed: &Value) -> u64 {
    let tools = parsed.get("tools").map(|t| t.to_string()).unwrap_or_default();
    let hash = Sha256::digest(tools.as_bytes());
    u64::from_be_bytes(hash[0..8].try_into().unwrap())
}

/// Flatten anthropic body text: system (string or blocks) + message blocks.
fn anthropic_text(parsed: &Value) -> (String, String, bool) {
    // returns (system, last_user_text, has_images)
    let block_text = |c: &Value| -> String {
        match c {
            Value::String(s) => s.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        }
    };
    let system = parsed
        .get("system")
        .map(block_text)
        .unwrap_or_default();
    let mut first_user = String::new();
    let mut last_user = String::new();
    let mut has_images = false;
    if let Some(msgs) = parsed.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            if m.get("role").and_then(|r| r.as_str()) != Some("user") {
                continue;
            }
            if let Some(c) = m.get("content") {
                if let Value::Array(parts) = c
                    && parts.iter().any(|b| {
                        b.get("type").and_then(|t| t.as_str()) == Some("image")
                    }) {
                        has_images = true;
                    }
                let t = block_text(c);
                if first_user.is_empty() {
                    first_user = t.clone();
                }
                last_user = t;
            }
        }
    }
    (system, {
        let _ = &first_user;
        last_user
    }, has_images)
}

fn estimate_anthropic(parsed: &Value) -> u64 {
    let (system, _, _) = anthropic_text(parsed);
    let mut total = tok::estimate_text(&system);
    if let Some(msgs) = parsed.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            total += 4;
            if let Some(c) = m.get("content") {
                if let Value::String(s) = c {
                    total += tok::estimate_text(s);
                } else if let Value::Array(parts) = c {
                    for b in parts {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            total += tok::estimate_text(t);
                        }
                    }
                }
            }
        }
    }
    if let Some(tools) = parsed.get("tools") {
        total += tok::estimate_text(&tools.to_string());
    }
    // tool_result blocks carry (often large) tool output; images are a
    // flat per-image cost
    if let Some(msgs) = parsed.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            if let Some(Value::Array(parts)) = m.get("content") {
                for b in parts {
                    match b.get("type").and_then(|t| t.as_str()) {
                        Some("tool_result") => {
                            if let Some(c) = b.get("content") {
                                match c {
                                    Value::String(t) => total += tok::estimate_text(t),
                                    Value::Array(bs) => {
                                        for bb in bs {
                                            if let Some(t) = bb.get("text").and_then(|t| t.as_str()) {
                                                total += tok::estimate_text(t);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Some("image") => total += 800,
                        _ => {}
                    }
                }
            }
        }
    }
    total
}

fn features_anthropic(parsed: &Value, est: u64) -> RequestFeatures {
    let (system, last_user, has_images) = anthropic_text(parsed);
    let _ = &system;
    let tool_count = parsed.get("tools").and_then(|t| t.as_array()).map(|a| a.len()).unwrap_or(0);
    let sample: String = last_user.chars().take(3000).collect();
    let total = sample.chars().count().max(1) as f32;
    let mut code = 0f32;
    let mut cjk = 0f32;
    for ch in sample.chars() {
        if tok::is_code_char_pub(ch) {
            code += 1.0;
        }
        if tok::is_cjk_pub(ch) {
            cjk += 1.0;
        }
    }
    let turns = parsed
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|a| a.iter().filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user")).count())
        .unwrap_or(0);
    RequestFeatures {
        est_input_tokens: est,
        user_text_chars: last_user.chars().count(),
        code_density: (code / total).min(1.0),
        tool_count,
        tool_ratio: (tool_count as f32 / 20.0).min(1.0),
        has_images,
        turn_count: turns,
        cjk_ratio: (cjk / total).min(1.0),
    }
}

fn build_digest(parsed: &Value) -> DigestSignals {
    let (_system, last_user, _) = anthropic_text(parsed);
    let first_user = {
        let mut f = String::new();
        if let Some(msgs) = parsed.get("messages").and_then(|m| m.as_array()) {
            for m in msgs {
                if m.get("role").and_then(|r| r.as_str()) == Some("user")
                    && let Some(c) = m.get("content") {
                        let t = match c {
                            Value::String(s) => s.clone(),
                            Value::Array(parts) => parts
                                .iter()
                                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                                .collect::<Vec<_>>()
                                .join("\n"),
                            _ => String::new(),
                        };
                        if !t.is_empty() {
                            f = t;
                            break;
                        }
                    }
            }
        }
        f
    };
    let overlap = overlap_ratio(&last_user, &first_user);
    let has_deixis = ["这个", "那个", "它", "刚才", "继续", "上面", "接着", "再改"]
        .iter()
        .any(|w| last_user.contains(w));
    let topic_shift_marker = ["另外", "顺便", "换个话题", "新任务"]
        .iter()
        .any(|w| last_user.contains(w));
    DigestSignals {
        first_user_text: first_user,
        last_user_text: last_user,
        overlap_ratio: overlap,
        has_deixis,
        topic_shift_marker,
        session_tools_seen: parsed.get("tools").and_then(|t| t.as_array()).map(|a| a.len()).unwrap_or(0),
    }
}

fn returned_tool_result_ids(parsed: &Value) -> Vec<String> {
    parsed
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|msgs| {
            msgs.iter()
                .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
                .filter_map(|m| m.get("content").and_then(|c| c.as_array()))
                .flatten()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                .filter_map(|b| b.get("tool_use_id").and_then(|i| i.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn overlap_ratio(a: &str, b: &str) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let norm = |s: &str| -> std::collections::HashSet<String> {
        s.to_lowercase()
            .split(|c: char| c.is_whitespace() || ",。.;;!?、".contains(c))
            .filter(|w| w.chars().count() > 1)
            .map(|w| w.to_string())
            .collect()
    };
    let (sa, sb) = (norm(a), norm(b));
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let inter = sa.intersection(&sb).count() as f32;
    let union = sa.union(&sb).count() as f32;
    inter / union
}

/// The quality analyzer's IR is openai-shaped; synthesize an equivalent
/// request view from the anthropic body.
fn to_openai_ir_request(parsed: &Value) -> Value {
    let tools: Vec<Value> = parsed
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.get("name").cloned().unwrap_or(json!("")),
                            "parameters": t.get("input_schema").cloned().unwrap_or(json!({})),
                        }
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({ "tools": tools, "messages": parsed.get("messages").cloned().unwrap_or(json!([])) })
}
