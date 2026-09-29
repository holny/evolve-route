use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mr_core::config::{BenchmarksCfg, CatalogCfg, DataCfg, DecisionCfg, DiscoveryCfg, FileConfig, ModelEntry, PolicyCfg, QuotaCfg, ServerCfg};
use mr_core::types::{Cost, Tiers};
use mr_server::state::{build_router, build_state};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn spawn_mock_upstream() -> u16 {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(|body: axum::body::Bytes| async move {
            let v: Value = serde_json::from_slice(&body).unwrap();
            let model = v["model"].as_str().unwrap_or("unknown").to_string();
            if v["stream"].as_bool().unwrap_or(false) {
                let sse = format!(
                    "data: {{\"id\":\"1\",\"model\":\"{model}\",\"choices\":[{{\"delta\":{{\"content\":\"hi\"}}}}]}}\n\n\
                     data: {{\"id\":\"1\",\"model\":\"{model}\",\"choices\":[],\"usage\":{{\"prompt_tokens\":123,\"completion_tokens\":45}}}}\n\n\
                     data: [DONE]\n\n"
                );
                axum::http::Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(Body::from(sse))
                    .unwrap()
            } else {
                axum::http::Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model": model, "choices": [{"message": {"role": "assistant", "content": "ok"}}],
                               "usage": {"prompt_tokens": 123, "completion_tokens": 45}})
                            .to_string(),
                    ))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    port
}

fn test_config(upstream_port: u16) -> FileConfig {
    let mk = |id: &str, upstream: &str, window: u64, inp: f32, outp: f32, coding: f32, speed: f32| ModelEntry {
        id: id.into(),
        provider: "mock".into(),
        base_url: format!("http://127.0.0.1:{upstream_port}/v1"),
        api_key_env: None,
        upstream_model: Some(upstream.into()),
        context_window: Some(window),
        max_output: 4096,
        cost: Some(Cost { input: inp, output: outp }),
        tiers: Tiers { reasoning: coding * 0.9, coding, vision: 0.0, agentic: coding },
        speed_tier: speed,
        source_note: None,
        ..Default::default()
    };
    FileConfig {
        server: ServerCfg { host: "127.0.0.1".into(), port: 0 },
        policy: PolicyCfg::default(),
        decision: DecisionCfg { backend: "heuristic".into(), redact: true },
        catalog: CatalogCfg::default(),
        quota: QuotaCfg::default(),
        data: DataCfg { dir: std::env::temp_dir().join(format!("mr-test-{}", std::process::id())) .to_string_lossy().into_owned() },
        discovery: DiscoveryCfg { agents: vec![] },
        benchmarks: BenchmarksCfg { enabled: false, interval_hours: 24, sources: vec![] },
        models: vec![
            mk("mini", "mock-mini", 32_000, 0.1, 0.4, 0.45, 0.95),
            mk("standard", "mock-standard", 128_000, 0.6, 2.4, 0.75, 0.7),
            mk("frontier", "mock-frontier", 200_000, 3.0, 15.0, 0.95, 0.4),
        ],
    }
}

fn chat_request(model: &str, content: Value, extra: Value) -> Request<Body> {
    let mut body = json!({"model": model, "messages": [{"role": "user", "content": content}]});
    if let (Some(obj), Some(ext)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in ext {
            obj.insert(k.clone(), v.clone());
        }
    }
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("x-mr-session", "test-session-1")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, bytes::Bytes) {
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes)
}

fn json_body(bytes: &bytes::Bytes) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

#[tokio::test]
async fn trivial_message_routes_to_cheapest() {
    let port = spawn_mock_upstream().await;
    let app = build_router(build_state(test_config(port)));
    let (status, headers, raw) = send(app, chat_request("auto", json!("你好"), json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-mr-model"], "mini");
    assert_eq!(json_body(&raw)["model"], "mock-mini", "upstream must receive the routed model id");
}

#[tokio::test]
async fn oversized_context_routes_to_only_fitting_model() {
    let port = spawn_mock_upstream().await;
    let app = build_router(build_state(test_config(port)));
    let big = "a".repeat(500_000); // ~125k tokens latin
    let (status, headers, raw) = send(app, chat_request("auto", json!(big), json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-mr-model"], "frontier");
    assert_eq!(json_body(&raw)["model"], "mock-frontier");
}

#[tokio::test]
async fn explicit_model_bypasses_routing() {
    let port = spawn_mock_upstream().await;
    let app = build_router(build_state(test_config(port)));
    let (_, headers, raw) = send(app, chat_request("standard", json!("你好"), json!({}))).await;
    assert_eq!(headers["x-mr-model"], "standard");
    assert_eq!(json_body(&raw)["model"], "mock-standard");
    assert!(headers["x-mr-decision-id"].to_str().unwrap().starts_with("direct"));
}

#[tokio::test]
async fn streaming_passthrough_relays_sse() {
    let port = spawn_mock_upstream().await;
    let app = build_router(build_state(test_config(port)));
    let (status, headers, raw) =
        send(app, chat_request("auto", json!("你好"), json!({"stream": true}))).await;
    assert_eq!(status, StatusCode::OK, "streaming status; body={}", String::from_utf8_lossy(&raw));
    assert_eq!(headers["x-mr-model"], "mini");
    let text = String::from_utf8_lossy(&raw).into_owned();
    assert!(text.contains("data: {"), "sse passthrough, got: {text}");
    assert!(text.contains("mock-mini"));
    assert!(text.contains("[DONE]"));
}

#[tokio::test]
async fn unknown_model_is_404() {
    let port = spawn_mock_upstream().await;
    let app = build_router(build_state(test_config(port)));
    let (status, _, _) = send(app, chat_request("no-such-model", json!("hi"), json!({}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn code_refactor_does_not_land_on_mini() {
    let port = spawn_mock_upstream().await;
    let app = build_router(build_state(test_config(port)));
    let text = "重构这个模块的错误处理，把 unwrap 全部换成 thiserror，然后补测试，先梳理类型再逐个文件改";
    let (_, headers, _) = send(app, chat_request("auto", json!(text), json!({}))).await;
    assert_ne!(headers["x-mr-model"], "mini");
}

#[tokio::test]
async fn sticky_session_reuses_model() {
    let port = spawn_mock_upstream().await;
    let app = build_router(build_state(test_config(port)));
    let (_, h1, _) = send(app.clone(), chat_request("auto", json!("你好"), json!({}))).await;
    let (_, h2, _) = send(app, chat_request("auto", json!("谢谢"), json!({}))).await;
    assert_eq!(h1["x-mr-model"], "mini");
    assert_eq!(h2["x-mr-model"], "mini");
    assert_eq!(h2["x-mr-sticky"], "true", "second trivial turn in same session should reuse");
}

#[tokio::test]
async fn models_endpoint_lists_auto_and_catalog_with_source() {
    let port = spawn_mock_upstream().await;
    let app = build_router(build_state(test_config(port)));
    let req = Request::builder().method("GET").uri("/v1/models").body(Body::empty()).unwrap();
    let (status, _, raw) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    let body = json_body(&raw);
    let ids: Vec<&str> = body["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"auto"));
    assert!(ids.contains(&"mini"));
    let mini = body["data"].as_array().unwrap().iter().find(|m| m["id"] == "mini").unwrap();
    assert_eq!(mini["source"], "user");
}

async fn spawn_picky_upstream(dead_model: &str, dead_status: u16, dead_body: &'static str) -> u16 {
    let dead = dead_model.to_string();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |body: axum::body::Bytes| {
            let dead = dead.clone();
            async move {
                let v: Value = serde_json::from_slice(&body).unwrap();
                let model = v["model"].as_str().unwrap_or("unknown").to_string();
                if model == dead {
                    return axum::http::Response::builder()
                        .status(dead_status)
                        .header("content-type", "application/json")
                        .body(Body::from(dead_body))
                        .unwrap();
                }
                axum::http::Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model": model, "choices": [{"message": {"role": "assistant", "content": "ok"}}],
                               "usage": {"prompt_tokens": 5, "completion_tokens": 1}})
                            .to_string(),
                    ))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    port
}

#[tokio::test]
async fn no_credit_model_is_skipped_and_marked() {
    let port = spawn_picky_upstream("mock-mini", 402, r#"{"error":{"message":"Insufficient Balance"}}"#).await;
    let mut cfg = test_config(port);
    cfg.data.dir = std::env::temp_dir().join(format!("mr-test-402-{}", std::process::id())).to_string_lossy().into_owned();
    let app = build_router(build_state(cfg));

    // trivial request routes to mini first -> 402 -> falls back to standard
    let (status, headers, raw) = send(app.clone(), chat_request("auto", json!("你好"), json!({}))).await;
    assert_eq!(status, StatusCode::OK, "body: {}", String::from_utf8_lossy(&raw));
    assert_eq!(headers["x-mr-model"], "standard", "mini dead, must fall back");
    let skipped = headers["x-mr-skipped"].to_str().unwrap();
    assert!(skipped.contains("mini"), "skipped: {skipped}");
    assert!(skipped.contains("no credit"), "skipped: {skipped}");
    assert_eq!(headers["x-mr-fallback-from"], "mini");

    // health registry now knows mini is dead
    let req = Request::builder().method("GET").uri("/api/health").body(Body::empty()).unwrap();
    let (hstatus, _, raw) = send(app.clone(), req).await;
    assert_eq!(hstatus, StatusCode::OK);
    let v = json_body(&raw);
    assert_eq!(v["models"]["mini"]["kind"], "payment_required");
    assert_eq!(v["models"]["mini"]["available"], false);

    // second request: mini is pre-filtered by health, straight to standard, no wasted attempt
    let (_, headers, _) = send(app, chat_request("auto", json!("你好"), json!({}))).await;
    assert_eq!(headers["x-mr-model"], "standard");
    assert!(headers.get("x-mr-skipped").is_none(), "no retry waste on second request");
}

#[tokio::test]
async fn plugin_feedback_updates_flywheel_reliability_inputs() {
    let port = spawn_mock_upstream().await;
    let mut cfg = test_config(port);
    cfg.data.dir = std::env::temp_dir().join(format!("mr-test-fb-{}", std::process::id())).to_string_lossy().into_owned();
    let app = build_router(build_state(cfg));

    // sticky session on mini
    let (_, h1, _) = send(app.clone(), chat_request("auto", json!("你好"), json!({}))).await;
    assert_eq!(h1["x-mr-model"], "mini");

    // plugin reports a tool failure without knowing the model (session-resolved)
    let req = Request::builder()
        .method("POST")
        .uri("/api/feedback")
        .header("content-type", "application/json")
        .body(Body::from(json!({"session": "test-session-1", "ok": false, "tool": "bash", "detail": "boom"}).to_string()))
        .unwrap();
    let (status, _, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // bad request: no session, no model
    let req = Request::builder()
        .method("POST")
        .uri("/api/feedback")
        .header("content-type", "application/json")
        .body(Body::from(json!({"ok": true}).to_string()))
        .unwrap();
    let (status, _, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // stats expose feedback counters
    let req = Request::builder().method("GET").uri("/api/stats").body(Body::empty()).unwrap();
    let (_, _, raw) = send(app, req).await;
    let v = json_body(&raw);
    assert_eq!(v["models"]["mini"]["feedback"]["total"], 1);
    assert_eq!(v["models"]["mini"]["feedback"]["ok"], 0);
}

async fn spawn_anthropic_mock() -> u16 {
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(|headers: axum::http::HeaderMap, body: axum::body::Bytes| async move {
            assert_eq!(headers.get("x-api-key").and_then(|k| k.to_str().ok()), Some("ak-test"), "anthropic auth header");
            assert!(headers.contains_key("anthropic-version"));
            let v: Value = serde_json::from_slice(&body).unwrap();
            assert!(v.get("max_tokens").is_some(), "anthropic requires max_tokens");
            let model = v["model"].as_str().unwrap_or("unknown").to_string();
            axum::http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "id": "msg_1", "type": "message", "role": "assistant",
                        "model": model,
                        "content": [{"type": "text", "text": "claude says hi"}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 12, "output_tokens": 4}
                    })
                    .to_string(),
                ))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    port
}

#[tokio::test]
async fn openai_ingress_routes_to_anthropic_upstream_translated() {
    // edition 2024: env::set_var is unsafe outside unsafe blocks; run once here
    unsafe { std::env::set_var("MR_TEST_ANTHROPIC_KEY", "ak-test") };
    let port = spawn_anthropic_mock().await;
    let mut cfg = test_config(port);
    cfg.data.dir = std::env::temp_dir().join(format!("mr-test-ax-{}", std::process::id())).to_string_lossy().into_owned();
    // only an anthropic-protocol model in the catalog
    cfg.models = vec![ModelEntry {
        id: "claude".into(),
        provider: "anthropic".into(),
        protocol: mr_core::types::Protocol::Anthropic,
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key_env: Some("MR_TEST_ANTHROPIC_KEY".into()),
        api_keys_env: None,
        upstream_model: Some("claude-sonnet".into()),
        context_window: Some(200_000),
        max_output: 4096,
        cost: Some(Cost { input: 3.0, output: 15.0 }),
        tiers: Tiers { reasoning: 0.9, coding: 0.95, vision: 0.9, agentic: 0.95 },
        speed_tier: 0.65,
        weight: None,
        source_note: None,
    }];
    let app = build_router(build_state(cfg));

    let (status, headers, raw) = send(
        app,
        chat_request("auto", json!("你好，帮我看看这个类"), json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {}", String::from_utf8_lossy(&raw));
    assert_eq!(headers["x-mr-model"], "claude");
    let v = json_body(&raw);
    // caller speaks OpenAI: response must be openai-shaped
    assert!(v["choices"][0]["message"]["content"].as_str().unwrap_or("").contains("claude says hi"), "got: {v}");
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert!(v.get("usage").is_some(), "usage must survive round-trip");
}

async fn spawn_anthropic_relay_mock() -> u16 {
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(|headers: axum::http::HeaderMap, body: axum::body::Bytes| async move {
            assert_eq!(headers.get("x-api-key").and_then(|k| k.to_str().ok()), Some("ak-live"));
            let v: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v["model"], "claude-sonnet", "model must be surgically rewritten");
            assert!(v.get("max_tokens").is_some());
            axum::http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "id": "msg_2", "type": "message", "role": "assistant",
                        "model": v["model"],
                        "content": [{"type": "text", "text": "好的，我来处理"}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 20, "output_tokens": 6}
                    })
                    .to_string(),
                ))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    port
}

#[tokio::test]
async fn anthropic_ingress_same_protocol_relays() {
    unsafe { std::env::set_var("MR_TEST_ANTHROPIC_KEY2", "ak-live") };
    let port = spawn_anthropic_relay_mock().await;
    let mut cfg = test_config(port);
    cfg.data.dir = std::env::temp_dir().join(format!("mr-test-am-{}", std::process::id())).to_string_lossy().into_owned();
    cfg.models = vec![ModelEntry {
        id: "claude".into(),
        provider: "anthropic".into(),
        protocol: mr_core::types::Protocol::Anthropic,
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key_env: Some("MR_TEST_ANTHROPIC_KEY2".into()),
        api_keys_env: None,
        upstream_model: Some("claude-sonnet".into()),
        context_window: Some(200_000),
        max_output: 4096,
        cost: Some(Cost { input: 3.0, output: 15.0 }),
        tiers: Tiers { reasoning: 0.9, coding: 0.95, vision: 0.9, agentic: 0.95 },
        speed_tier: 0.65,
        weight: None,
        source_note: None,
    }];
    let app = build_router(build_state(cfg));

    // claude-code style request: system top-level, max_tokens mandatory
    let body = json!({
        "model": "auto",
        "max_tokens": 512,
        "system": "You are a coding assistant.",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "你好，帮我看下这个模块"}]}]
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .header("x-mr-session", "cc-1")
        .header("anthropic-version", "2023-06-01")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, headers, raw) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK, "body: {}", String::from_utf8_lossy(&raw));
    assert_eq!(headers["x-mr-model"], "claude");
    let v = json_body(&raw);
    assert_eq!(v["type"], "message");
    assert!(v["content"][0]["text"].as_str().unwrap_or("").contains("我来处理"), "anthropic shape preserved: {v}");
    assert_eq!(v["usage"]["input_tokens"], 20);

    // count_tokens endpoint estimates locally
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, _, raw) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    let v = json_body(&raw);
    let n = v["input_tokens"].as_u64().unwrap();
    assert!(n > 0 && n < 1000, "estimate: {n}");
}

async fn spawn_anthropic_stream_mock() -> u16 {
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(|body: axum::body::Bytes| async move {
            let v: Value = serde_json::from_slice(&body).unwrap();
            let model = v["model"].as_str().unwrap_or("unknown").to_string();
            let sse = format!(
                "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"m1\",\"model\":\"{model}\",\"role\":\"assistant\",\"content\":[],\"usage\":{{\"input_tokens\":9}}}}}}\n\n\
                 event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
                 event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"hello from claude\"}}}}\n\n\
                 event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
                 event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":4}}}}\n\n\
                 event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
            );
            axum::http::Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(sse))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    port
}

#[tokio::test]
async fn openai_streaming_to_anthropic_upstream_translated() {
    unsafe { std::env::set_var("MR_TEST_ANTHROPIC_KEY3", "ak-s") };
    let port = spawn_anthropic_stream_mock().await;
    let mut cfg = test_config(port);
    cfg.data.dir = std::env::temp_dir().join(format!("mr-test-axs-{}", std::process::id())).to_string_lossy().into_owned();
    cfg.models = vec![ModelEntry {
        id: "claude".into(),
        provider: "anthropic".into(),
        protocol: mr_core::types::Protocol::Anthropic,
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key_env: Some("MR_TEST_ANTHROPIC_KEY3".into()),
        api_keys_env: None,
        upstream_model: Some("claude-sonnet".into()),
        context_window: Some(200_000),
        max_output: 4096,
        cost: Some(Cost { input: 3.0, output: 15.0 }),
        tiers: Tiers { reasoning: 0.9, coding: 0.95, vision: 0.9, agentic: 0.95 },
        speed_tier: 0.65,
        weight: None,
        source_note: None,
    }];
    let app = build_router(build_state(cfg));
    let (status, headers, raw) = send(
        app,
        chat_request("auto", json!("打个招呼"), json!({"stream": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-mr-model"], "claude");
    let text = String::from_utf8_lossy(&raw);
    // caller speaks OpenAI SSE: chat.completion.chunk deltas + [DONE]
    assert!(text.contains("chat.completion.chunk"), "openai chunks: {text}");
    assert!(text.contains("hello from claude"), "content mapped: {text}");
    assert!(text.contains("finish_reason"), "finish mapped");
    assert!(text.contains("data: [DONE]"), "terminated: {text}");
}

async fn spawn_key_pool_mock() -> u16 {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(|headers: axum::http::HeaderMap, body: axum::body::Bytes| async move {
            let auth = headers.get("authorization").and_then(|a| a.to_str().ok()).unwrap_or("");
            if auth.contains("sk-dead") {
                return axum::http::Response::builder()
                    .status(402)
                    .body(Body::from(r#"{"error":{"message":"Insufficient Balance"}}"#))
                    .unwrap();
            }
            let v: Value = serde_json::from_slice(&body).unwrap();
            let model = v["model"].as_str().unwrap_or("?").to_string();
            axum::http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model": model, "choices": [{"message": {"role": "assistant", "content": format!("ok with {auth}")}}],
                           "usage": {"prompt_tokens": 3, "completion_tokens": 1}})
                        .to_string(),
                ))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    port
}

#[tokio::test]
async fn dead_key_rotates_within_pool() {
    unsafe { std::env::set_var("MR_TEST_POOL_KEY1", "sk-dead") };
    unsafe { std::env::set_var("MR_TEST_POOL_KEY2", "sk-alive") };
    let port = spawn_key_pool_mock().await;
    let mut cfg = test_config(port);
    cfg.data.dir = std::env::temp_dir().join(format!("mr-test-pool-{}", std::process::id())).to_string_lossy().into_owned();
    cfg.models[0].api_key_env = None;
    cfg.models[0].api_keys_env = Some(vec!["MR_TEST_POOL_KEY1".into(), "MR_TEST_POOL_KEY2".into()]);
    let app = build_router(build_state(cfg));

    let (status, headers, raw) = send(app.clone(), chat_request("auto", json!("你好"), json!({}))).await;
    assert_eq!(status, StatusCode::OK, "body: {}", String::from_utf8_lossy(&raw));
    assert_eq!(headers["x-mr-model"], "mini");
    let skipped = headers["x-mr-skipped"].to_str().unwrap_or("");
    assert!(skipped.contains("[0]") && skipped.contains("no credit"), "dead key skipped first: {skipped}");

    // health ledger marks model×key0 dead, key1 alive
    let req = Request::builder().method("GET").uri("/api/health").body(Body::empty()).unwrap();
    let (_, _, raw) = send(app, req).await;
    let v = json_body(&raw);
    let keys = v["models"].as_object().unwrap();
    let _dead_id = keys.keys().find(|k| k.contains('\u{1f}') && k.ends_with('\u{0}')).cloned();
    let mini_entries: Vec<&Value> = keys.iter().filter(|(k, _)| k.starts_with("mini")).map(|(_, v)| v).collect();
    assert!(!mini_entries.is_empty(), "per-key health present: {keys:?}");
    assert!(mini_entries.iter().any(|h| h["kind"] == "payment_required"), "dead key marked");
}
