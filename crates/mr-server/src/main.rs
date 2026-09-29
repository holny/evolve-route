use mr_server::{config, state};

use clap::{Parser, Subcommand};
use mr_core::catalog::Catalog;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "modelroute", version, about = "Multi-agent LLM smart routing gateway")]
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
    /// Start a fake OpenAI-compatible upstream for demos and benchmarks
    #[command(hide = true)]
    MockUpstream {
        #[arg(long, default_value = "9101")]
        port: u16,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,mr::event=info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve { config, port } => serve(config, port),
        Cmd::Models { config } => models(config),
        Cmd::Doctor { config } => doctor(config),
        Cmd::MockUpstream { port } => mock_upstream(port),
        Cmd::Stats { config } => stats(config),
    }
}

fn stats(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let (cfg, path) = config::load_config(config_path.as_deref())?;
    println!("config: {}", path.as_deref().map(|p| p.display().to_string()).unwrap_or("<embedded default>".into()));
    let flywheel = mr_memory::Flywheel::open(&cfg.data.dir);
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
        tracing::info!("modelroute gateway listening on http://{addr}");
        axum::serve(listener, app).await?;
        anyhow::Ok(())
    })?;
    Ok(())
}

fn models(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let (cfg, path) = config::load_config(config_path.as_deref())?;
    let discovered = mr_discovery::discover(&cfg.discovery.agents);
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
    let discovered = mr_discovery::discover(&cfg.discovery.agents);
    let catalog = Catalog::build_with_discovered(&cfg, discovered);
    println!("[ok] catalog: {} models ({} builtin priors)",
        catalog.len(),
        catalog.models.iter().filter(|m| m.source == mr_core::types::Source::Builtin).count());
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
