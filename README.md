<div align="center">

# EvolveRouter

**A local LLM routing gateway with a decision model at its core. It evolves with every request.**

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org)
[![Tests](https://img.shields.io/badge/tests-111%20passing-brightgreen.svg)](#development)

[English](README.md) | [中文](README.zh-CN.md)

</div>

---

> **Not the best model — the right one, for every request.**
>
> A local-first LLM routing gateway: a decision model judges every request's task profile, a flywheel learns from every outcome, and a scientific formula keeps every decision traceable. **Your data never leaves your machine.**

![EvolveRouter architecture](docs/assets/architecture.svg)

**EvolveRouter** is a local-first routing gateway that sits between your Agent and every LLM provider you use. It doesn't ship every request to the "best" model — it routes each one to the **most suitable** one, judged on the fly by a **decision model** (any System-One-compatible backend — cloud or self-hosted — plus a multilingual heuristic second opinion, take-conservative fusion) against what the task actually needs and what your providers can afford right now. Every request feeds the flywheel, so the routing gets sharper the more you use it.

What makes it different: every decision is **local, transparent, and auditable** — JSONL event log, response headers (`x-ev-model / x-ev-reason / x-ev-decision-id`), zero-build dashboard. User data never leaves the host. And when the decision model is unavailable, the heuristic takes over and **routing never blocks**.


## Why not another proxy

| | EvolveRouter | LiteLLM / OpenRouter | RouteLLM |
|---|---|---|---|
| **Routing decision** | Decision model judges *every request* (8 signals) + heuristic second opinion, fail-open | Config/prefix rules, or model-based router trained offline | Trained router models, offline |
| **Self-evolution** | Flywheel: 4-layer feedback (transport / tool-syntax / semantic loop / plugin) rewrites per-model learned bias & reliability | Static weights | Static after training |
| **Subscription economics** | Per-plan credit multipliers, tier allowances (5h/week/month), quota budget protection (soft-deprioritize → sticky-break → hard-filter) | Cost tracking only | – |
| **Deployment** | One Rust binary (~7 MB), zero deps, no Docker/DB | Python + DB | Python |
| **Protocol** | OpenAI ↔ Anthropic bidirectional translation (incl. streaming) | OpenAI-centric | OpenAI |
| **Multi-agent** | Native identity for opencode / codex / claude-code / pi / openclaw / hermes / dsh (session-aware stickiness) | Generic | Generic |
| **Failure handling** | Multi-key pools → per-provider & per-endpoint circuit breaking → fallback chain → **out-of-chain last-resort sweep** | Retries | – |


## The routing formula, step by step

![EvolveRouter formula](docs/assets/formula.png)

The card above is the overview. Here is how each formula actually fires, in the order the decision pipeline runs. Every quantity is named and visible in the event log.

### Step 1 — Hard filters *(no formula: binary gates)*

Context window too small for the estimate → out. Task needs vision, model can't → out. Model in circuit-breaker cooldown → out. Remaining quota below 8× the estimate → out. Candidates dominated on every factor (Pareto) → out. What survives is the eligible set.

### Step 2 — Dynamic weights *(difficulty-linked × quota-aware — [BaRP](https://arxiv.org/abs/2510.07429) Eq.1)*

$$\mathbf{W} = \operatorname{norm}\big(\mathbf{w}^{\mathrm{diff}}(d_{\mathrm{eff}}) \odot \mathbf{w}^{\mathrm{quota}}(t)\big), \quad t = \frac{\#\{\text{candidates with quota} < 8 \times \text{est}\}}{\#\{\text{candidates with quota data}\}}$$

*What happens:* a hard task (d_eff high) tilts weights toward quality; a simple task tilts toward cost. Independently, when many eligible candidates are quota-tight (t → 1), the cost weight rises ×(1+1.5t) and quality yields ×(1−0.25t). No quota data → t undefined → weights untouched.

### Step 3 — Five-factor score + advisor bonus

$$\mathrm{base}(m) = \sum_k W_k \cdot f_k(m), \qquad f_k \in \{\text{quality, speed, cost, reliability, headroom}\}$$
$$\mathrm{base}(m) \times = 1 + 0.15 \cdot c_r \quad \text{if the advisor recommended } m \text{ with confidence } c_r > 0.3$$

*What happens:* each candidate is priced **in its plan's own currency** (credits / dollars / AFP) before cost scoring. The advisor (decision model, seeing anonymized profiles of the top-8) can directly boost its pick by up to +15%.

### Step 4 — Thompson exploration *(uncertainty-driven — [BayesianRouter](https://arxiv.org/abs/2510.02850) Eqs.4–6, cross-domain)*

$$\mu_0 = 0.15 + 0.70\,\mathrm{rank}(q_m), \quad \alpha = \mu_0 \nu_0 + s, \quad \beta = (1-\mu_0)\nu_0 + f, \quad \nu_0 = 8$$
$$\tilde{\theta} \sim \mathrm{Beta}(\alpha, \beta), \qquad \mathrm{final}(m) = \mathrm{base}(m) \times \big(1 + 3\rho(\tilde{\theta} - \mathbb{E}[\theta])\big)$$

*What happens:* s/f are the model's recent success/failure counts. A model with few observations has a wide posterior → large sampling offset → it gets tried. A well-observed model has a narrow posterior → offset vanishes → no more free chances. ρ = `explore_ratio` (default 0.1, i.e. ±3% typical jitter). The prior mean μ₀ uses the *rank* of the decision-model quality factor among candidates — absolute q values saturate near 1.0, and a saturated prior would silently kill exploration.

### Step 5 — Flywheel bias *(two-stage normalization + baseline-relative update)*

$$\operatorname{norm}(v) = \operatorname{clamp}\!\left(\frac{v - \bar{x}}{q_{80} - q_{20}},\ 0,\ 1\right), \qquad \mathrm{reward} = w_{\mathrm{ok}} \cdot \operatorname{norm}(\mathrm{ok}) + (1{-}w_{\mathrm{ok}})\big(1 - \operatorname{norm}(\ln \mathrm{ms})\big)$$
$$B(m) = \operatorname{clamp}\big(1 + (\mathrm{reward}_m - \operatorname{median}_{m'}\mathrm{reward}_{m'}) \cdot \gamma,\ 0.7,\ 1.3\big)$$

*What happens:* for each model, the last 30 outcomes (success + latency, ln-compressed) become a reward in [0,1] — first normalized against its own rolling mean, then scaled by its own 20/80 spread (this is what makes a 200 ms model and a 20 s model comparable). The bias learns the *advantage over the cross-model median*, not absolute outcomes — when the whole workload gets harder, every reward drops together, the median moves with them, and bias stays still. Final scores are multiplied by B(m). The flywheel can only fine-tune: |B| ≤ 1.3, and the decision model's judgment is the ceiling.

### Step 6 — After routing

Quality cascade: a *successful* response that is degenerate / empty / has broken tool calls is discarded and the next chain candidate is tried — stricter at the chain head, looser further down. Repeated identical requests hit a local completion cache. Every event records `regret = best-available-score − chosen-score`, so routing quality is measurable, and every strategy parameter change must survive `evo-router backtest` (replay on real events, judged by Performance-Gap-Recovered) before it ships.

## Three pillars

1. **Intelligent decisions, not prefix rules.** Every request is judged by a pluggable **decision model** on 8 signals (domain, difficulty, needs-vision, triviality, tool-density, high-stakes, session depth, size). A built-in multilingual heuristic judge runs as an independent second opinion (take-conservative fusion, covering the decision model's CJK blind spots); if the decision model is unreachable, the heuristic takes over and **routing never blocks**. Backends are swappable by config — cloud (TypeSafe Jev) or self-hosted open-source decision models (Intern-Decision, StartLux-Decision) behind the same protocol.
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
- **Local-first, data-safe** — One Rust single-binary (~7 MB), zero runtime dependencies, no Docker, no Python, no cloud. The whole routing chain runs in-process on your machine: user data, session content, and telemetry signals **never leave the host**. Every decision lands in a local JSONL event log — fully auditable, exportable, deletable.
- **Decision-model intelligence, economics-aware** — A pluggable decision model (cloud or self-hosted, System-One protocol) + a multilingual heuristic second opinion, take-conservative fusion, judges every request on 8 signals (domain, difficulty, needs-vision, triviality, tool-density, high-stakes, session depth, size). Each candidate is **priced in its plan's own currency before it is scored** (credits for zhipu / Z.ai / MiniMax, dollars for opencode Go, AFP for volces), then the five factors are fused with difficulty-linked weights. Three-tier budget protection: >60% quota deprioritizes, >85% breaks session stickiness, >95% filters out — saving headroom for the tasks that actually need it.
- **Scientific formula, no black box** — Five factors (quality · speed · cost · reliability · headroom) combined with difficulty-linked weights and a flywheel bias multiplier — see [The routing formula](#the-routing-formula-step-by-step) for the full derivation. Every term in the score traces back to a named quantity on the request, the model, or the flywheel. No black box.
- **Self-evolving flywheel** — Every request feeds the flywheel. 4 layers of feedback (transport success, tool-call syntax validity, session-confirmed semantic loops, plugin-reported outcomes) are **two-stage normalized** (subtract rolling mean, clamp by the 20/80 inter-quantile range) and learned **relative to the cross-model median** — so workload drift never leaks into the learned bias. Long-term 50% + last-30 30% + last-10 20%. **Usage is the learning rate.**
- **Uncertainty-driven exploration** — No fixed ε-greedy. Each candidate carries a Beta(α, β) posterior (success/failure counts with the decision-model score as prior mean); Thompson sampling makes sample-poor models explore automatically and sample-rich models converge. Strategy parameters are versioned and must pass `evo-router backtest` — replay on real production events, judged by Performance-Gap-Recovered — before adoption.
- **Quality cascade & completion cache** — The fallback chain escalates not only on errors but on *poor quality* (degenerate output, empty responses, broken tool calls) with stricter gating at the chain head; exhausted chains still return the degraded response rather than an error. Identical logical requests hit a local completion cache (TTL-limited, `x-ev-no-cache` to bypass) for zero-cost zero-latency repeats.
- **Self-distilling capability cards** — Every 50 gated successes per model, the decision model re-estimates that model's capability tiers from observed statistics (conservatively merged, shift ≤ 0.15 per step) — so silently-updated upstream models can't leave the catalog stale. Users' explicit tier declarations are never overridden; delete `distilled.json` to roll back.
- **Any Agent, any Provider** — opencode / Claude Code / Codex / Pi (coding agents), openclaw / hermes / dsh (local harnesses), and any OpenAI- or Anthropic-compatible client. No code changes to your stack. Routes to any OpenAI-compatible **or** Anthropic upstream; Switchyard handles bidirectional streaming translation (event mapping + deterministic IDs) so Claude Code works out of the box. Subscription plans (zhipu coding, Z.ai, opencode zen, volces, MiniMax) and pay-as-you-go APIs share one endpoint.
- **Fast decisions, zero-deps deployment** — Single Rust binary, 6.7 MB; cold start <30 ms; one request adds <300 ms (a decision-model call + heuristic fallback). Multi-key pool rotation, provider- and endpoint-level circuit breaking, and an out-of-chain last-resort sweep keep routing alive through single-vendor outages.
- **Full dashboard control, every decision visible** — Zero-build dashboard at `http://127.0.0.1:8787`: live decision stream with plain-language reasons, scoring matrix, fallback chains with per-hop causes, latency waterfall, model trends, provider & quota management, flywheel learning state. Every response carries `x-ev-model / x-ev-reason / x-ev-decision-id`; `/api/events` exposes the JSONL decision log for query and replay.

## Quick Start

**Requirements:** Rust stable (2024 edition), any OpenAI-compatible or Anthropic provider credentials.

```bash
git clone https://github.com/holny/evolve-router.git
cd evolve-router
cargo build --release

# binary at target/release/evo-router
./target/release/evo-router serve
# gateway listening on http://127.0.0.1:8787
```

Configuration lives at `~/.evolve/evolve.toml` (a default is embedded and written on first run). Point providers with their base URLs and API keys:

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
    "evo-router": {
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
evo-router service install   # also: start / stop / restart / status
```


## The dashboard

Open `http://127.0.0.1:8787/`:

![EvolveRouter dashboard](docs/assets/dashboard.png)

- **Live decision stream** — every request with its 8-signal judgment, chosen model, and a plain-language reason
- **Scoring matrix** — top candidates with per-factor scores (quality / speed / cost / reliability / headroom)
- **Fallback chains** — which models were tried, which failed, and exactly why
- **Latency waterfall** — preprocess / decision / TTFT / streaming, per request
- **Provider management** — add/edit/scan providers, tier allowances (5h/week/month), live provider-reported quotas, one-click reconciliation
- **Flywheel card** — learned bias, observed reliability, calibration, semantic confirmations per model


## How routing decisions work

```
request → digest signals → session stickiness check
        → dual judge (decision model + multilingual heuristic, take-conservative fusion)
        → route advisor (top-8 candidates, anonymized, decision model picks one directly)
        → hard filters (context window / vision / circuit breaking / quota)
        → dynamic weights (difficulty-linked × quota-aware)
        → five-factor score × flywheel bias
        → Thompson exploration (Beta posterior sampling — uncertainty-driven)
        → route → fallback chain (3) → quality cascade
        → observe outcome → feed the flywheel
```

Self-evolution is grounded in recent routing research, adopted critically:
two-stage reward normalization & Bayesian exploration ([BayesianRouter][br],
cross-domain), baseline-relative bias updates & quota-aware preference
([BaRP][barp]), quality-triggered cascade ([FrugalGPT][fgpt] — the
strongest-endorsed work in this list, 1300+ citations), replay-validated
strategy changes ([MERA][mera]), and capability-card distillation
([FlyRoute][flyroute]). The flywheel only fine-tunes — every learned quantity
is clamped (bias ∈ [0.7, 1.3], cascade thresholds per chain position, distill
shift ≤ 0.15) and every strategy change must pass `evo-router backtest` on
replayed production traffic before adoption.

```bash
evo-router backtest              # A/B replay two strategy params on events.jsonl (PGR verdict)
```

[br]: https://arxiv.org/abs/2510.02850
[barp]: https://arxiv.org/abs/2510.07429
[fgpt]: https://arxiv.org/abs/2305.05176
[mera]: https://arxiv.org/abs/2608.10333
[flyroute]: https://arxiv.org/abs/2605.22057

Deep dives: [Architecture](docs/ARCHITECTURE.md) · [Decision flow](docs/DECISION-FLOW.md) · [Scoring factors](docs/FACTORS.md) · [Papers](docs/reference.md) *(Chinese, English translation welcome)*


## Development

```bash
cargo build --release
cargo test          # 111 tests
cargo clippy        # zero warnings policy
```

Workspace layout:

| Crate | Role |
|---|---|
| `evolve-core` | Decision engine, scoring, catalog, plan registry, config, distillation |
| `evolve-decision` | Decision-model backends (cloud or self-hosted, laya, heuristic) + route advisor |
| `evolve-discovery` | Agent config scanning, remote /models fetching, models.dev enrichment |
| `evolve-memory` | Flywheel, quota ledger, health registry, session store, event log, strategy & backtest |
| `evolve-server` | Axum gateway, relay, dashboard, provider APIs, CLI (binary: `evo-router`) |

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.


## License

[MIT](LICENSE)
