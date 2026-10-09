<div align="center">

# EvolveRoute

**A local LLM routing gateway with a decision model at its core. It evolves with every request.**

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org)
[![Tests](https://img.shields.io/badge/tests-94%20passing-brightgreen.svg)](#development)

[English](README.md) | [中文](README.zh-CN.md)

</div>

---

**EvolveRoute** is a single-binary gateway that sits between your coding agents and your LLM providers. Point your agents at `http://127.0.0.1:8787` with `model = "auto"` — for every request, a decision model judges the task (domain, difficulty, vision, tool-density, stakes…) and routes it to the best *fitting* model: cheap models for chit-chat, strong models for complex refactors, and **oversized contexts never enter small-window models**. Everything is observable: why it routed, where it went, what it cost.

It is **local-first** (no cloud dependency for routing), **economics-aware** (subscription quotas, credit multipliers, per-model dollar limits), and **self-evolving** (a flywheel learns from every outcome and rewrites its own scoring biases).

![EvolveRoute architecture](docs/assets/architecture.svg)

*Agents point at one endpoint with `model = "auto"`. The dual judge (TypeSafe Jev + heuristic) scores every request; the flywheel feeds every outcome back into ranking; every provider plan is priced and budgeted its own way. [Edit the source](docs/assets/architecture.excalidraw).*

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

## Why EvolveRoute

| | EvolveRoute | LiteLLM / OpenRouter | RouteLLM |
|---|---|---|---|
| **Routing decision** | Decision model judges *every request* (8 signals) + heuristic second opinion, fail-open | Config/prefix rules, or model-based router trained offline | Trained router models, offline |
| **Self-evolution** | Flywheel: 4-layer feedback (transport / tool-syntax / semantic loop / plugin) rewrites per-model learned bias & reliability | Static weights | Static after training |
| **Subscription economics** | Per-plan credit multipliers, tier allowances (5h/week/month), quota budget protection (soft-deprioritize → sticky-break → hard-filter) | Cost tracking only | – |
| **Deployment** | One Rust binary (~7 MB), zero deps, no Docker/DB | Python + DB | Python |
| **Protocol** | OpenAI ↔ Anthropic bidirectional translation (incl. streaming) | OpenAI-centric | OpenAI |
| **Multi-agent** | Native identity for opencode / codex / claude-code / pi (session-aware stickiness) | Generic | Generic |
| **Failure handling** | Multi-key pools → per-provider & per-endpoint circuit breaking → fallback chain → **out-of-chain last-resort sweep** | Retries | – |

## Features

- **Decision model in the loop** — a judge model scores every request on 8 signals (domain, difficulty, needs-vision, triviality, tool-density, high-stakes, session depth…), fused with a multilingual heuristic judge (double-judge, take-conservative). Decision failure never blocks routing.
- **Two-phase scoring** — hard constraints first (context window with headroom, vision modality, credentials, proven max-accepted), then a quality floor, then composite weights (quality / speed / cost / stability / headroom) with **difficulty-linked weighting**: cheap models favored for easy tasks, premium models unlocked as difficulty rises.
- **The flywheel** — every request feeds back: transport success, tool-call syntax validity, session-confirmed semantic loops, plugin-reported outcomes. Per-model learned bias, observed reliability (long-term 50% + last-30 30% + last-10 20%), and token calibration evolve the ranking automatically.
- **Subscription economics** — built-in plan registry (zhipu coding, z.ai, opencode zen, volces coding/agent, minimax, pay-as-you-go) with official credit multipliers and tier allowances. Budget protection at three levels: >60% quota deprioritizes, >85% breaks stickiness, >95% filters out — saving headroom for hard tasks.
- **Failure resilience** — multi-key pools with model×key accounting, provider-account and endpoint-level circuit breaking, ranked fallback chains, context-overflow rerouting, and an out-of-chain last-resort sweep when the whole chain dies.
- **Cross-protocol** — OpenAI-ingress requests are transparently translated to Anthropic upstreams (and responses back), including streaming event mapping with deterministic IDs. Claude Code works out of the box.
- **Session stickiness** — stable sessions reuse the chosen model (zero re-decision latency) with bounded turns; task shifts, budget pressure, or context growth break it automatically. ε-greedy exploration (10%) keeps sampling runner-ups so the flywheel never starves.
- **Observability** — built-in zero-build dashboard (`/`): live decision stream with plain-language reasons, scoring matrix, fallback chains with per-hop causes, latency waterfall (preprocess / decision / TTFT / streaming), model trends, provider & quota management, flywheel learning state — in 6 languages. Plus `x-ev-model / x-ev-reason / x-ev-decision-id` response headers and JSONL event logs.

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

## The routing formula

Every candidate model is scored **per request** — every factor below is computed live from the plan registry, the vendor's official rates, and the flywheel:

$$\text{score}(m) = w_q Q + w_s S + w_c C + w_r R + w_h H \;\times\; \sqrt{\,u_m \cdot b_m\,}$$

| Factor | How it's computed |
|---|---|
| Quality `Q` | $1 - \rho\,(1 - \tau^{\,1+2\rho})$ — tier requirement steepens exponentially with difficulty; high-stakes tasks multiply in reasoning tier |
| Quality floor | $1 - 0.45\,\rho$ — below the floor a model is **filtered out** before scoring (difficulty 3 → only quality ≥ 0.55 qualifies) |
| Speed `S` | $0.7\,\text{prior} + 0.3\,\text{observed tok/s}$ |
| Cost `C` | plan models: cheapest-in-plan credit rate ÷ own rate (official multipliers) · pay-as-you-go: min price ÷ own price |
| Reliability `R` | $0.5 R_\infty + 0.3 R_{30} + 0.2 R_{10}$ — long-term baseline + last 30 + last 10 (missing layers re-merge) |
| Headroom `H` | $(W - n_{in} - n_{out}) / W$ — remaining context after the estimate |

And the weights themselves **move with difficulty** ($\hat d = $ difficulty $/ 3$):

$$w_c \times (2 - 1.6\hat d) \qquad w_q \times (0.75 + 0.25\hat d) \qquad \text{(then renormalized)}$$

Easy task ($\hat d \to 0$): the cost weight **doubles** — cheap models win. Hard task ($\hat d \to 1$): cost weight drops to 40% — quality dominates. The result is multiplied by $\sqrt{\text{user weight} \times \text{learned bias}}$ (clamped 0.4–1.8): the flywheel can nudge, never decide. Then ε-greedy exploration samples the runner-up 10% of the time, stable sessions reuse the chosen model, and any plan above 95% quota burn is filtered out entirely.

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
