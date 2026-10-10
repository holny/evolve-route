use evolve_server::{config, service, state};

use clap::{Parser, Subcommand};
use evolve_core::catalog::Catalog;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "evo-router", version, about = "Multi-agent LLM smart routing gateway")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the local routing gateway
    Serve {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        port: Option<u16>,
    },
    /// Print the resolved model catalog with source labels
    Models {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Validate configuration, credentials and catalog sanity
    Doctor {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Print flywheel aggregates learned from live traffic
    Stats {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// ⑦ Replay historical traffic to A/B test flywheel strategy params
    Backtest {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Candidate strategy B: success-rate signal weight (latency takes the rest)
        #[arg(long, default_value_t = 0.7)]
        ok_weight_b: f32,
        /// Candidate strategy B: bias gain applied to (reward − cross-model median)
        #[arg(long, default_value_t = 0.6)]
        gain_b: f32,
        /// Replay the most recent N routing events
        #[arg(long, default_value_t = 5000)]
        limit: usize,
        /// A3b LLM-as-a-Judge: point-wise score up to N events carrying
        /// resp_digest (requires telemetry.response_digest=true upstream)
        #[arg(long, default_value_t = 0)]
        judge_samples: usize,
    },
    /// Manage the LaunchAgent service (start/stop/restart/status/install)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
        /// Override the gateway port (defaults to config value)
        #[arg(long)]
        port: Option<u16>,
    },
    /// Install/rewrite the LaunchAgent service definition
    Install {
        /// Override the gateway port (defaults to config value)
        #[arg(long)]
        port: Option<u16>,
    },
    /// Start a fake OpenAI-compatible upstream for demos and benchmarks
    #[command(hide = true)]
    MockUpstream {
        #[arg(long, default_value = "9101")]
        port: u16,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Start the gateway service
    Start,
    /// Stop the gateway service
    Stop,
    /// Restart the gateway service
    Restart,
    /// Show service load/listen state
    Status,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,ev::event=info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve { config, port } => serve(config, port),
        Cmd::Models { config } => models(config),
        Cmd::Doctor { config } => doctor(config),
        Cmd::MockUpstream { port } => mock_upstream(port),
        Cmd::Stats { config } => stats(config),
        Cmd::Backtest { config, ok_weight_b, gain_b, limit, judge_samples } => {
            backtest(config, ok_weight_b, gain_b, limit, judge_samples)
        }
        Cmd::Service { action, port } => {
            let p = service::resolve_port(port);
            match action {
                ServiceAction::Start => service::start(p),
                ServiceAction::Stop => service::stop(p),
                ServiceAction::Restart => service::restart(p),
                ServiceAction::Status => service::status(p),
            }
        }
        Cmd::Install { port } => service::install(service::resolve_port(port)),
    }
}

/// ⑦ 策略回放验证：在 events.jsonl 上 A/B 对比当前参数与候选参数，
/// 报告裁决（采纳/保持）。采纳动作 = 编辑 evolve.toml [strategy] 段；
/// 回滚 = 删除该段（回到编译默认）。
fn backtest(
    config_path: Option<PathBuf>,
    ok_weight_b: f32,
    gain_b: f32,
    limit: usize,
    judge_samples: usize,
) -> anyhow::Result<()> {
    let (cfg, _) = config::load_config(config_path.as_deref())?;
    let expanded = evolve_memory::events::shellexpand_home_pub(&cfg.data.dir);
    let events = expanded.join("events.jsonl");
    // review#3 修复：A 组 = 用户当前生效参数（config [strategy] 段，缺省回落
    // 编译默认）——否则用户采纳过自定义参数后，backtest 会拿编译默认当
    // "当前"，裁决结论失实
    let a = evolve_memory::strategy::StrategyParams {
        version: cfg.strategy.version,
        ok_weight: cfg.strategy.ok_weight,
        bias_gain: cfg.strategy.bias_gain,
    };
    let b = evolve_memory::strategy::StrategyParams {
        version: u32::MAX, // 候选未定版
        ok_weight: ok_weight_b,
        bias_gain: gain_b,
    };

    // A3b：judge_samples > 0 时构建 LLM-as-a-Judge asker（System One Noul——
    // "该响应是否充分回答了查询"）。无 DECISION_MODEL_API_KEY（或旧名 TYPESAFE_API_KEY）或 samples=0 时跳过。
    let report = if judge_samples > 0 {
        let backend = evolve_decision::decision_backend::DecisionModelBackend::from_env()
            .ok_or_else(|| anyhow::anyhow!("--judge-samples 需要 DECISION_MODEL_API_KEY（兼容旧名 TYPESAFE_API_KEY）"))?;
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        let asker = move |query: &str, response: &str| -> Option<f32> {
            runtime.block_on(backend.judge_response_quality(query, response))
        };
        evolve_memory::backtest::replay_with_judge(&events, &a, &b, limit, &asker, judge_samples)?
    } else {
        evolve_memory::backtest::replay(&events, &a, &b, limit)?
    };
    evolve_memory::backtest::print_report(&report);
    Ok(())
}

fn stats(config_path: Option<PathBuf>) -> anyhow::Result<()> {    let (cfg, path) = config::load_config(config_path.as_deref())?;
    println!("config: {}", path.as_deref().map(|p| p.display().to_string()).unwrap_or("<embedded default>".into()));
    let flywheel = evolve_memory::Flywheel::open(&cfg.data.dir);
    let telemetry = flywheel.telemetry_snapshot();
    let stats = flywheel.stats();
    if stats.is_empty() {
        println!("(no flywheel data yet in {})", cfg.data.dir);
        return Ok(());
    }
    println!(
        "{:<34} {:>5} {:>5} {:>8} {:>8} {:>10} {:>8} {:>7} {:>8}",
        "model", "req", "ok", "ttft", "total", "tok(p/c)", "cached", "tool%", "w×bias"
    );
    let mut rows: Vec<_> = stats.into_iter().collect();
    rows.sort_by_key(|(_, s)| std::cmp::Reverse(s.requests));
    for (id, s) in rows {
        let t = telemetry.get(&id).cloned().unwrap_or_default();
        let tool_rate = if s.tc_total > 0 {
            format!("{:.0}%", 100.0 * s.tc_valid_json as f32 / s.tc_total as f32)
        } else { "-".into() };
        let w = format!(
            "{}×{}",
            Catalog::build(&cfg).get(&id).and_then(|m| m.weight).unwrap_or(1.0),
            t.learned_bias.map(|b| format!("{b:.2}")).unwrap_or_else(|| "1.00".into())
        );
        println!(
            "{:<34} {:>5} {:>5} {:>8} {:>8} {:>10} {:>8} {:>7} {:>8}",
            id,
            s.requests,
            s.success,
            s.ttft_n.checked_div(s.ttft_n).map(|_| format!("{}ms", s.ttft_ms_sum / s.ttft_n)).unwrap_or_else(|| "-".into()),
            s.total_ms_n.checked_div(s.total_ms_n).map(|_| format!("{}ms", s.total_ms_sum / s.total_ms_n)).unwrap_or_else(|| "-".into()),
            format!("{}/{}", s.prompt_tokens, s.completion_tokens),
            s.cached_tokens,
            tool_rate,
            w
        );
        if let Some(c) = t.calibration {
            println!("  └─ calibration: ×{c:.2}  reliability: {}  speed_obs: {}",
                t.reliability.map(|v| format!("{v:.2}")).unwrap_or_else(|| "-".into()),
                t.speed_obs.map(|v| format!("{v:.2}")).unwrap_or_else(|| "-".into()));
        }
    }
    Ok(())
}

async fn mock_upstream_server(port: u16) -> anyhow::Result<()> {
    use axum::body::Body;
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(|body: axum::body::Bytes| async move {
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let model = v["model"].as_str().unwrap_or("unknown").to_string();
            let stream = v["stream"].as_bool().unwrap_or(false);
            if stream {
                let sse = format!(
                    "data: {{\"id\":\"m1\",\"model\":\"{model}\",\"choices\":[{{\"delta\":{{\"content\":\"hello from mock\"}}}}]}}\n\n\
                     data: {{\"id\":\"m1\",\"model\":\"{model}\",\"choices\":[],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":4}}}}\n\n\
                     data: [DONE]\n\n"
                );
                axum::http::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from(sse))
                    .unwrap()
            } else {
                let payload = serde_json::json!({
                    "model": model,
                    "choices": [{"message": {"role": "assistant", "content": format!("hello from mock-{model}")}}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 4},
                });
                axum::http::Response::builder()
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await?;
    tracing::info!("mock upstream on http://127.0.0.1:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

fn mock_upstream(port: u16) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(mock_upstream_server(port))
}

fn serve(config_path: Option<PathBuf>, port_override: Option<u16>) -> anyhow::Result<()> {
    let (cfg, path) = config::load_config(config_path.as_deref())?;
    tracing::info!(?path, "config loaded");
    let port = port_override.unwrap_or(cfg.server.port);
    let host = cfg.server.host.clone();
    let state = state::build_state(cfg);

    let app = state::build_router(state.clone());

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        state.start_background();
        let addr = format!("{host}:{port}");
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        tracing::info!("evolve gateway listening on http://{addr}");
        // 优雅关闭：SIGINT/SIGTERM 后停止接新请求，flush 飞轮聚合再退出
        // （此前 SIGINT 直接杀进程，flush 永远执行不到，每次重启丢 30s 聚合）
        let shutdown = async {
            let _ = tokio::signal::ctrl_c().await;
            #[cfg(unix)]
            {
                let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
                if term.recv().await.is_some() {
                    return;
                }
                let _ = tokio::signal::ctrl_c().await;
            }
        };
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await?;
        state.flywheel.flush();
        tracing::info!("flywheel flushed, shutting down");
        anyhow::Ok(())
    })?;
    Ok(())
}

fn models(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let (cfg, path) = config::load_config(config_path.as_deref())?;
    let discovered = evolve_discovery::discover(&cfg.discovery.agents);
    let catalog = Catalog::build_with_discovered(&cfg, discovered);
    println!("config: {}", path.as_deref().map(|p| p.display().to_string()).unwrap_or("<embedded default>".into()));
    println!(
        "{:<22} {:<12} {:>10} {:>8} {:>8}  {:<10} note",
        "id", "provider", "window", "in$/M", "out$/M", "source"
    );
    for m in &catalog.models {
        println!(
            "{:<22} {:<12} {:>10} {:>8.2} {:>8.2}  {:<10} {}",
            m.id,
            m.provider,
            m.context_window.map(|w| w.to_string()).unwrap_or_else(|| "unknown".into()),
            m.cost.map(|c| c.input).unwrap_or(-1.0),
            m.cost.map(|c| c.output).unwrap_or(-1.0),
            m.source.label(),
            m.source_note.clone().unwrap_or_default()
        );
    }
    Ok(())
}

fn doctor(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let (cfg, path) = config::load_config(config_path.as_deref())?;
    println!("[ok] config parsed: {}", path.as_deref().map(|p| p.display().to_string()).unwrap_or("<embedded default>".into()));
    let discovered = evolve_discovery::discover(&cfg.discovery.agents);
    let catalog = Catalog::build_with_discovered(&cfg, discovered);
    println!("[ok] catalog: {} models ({} builtin priors)",
        catalog.len(),
        catalog.models.iter().filter(|m| m.source == evolve_core::types::Source::Builtin).count());
    let mut problems = 0;
    for m in &catalog.models {
        if m.base_url.is_empty() {
            println!("[warn] {} {}: no base_url (builtin prior, unusable until configured)", m.source.label(), m.id);
            problems += 1;
            continue;
        }
        match (&m.api_key_env, m.has_credential()) {
            (Some(env), false) => {
                println!("[warn] {}: env {env} is empty (local urls are exempt)", m.id);
                problems += 1;
            }
            _ => println!("[ok] {} -> {} ({})", m.id, m.base_url, m.upstream_model),
        }
    }
    if problems == 0 {
        println!("[ok] doctor clean");
    } else {
        println!("[note] {problems} warning(s); routing hard-constraints will skip unusable models");
    }
    Ok(())
}
