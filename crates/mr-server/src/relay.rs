use crate::rewrite::rewrite_model_field;
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use mr_core::engine::RoutingInput;
use mr_core::features as feat;
use mr_core::tokens as tok;
use mr_core::types::*;
use mr_memory::health::{classify_failure, Failure};
use futures::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Instant;

const MAX_ERROR_BODY: usize = 8 * 1024;

/// Health-map key separator: model id + unit-sep + key index.
const KEY_SEP: char = '\u{1f}';

pub async fn chat_completions(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    let agent_hdr = crate::identity::agent_identity(&headers, &st.config.telemetry.agent_header);
    let Ok(parsed) = serde_json::from_slice::<Value>(&body) else {
        return json_error(StatusCode::BAD_REQUEST, "invalid json body");
    };

    let model_field = parsed.get("model").and_then(|v| v.as_str()).unwrap_or("auto").to_string();
    let target = match resolve_target(&st, &model_field) {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    let session_key = session_key_of(&st, &headers, &parsed);
    // 内部状态（粘性/L3 反馈）按 agent 隔离：跨 agent 同名会话绝不共享状态
    let sticky_key = format!("{}\u{1f}{}", agent_hdr, session_key);
    let header_policy = headers
        .get("x-mr-policy")
        .and_then(|v| v.to_str().ok())
        .and_then(PolicyProfile::parse);

    let extracted = feat::extract(&parsed);
    let est = tok::estimate_messages(&extracted.messages, extracted.tools_json.as_deref());
    let features = feat::features(&extracted, est);
    let tools_sig = tools_signature(&parsed);
    let digest = build_digest(&extracted, features.turn_count);

    let mut decision = match &target {
        Target::Auto(alias_policy) => {
            let sticky = st.sessions.get(&sticky_key);
            let health = st.health.snapshot();
            let telemetry = st.flywheel.telemetry_snapshot();
            let quota_view = st.quota.best_remaining_by_models();
            let plan_pressure = st.plan_pressure_map();

            // L3 session-loop: did the previous turn's tool calls come back
            // executed as role=tool messages? (gateway-side semantic signal)
            if let Some(pending) = st.sessions.take_pending(&sticky_key) {
                let ids: Vec<&str> = parsed
                    .get("messages")
                    .and_then(|m| m.as_array())
                    .map(|msgs| {
                        msgs.iter()
                            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
                            .filter_map(|m| m.get("tool_call_id").and_then(|i| i.as_str()))
                            .collect()
                    })
                    .unwrap_or_default();
                let total = pending.call_ids.len() as u64;
                let matched = pending.call_ids.iter().filter(|id| ids.contains(&id.as_str())).count() as u64;
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
                max_output_req: parsed.get("max_tokens").and_then(|v| v.as_u64()),
                policy: header_policy.or(*alias_policy),
                sticky,
                health: &health,
                telemetry: &telemetry,
                quota: &quota_view,
                plan_pressure: &plan_pressure,
            };
            let d = st.engine.decide(input);
            st.sessions.put(
                &sticky_key,
                StickyState {
                    chosen: d.chosen.clone(),
                    est_tokens_band: tok::tokens_band(est),
                    turns_left: st.config.policy.sticky_turns,
                    tools_sig,
                    domain: d.judgment.domain,
                    difficulty: d.difficulty_eff,
                    est_tokens: est,
                },
            );
            if d.sticky {
                st.sessions.decrement_turns(&sticky_key);
            }
            d
        }
        Target::Direct(dec) => dec.clone(),
    };
    decision.est_input_tokens = est;

    let original_choice = decision.chosen.clone();
    let is_stream = parsed.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    // Candidate chain: auto decisions try the ranked chain on quota/auth
    // failures; explicit model selections return the upstream error as-is.
    // Chain exhausted → keep pulling from the full eligible ranking (scores,
    // desc) up to fallback_depth so a top-3 outage never takes the whole
    // provider fleet down with it.
    let attempts: Vec<String> = match &target {
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
            list
        }
        Target::Direct(_) => vec![decision.chosen.clone()],
    };

    let mut skipped: Vec<String> = Vec::new();
    let mut last_error: Option<(StatusCode, Bytes)> = None;
    // context overflow on one candidate means the session needs MORE window:
    // later candidates smaller than the est are pointless, skip them
    let mut min_context_needed: Option<u64> = None;

    // Cross-protocol: OpenAI ingress -> Anthropic upstream (Switchyard IR)
    // provider 账户级熔断：配额/余额/鉴权/限流是 provider 级共享资源，
    // 同 provider 的其他模型不再重复尝试（本请求内）
    let mut dead_providers: std::collections::HashSet<String> = std::collections::HashSet::new();
    for cand in &attempts {
        let Some(record) = st.engine.catalog.get(cand).cloned() else { continue };
        if dead_providers.contains(&record.provider) {
            skipped.push(format!("{cand}(provider {} account-level failure)", record.provider));
            continue;
        }
        if let Some(need) = min_context_needed
            && record.context_window.map(|w| w < need).unwrap_or(true)
        {
            skipped.push(format!("{cand}(window < {need})"));
            continue;
        }
        let to_anthropic = record.protocol == Protocol::Anthropic;
        let keys = record.key_values();
        let key_len = keys.len().max(1);
        let key_start = st.key_start(cand, key_len);
        let health_snap = st.health.snapshot();
        let now_ms = mr_memory::health::now();

        // multi-key pool: try each key in rotation before falling to the
        // next candidate (per model×key health bookkeeping)
        for key_off in 0..key_len {
        let key_idx = (key_start + key_off) % key_len;
        let key_value = keys.get(key_idx).cloned();
        let health_id = if keys.is_empty() {
            cand.clone()
        } else {
            format!("{cand}{KEY_SEP}{key_idx}")
        };
        if let Some(h) = health_snap.get(&health_id)
            && !h.available(now_ms) {
                let remaining = h
                    .cooldown_remaining_ms(now_ms)
                    .map(|ms| format!(" for {}s", ms / 1000))
                    .unwrap_or_default();
                skipped.push(format!("{cand}[{key_idx}]({}){remaining}", h.kind.label()));
                continue;
            }
        let (fwd_body, url) = if to_anthropic {
            let mut openai_value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if let Some(obj) = openai_value.as_object_mut() {
                obj.insert("model".into(), json!(record.upstream_model));
                obj.entry("max_tokens").or_insert(json!(4096));
            }
            let translated = crate::translate::Translator::global()
                .request_openai_to_anthropic(&openai_value);
            match translated {
                Ok(v) => (v.to_string().into_bytes(), format!("{}/messages", record.base_url)),
                Err(e) => {
                    tracing::warn!(model = %cand, error = %e, "openai->anthropic translation failed");
                    skipped.push(format!("{cand}(translate)"));
                    last_error = Some((StatusCode::BAD_GATEWAY, Bytes::new()));
                    continue;
                }
            }
        } else {
            let rewritten = rewrite_model_field(&body, &record.upstream_model);
            (rewritten, format!("{}/chat/completions", record.base_url))
        };
        let cross = to_anthropic;

        let mut req = st.http.post(&url).header("content-type", "application/json");
        if to_anthropic {
            req = req.header("anthropic-version", "2023-06-01");
            if let Some(key) = key_value.as_deref() {
                req = req.header("x-api-key", key);
            }
        } else {
            // request shaping: opencode zen upstreams require a session header
            if record.base_url.contains("opencode.ai/zen") {
                req = req.header("x-opencode-session", &session_key);
            }
            if let Some(key) = key_value.as_deref() {
                req = req.bearer_auth(key);
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
                skipped.push(format!("{cand}(transport)"));
                last_error = Some((StatusCode::BAD_GATEWAY, Bytes::new()));
                continue;
            }
        };

        let status = resp.status();

        if fallback_eligible(status.as_u16()) {
            // 重置时间提取：retry-after(秒) 优先，回退 x-ratelimit-reset-*（纪元 ms 或时长 ms）
            let retry_after_ms = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|s| s * 1000)
                .or_else(|| {
                    ["x-ratelimit-reset-requests", "x-ratelimit-reset-tokens", "x-ratelimit-reset"]
                        .iter()
                        .find_map(|h| resp.headers().get(*h))
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(|n| {
                            if n > 1_000_000_000_000 {
                                n.saturating_sub(mr_memory::health::now())
                            } else {
                                n
                            }
                        })
                });
            let err_body = resp.bytes().await.unwrap_or_default();
            let snippet =
                String::from_utf8_lossy(&err_body.slice(..err_body.len().min(MAX_ERROR_BODY))).into_owned();
            let failure = classify_failure(status.as_u16(), &snippet, retry_after_ms);
            tracing::warn!(model = %cand, status = status.as_u16(), kind = failure.kind.label(), "upstream failure, marking health");
            st.events.record(json!({
                "kind": "upstream_error",
                "model": cand,
                "upstream_model": record.upstream_model,
                "status": status.as_u16(),
                "health": failure.kind.label(),
                "message": failure.message,
                "session": session_key,
            }));
            let health_id = if keys.is_empty() {
                cand.clone()
            } else {
                format!("{cand}{KEY_SEP}{key_idx}")
            };
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

        // quota window learning from success headers (before body consumed)
        let windows = mr_memory::QuotaLedger::parse_headers(resp.headers());
        st.quota.observe(&format!("{cand}{KEY_SEP}{key_idx}"), windows);
        // success path
        let success_health_id = if keys.is_empty() {
            cand.clone()
        } else {
            format!("{cand}{KEY_SEP}{key_idx}")
        };
        st.health.mark_ok(&success_health_id);
        decision.chosen = cand.clone();
        decision.upstream_model = record.upstream_model.clone();
        decision.id = format!("{}-{}", decision.id, skipped.len());

        // 响应头到达时刻 = 非流式 TTFT / 流式首块前基准
        let head_ms = started.elapsed().as_millis() as u64;
        let telem = std::sync::Arc::new(std::sync::Mutex::new(crate::stream::Telemetry {
            decision_id: decision.id.clone(),
            session: session_key.clone(),
            chosen: decision.chosen.clone(),
            upstream_model: record.upstream_model.clone(),
            est_tokens: est,
            sticky: decision.sticky,
            stream: is_stream,
            started,
            ttft_ms: Some(head_ms as u128),
            bytes: 0,
            status: status.as_u16(),
            usage: None,
            est_cost_usd: None,
            translated: cross.then(|| "anthropic->openai".to_string()),
            agent: Some(agent_hdr),
            plan_key: mr_core::plans::plan_key_for(&record.base_url).map(|k| k.to_string()),
            extra: {
                let mut ex = json!({
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
                });
                if cross {
                    ex["translated"] = json!("anthropic->openai");
                }
                Some(ex)
            },
        }));

        let mut out = Response::builder().status(map_status(status));
        let h = out.headers_mut().unwrap();
        append_decision_headers(h, &decision, est);
        if !skipped.is_empty() {
            insert_header(h, "x-mr-fallback-from", &original_choice);
            insert_header(h, "x-mr-skipped", &skipped.join(","));
        }

        if !status.is_success() {
            let err_body = resp.bytes().await.unwrap_or_default();
            crate::stream::finalize_event(&st.events, &st.flywheel.clone(), &st.quota, &st.bus, &telem.lock().unwrap());
            insert_header(h, "x-mr-upstream-status", status.as_str());
            return out.body(axum::body::Body::from(err_body)).unwrap();
        }

        if is_stream {
            let ct = resp
                .headers()
                .get("content-type")
                .cloned()
                .unwrap_or_else(|| "text/event-stream".parse().unwrap());
            h.insert("content-type", ct);
            if cross {
                // telemetry sees the RAW anthropic stream (parser has an
                // anthropic mode); translation wraps telemetry output so the
                // client still receives openai chunks. Order matters: parse
                // pre-translation or quality/usage signals are lost.
                let telem_body = crate::stream::telemetry_body(
                    resp.bytes_stream(),
                    telem,
                    st.events.clone(),
                    st.flywheel.clone(),
                    st.sessions.clone(),
                    st.bus.clone(),
                    parsed.clone(),
                    true,
                );
                let translated =
                    crate::translate::AnthropicToOpenaiStream::new(telem_body).boxed();
                return out.body(axum::body::Body::from_stream(translated)).unwrap();
            }
            let stream = crate::stream::telemetry_body(
                resp.bytes_stream(),
                telem,
                st.events.clone(),
                st.flywheel.clone(),
                st.sessions.clone(),
                st.bus.clone(),
                parsed.clone(),
                false,
            );
            return out.body(axum::body::Body::from_stream(stream)).unwrap();
        }

        let bytes = resp.bytes().await.unwrap_or_default();
        let mut response_value: Option<Value> = serde_json::from_slice::<Value>(&bytes).ok();
        if cross {
            // anthropic -> openai response shape for the caller; quality
            // analysis and pending tool-call extraction run on openai form
            if let Some(v) = &response_value {
                match crate::translate::Translator::global().response_anthropic_to_openai(v) {
                    Ok(openai_v) => {
                        response_value = Some(openai_v);
                        insert_header(h, "x-mr-translated", "anthropic->openai");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "anthropic->openai response translation failed");
                    }
                }
            }
        }
        {
            let Ok(mut t) = telem.lock() else {
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, "telemetry lock poisoned");
            };
            t.bytes = bytes.len() as u64;
            if t.ttft_ms.is_none() {
                t.ttft_ms = Some(started.elapsed().as_millis());
            }
            if let Some(v) = &response_value {
                if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                    t.usage = Some(u.clone());
                }
                if let (Some(cost), Some(u)) = (record.cost, t.usage.as_ref()) {
                    let pt = u.get("prompt_tokens").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    let ct = u.get("completion_tokens").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    t.est_cost_usd =
                        Some((pt / 1e6) * cost.input as f64 + (ct / 1e6) * cost.output as f64);
                }
                let q = crate::quality::analyze_response(&parsed, v);
                if let Some(ex) = t.extra.as_mut() {
                    ex["quality"] = serde_json::to_value(&q).unwrap_or_default();
                } else {
                    t.extra = Some(json!({"quality": q}));
                }
                let ids: Vec<String> = v
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .and_then(|c| c.first())
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("tool_calls"))
                    .and_then(|t| t.as_array())
                    .map(|tcs| {
                        tcs.iter()
                            .filter_map(|tc| {
                                tc.get("id").and_then(|i| i.as_str()).map(|s| s.to_string())
                            })
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                let chosen = t.chosen.clone();
                st.sessions.set_pending(&sticky_key, &chosen, ids);
            }
        } // guard dropped before finalize (which locks telem itself)

        if let Some(v) = &response_value {
            crate::stream::finalize_event(&st.events, &st.flywheel.clone(), &st.quota, &st.bus, &telem.lock().unwrap());
            h.insert("content-type", "application/json".parse().unwrap());
            let body_bytes = serde_json::to_vec(v).unwrap_or_default();
            return out.body(axum::body::Body::from(body_bytes)).unwrap();
        }
        crate::stream::finalize_event(&st.events, &st.flywheel.clone(), &st.quota, &st.bus, &telem.lock().unwrap());
        h.insert("content-type", "application/json".parse().unwrap());
        return out.body(axum::body::Body::from(bytes)).unwrap();
        } // key_off loop (keys exhausted for this candidate)
    } // candidate loop

    // every candidate failed: explicit selections get the raw upstream
    // error back; auto routing gets a summary of what was skipped and why
    // 全灭也落事件（含 x-mr-skipped 明细），否则面板回看不到这次失败
    {
        let status = last_error.as_ref().map(|(s, _)| s.as_u16()).unwrap_or(502);
        let filtered_note: Vec<String> = decision
            .filtered
            .iter()
            .take(4)
            .map(|f| format!("{} ({})", f.model, f.cause))
            .collect();
        let mut detail = if skipped.is_empty() {
            "all routed upstreams failed".to_string()
        } else {
            format!("all routed upstreams failed: {}", skipped.join("; "))
        };
        if !filtered_note.is_empty() {
            detail.push_str(&format!(" | filtered: {}", filtered_note.join("; ")));
        }
        let telem = crate::stream::Telemetry {
            decision_id: decision.id.clone(),
            session: session_key.clone(),
            chosen: original_choice.clone(),
            upstream_model: String::new(),
            est_tokens: decision.est_input_tokens,
            sticky: decision.sticky,
            stream: false,
            started,
            ttft_ms: None,
            bytes: 0,
            status,
            usage: None,
            est_cost_usd: None,
            translated: None,
            extra: Some(json!({"reason": detail, "skipped": skipped, "filtered": decision.filtered})),
            agent: Some(crate::identity::agent_identity(&headers, &st.config.telemetry.agent_header)),
            plan_key: None,
        };
        crate::stream::finalize_event(&st.events, &st.flywheel.clone(), &st.quota, &st.bus, &telem);
    }
    let direct_raw = matches!(target, Target::Direct(_)) && last_error.is_some();
    let (status, err_body) = last_error.unwrap_or((StatusCode::BAD_GATEWAY, Bytes::new()));
    let mut resp = if direct_raw {
        let mut r = Response::builder().status(status)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(err_body))
            .unwrap();
        insert_header(r.headers_mut(), "x-mr-model", &original_choice);
        insert_header(r.headers_mut(), "x-mr-decision-id", &decision.id);
        r
    } else {
        json_error(status, "all routed upstreams failed; see x-mr-skipped for per-model reasons")
    };
    // 引导调用方退避：按链上模型已知的最短冷却给 Retry-After，
    // 让 opencode 等客户端按此退避而不是立即重撞
    {
        let snap = st.health.snapshot();
        let now_ms = mr_memory::health::now();
        let mut ra: Option<u64> = None;
        for cand in &attempts {
            for (k, h) in &snap {
                if k.split(KEY_SEP).next() == Some(cand.as_str())
                    && let Some(ms) = h.cooldown_remaining_ms(now_ms)
                {
                    ra = Some(ra.map_or(ms, |x: u64| x.min(ms)));
                }
            }
        }
        if let Some(secs) = ra {
            insert_header(resp.headers_mut(), "retry-after", &(secs.div_ceil(1000)).to_string());
        }
    }
    if !skipped.is_empty() {
        insert_header(resp.headers_mut(), "x-mr-skipped", &skipped.join(","));
    }
    resp
}

fn fallback_eligible(status: u16) -> bool {
    matches!(status, 400 | 401 | 402 | 403 | 404 | 408 | 429) || status >= 500
}

fn insert_header(h: &mut axum::http::HeaderMap, k: &'static str, v: &str) {
    if let Ok(val) = axum::http::HeaderValue::from_str(&v.chars().map(|c| if c.is_ascii() { c } else { ' ' }).collect::<String>()) {
        h.insert(k, val);
    }
}

#[allow(clippy::large_enum_variant)]
enum Target {
    Auto(Option<PolicyProfile>),
    Direct(Decision),
}

#[allow(clippy::result_large_err)]
fn resolve_target(st: &AppState, model_field: &str) -> Result<Target, Response> {
    if let Some(alias) = model_field.strip_prefix("auto:") {
        let policy = PolicyProfile::parse(alias).ok_or_else(|| {
            json_error(StatusCode::NOT_FOUND, &format!("unknown policy alias '{alias}'"))
        })?;
        return Ok(Target::Auto(Some(policy)));
    }
    if model_field == "auto" {
        return Ok(Target::Auto(None));
    }
    if let Some(m) = st.engine.catalog.get(model_field) {
        let dec = Decision {
            id: format!("direct-{}", mr_memory::now_millis().unwrap_or(0)),
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
            sticky: false,
            est_input_tokens: 0,
            difficulty_eff: 0.0,
        };
        return Ok(Target::Direct(dec));
    }
    Err(json_error(
        StatusCode::NOT_FOUND,
        &format!("model '{model_field}' is neither 'auto' nor a configured catalog id"),
    ))
}

fn build_digest(extracted: &feat::Extracted, _turns: usize) -> DigestSignals {
    let overlap = overlap_ratio(&extracted.last_user_text, &extracted.first_user_text);
    let has_deixis = ["这个", "那个", "它", "刚才", "继续", "上面", "接着", "再改"]
        .iter()
        .any(|w| extracted.last_user_text.contains(w));
    let topic_shift_marker = ["另外", "顺便", "换个话题", "新任务"]
        .iter()
        .any(|w| extracted.last_user_text.contains(w));
    DigestSignals {
        first_user_text: extracted.first_user_text.clone(),
        last_user_text: extracted.last_user_text.clone(),
        overlap_ratio: overlap,
        has_deixis,
        topic_shift_marker,
        session_tools_seen: extracted.tool_count,
    }
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

fn tools_signature(parsed: &Value) -> u64 {
    let tools = parsed.get("tools").map(|t| t.to_string()).unwrap_or_default();
    let hash = Sha256::digest(tools.as_bytes());
    u64::from_be_bytes(hash[0..8].try_into().unwrap())
}

fn session_key_of(st: &AppState, headers: &HeaderMap, parsed: &Value) -> String {
    if let Some(v) = crate::identity::session_identity(headers, parsed, &st.config.telemetry.session_header) {
        return v;
    }
    let mut hasher = Sha256::new();
    if let Some(msgs) = parsed.get("messages").and_then(|v| v.as_array()) {
        if let Some(first) = msgs.first() {
            hasher.update(first.to_string().as_bytes());
        }
        if let Some(sys) = msgs.iter().find(|m| m.get("role").and_then(|r| r.as_str()) == Some("system")) {
            hasher.update(sys.to_string().as_bytes());
        }
    }
    format!("h{}", hex::encode(&hasher.finalize()[..8]))
}

fn append_decision_headers(h: &mut axum::http::HeaderMap, decision: &Decision, est: u64) {
    insert_header(h, "x-mr-model", &decision.chosen);
    insert_header(h, "x-mr-decision-id", &decision.id);
    insert_header(h, "x-mr-sticky", &decision.sticky.to_string());
    insert_header(h, "x-mr-est-tokens", &est.to_string());
    insert_header(h, "x-mr-reason", &decision.header_reason());
}

fn map_status(s: reqwest::StatusCode) -> StatusCode {
    StatusCode::from_u16(s.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY)
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    (
        status,
        axum::Json(json!({
            "error": { "message": msg, "type": "modelroute_error" }
        })),
    )
        .into_response()
}
