<div align="center">

# EvolveRoute

**A local LLM routing gateway with a decision model at its core. It evolves with every request.**

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org)
[![Tests](https://img.shields.io/badge/tests-94%20passing-brightgreen.svg)](#development)

[English](README.md) | [中文](README.zh-CN.md)

</div>

---

> **不选最好的模型——选最合适的。**
>
> 一个本地部署的 LLM 智能路由网关：决策模型逐请求判定任务画像，飞轮从每次结果中学习，科学公式保证每次决策可追溯。**你的数据从不出主机。**

![EvolveRoute architecture](docs/assets/architecture.svg)

**EvolveRoute** is a local-first routing gateway that sits between your Agent and every LLM provider you use. It doesn't ship every request to the "best" model — it routes each one to the **most suitable** one, judged on the fly by a decision model (TypeSafe Jev + a multilingual heuristic second opinion, take-conservative) against what the task actually needs and what your providers can afford right now. Every request feeds the flywheel, so the routing gets sharper the more you use it.

What makes it different: every decision is **local, transparent, and auditable** — JSONL event log, response headers (`x-ev-model / x-ev-reason / x-ev-decision-id`), zero-build dashboard. User data never leaves the host. And when the decision model is unavailable, the heuristic takes over and **routing never blocks**.


## Why not another proxy

| | EvolveRoute | LiteLLM / OpenRouter | RouteLLM |
|---|---|---|---|
| **Routing decision** | Decision model judges *every request* (8 signals) + heuristic second opinion, fail-open | Config/prefix rules, or model-based router trained offline | Trained router models, offline |
| **Self-evolution** | Flywheel: 4-layer feedback (transport / tool-syntax / semantic loop / plugin) rewrites per-model learned bias & reliability | Static weights | Static after training |
| **Subscription economics** | Per-plan credit multipliers, tier allowances (5h/week/month), quota budget protection (soft-deprioritize → sticky-break → hard-filter) | Cost tracking only | – |
| **Deployment** | One Rust binary (~7 MB), zero deps, no Docker/DB | Python + DB | Python |
| **Protocol** | OpenAI ↔ Anthropic bidirectional translation (incl. streaming) | OpenAI-centric | OpenAI |
| **Multi-agent** | Native identity for opencode / codex / claude-code / pi / openclaw / hermes / dsh (session-aware stickiness) | Generic | Generic |
| **Failure handling** | Multi-key pools → per-provider & per-endpoint circuit breaking → fallback chain → **out-of-chain last-resort sweep** | Retries | – |


## The routing formula
![EvolveRoute formula](docs/assets/formula.png)

## Three pillars

1. **Intelligent decisions, not prefix rules.** Every request is judged by a decision model — **[TypeSafe Jev](https://github.com/typesafe-ai)** — on 8 signals (domain, difficulty, needs-vision, triviality, tool-density, high-stakes, session depth, size). A built-in multilingual heuristic judge runs as an independent second opinion (take-conservative fusion, covering Jev's CJK blind spots); if Jev is unreachable, the heuristic takes over and **routing never blocks**. Optional `laya` backend slots into the same interface.
2. **Budget-aware routing.** Every candidate is priced in its plan's own currency — credits (zhipu / Z.ai / MiniMax), dollars (opencode zen Go), AFP (volces) — before it is scored. Quota budget protection runs at three levels: **>60%** of your allowance deprioritizes the plan, **>85%** breaks session stickiness, **>95%** filters it out entirely — preserving headroom for the tasks that actually need it.
3. **Managed plans in one dashboard.** Vendor subscription plans are first-class citizens: tiers, allowances, credit multipliers and per-model dollar limits are built in; the dashboard shows live provider-reported quotas per window with one-click reconciliation.


## Managed plans

| Plan | Vendor | Kind | Pricing model | Tiers | Windows |
|---|---|---|---|---|---|
| `zhipu-coding` | zhipu (bigmodel.cn) | Coding Plan | credits, off-peak 50% | lite / pro / max | 5h · week · month |
| `zai-devpack` | Z.ai | Coding Plan | credits | lite / pro / max | 5h · week · month |
| `opencode-go` | opencode zen | Go Plan | $/1M per model + per-model monthly $ cap | go / go-plus | month (5h = 20%) |
| `volces-coding` | volces | Coding Plan | AFP credits | lite / pro | 5h · week · month |
| `volces-agent` | volces | Agent Plan | AFP credits | — | 5h · week · month |
| `minimax-coding` | MiniMax | Coding Plan | credits | lite / pro / max | 5h · week · month |
| pay-as-you-go | any OpenAI / Anthropic | API | $ per token | — | — |

Plans are auto-detected from base URLs, and every window (5h / weekly / monthly) is tracked with provider-truth quota headers where the vendor reports them.


## Features
- **本地部署，数据安全** — One Rust single-binary (~7 MB), zero runtime dependencies, no Docker, no Python, no cloud. The whole routing chain runs in-process on your machine: user data, session content, and telemetry signals **never leave the host**. Every decision lands in a local JSONL event log — fully auditable, exportable, deletable.
- **决策模型智能判断，懂经济账** — A decision model (TypeSafe Jev + a multilingual heuristic second opinion, take-conservative) judges every request on 8 signals (domain, difficulty, needs-vision, triviality, tool-density, high-stakes, session depth, size). Each candidate is **priced in its plan's own currency before it is scored** (credits for zhipu / Z.ai / MiniMax, dollars for opencode Go, AFP for volces), then the five factors are fused with difficulty-linked weights. Three-tier budget protection: >60% quota deprioritizes, >85% breaks session stickiness, >95% filters out — saving headroom for the tasks that actually need it.
- **科学公式归因** — Five factors (quality · speed · cost · reliability · headroom) combined with difficulty-linked weights and a flywheel bias multiplier — see [The routing formula](#the-routing-formula) for the full derivation. Every term in the score traces back to a named quantity on the request, the model, or the flywheel. No black box.
- **自进化飞轮** — Every request feeds the flywheel. 4 layers of feedback (transport success, tool-call syntax validity, session-confirmed semantic loops, plugin-reported outcomes) rewrite the per-model learned bias, observed reliability, and token calibration — long-term 50% + last-30 30% + last-10 20%. **Usage is the learning rate.**
- **多 Agent + 多 Provider 适配** — opencode / Claude Code / Codex / Pi (coding agents), openclaw / hermes / dsh (local harnesses), and any OpenAI- or Anthropic-compatible client. No code changes to your stack. Routes to any OpenAI-compatible **or** Anthropic upstream; Switchyard handles bidirectional streaming translation (event mapping + deterministic IDs) so Claude Code works out of the box. Subscription plans (zhipu coding, Z.ai, opencode zen, volces, MiniMax) and pay-as-you-go APIs share one endpoint.
- **极速决策，零依赖部署** — Single Rust binary, 6.7 MB; cold start <30 ms; one request adds <300 ms (a Jev call + heuristic fallback). Multi-key pool rotation, provider- and endpoint-level circuit breaking, and an out-of-chain last-resort sweep keep routing alive through single-vendor outages.
- **本地面板管控，全程决策可见** — Zero-build dashboard at `http://127.0.0.1:8787`: live decision stream with plain-language reasons, scoring matrix, fallback chains with per-hop causes, latency waterfall, model trends, provider & quota management, flywheel learning state. Every response carries `x-ev-model / x-ev-reason / x-ev-decision-id`; `/api/events` exposes the JSONL decision log for query and replay.

## Quick Start

**Requirements:** Rust stable (2024 edition), any OpenAI-compatible or Anthropic provider credentials.

```bash
git clone https://github.com/your-org/evolve-route.git
cd evolve-route
cargo build --release

# binary at target/release/evolveroute
./target/release/evolveroute serve
# gateway listening on http://127.0.0.1:8787
```

Configuration lives at `~/.evolveroute/evolveroute.toml` (a default is embedded and written on first run). Point providers with their base URLs and API keys:

```toml
[server]
host = "127.0.0.1"
port = 8787

[[models]]
id = "zhipuai-coding-plan/glm-5.3"
provider = "zhipuai-coding-plan"
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
api_key_env = "ZHIPU_API_KEY"
upstream_model = "glm-5.3"
context_window = 1024000
cost = { input = 0.0, output = 0.0 }   # plan models: marginal cost ≈ 0
```

### Connect your agents

**opencode** (`~/.config/opencode/opencode.jsonc`):

```jsonc
{
  "provider": {
    "evolveroute": {
      "npm": "@ai-sdk/openai-compatible",
      "options": { "baseURL": "http://127.0.0.1:8787/v1" },
      "models": {
        "auto": {
          "name": "Auto (smart routing)",
          "attachment": true,
          "reasoning": true,
          "tool_call": true
        }
      }
    }
  }
}
```

**Claude Code:**

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8787
export ANTHROPIC_MODEL=auto
claude
```

**Any OpenAI-compatible client:** base URL `http://127.0.0.1:8787/v1`, model `auto`.

### Run as a service (macOS LaunchAgent)

```bash
evolveroute service install   # also: start / stop / restart / status
```


## The dashboard

Open `http://127.0.0.1:8787/`:

![EvolveRoute dashboard](docs/assets/dashboard.png)

- **Live decision stream** — every request with its 8-signal judgment, chosen model, and a plain-language reason
- **Scoring matrix** — top candidates with per-factor scores (quality / speed / cost / reliability / headroom)
- **Fallback chains** — which models were tried, which failed, and exactly why
- **Latency waterfall** — preprocess / decision / TTFT / streaming, per request
- **Provider management** — add/edit/scan providers, tier allowances (5h/week/month), live provider-reported quotas, one-click reconciliation
- **Flywheel card** — learned bias, observed reliability, calibration, semantic confirmations per model


## How routing decisions work

```
request → estimate tokens → hard constraints (window / vision / credentials)
        → decision model judge (8 signals, heuristic backup)
        → quality floor → composite score (difficulty-linked weights)
        → ε-greedy exploration (10%) → session stickiness check
        → route → observe outcome → feed the flywheel
```

Deep dives: [Architecture](docs/ARCHITECTURE.md) · [Decision flow](docs/DECISION-FLOW.md) · [Scoring factors](docs/FACTORS.md) · [Modules](docs/MODULES.md) *(Chinese, English translation welcome)*


## Development

```bash
cargo build --release
cargo test          # 94 tests
cargo clippy        # zero warnings policy
```

Workspace layout:

| Crate | Role |
|---|---|
| `ev-core` | Decision engine, scoring, catalog, plan registry, config |
| `ev-decision` | Decision-model backends (TypeSafe Jev, laya, heuristic) + protocol translation |
| `ev-discovery` | Agent config scanning, remote /models fetching, models.dev enrichment |
| `ev-memory` | Flywheel, quota ledger, health registry, session store, event log |
| `ev-server` | Axum gateway, relay, dashboard, provider APIs, CLI |

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.


## License

[MIT](LICENSE)
