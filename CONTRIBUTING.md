# Contributing to EvolveRouter

[English](CONTRIBUTING.md) | [中文](CONTRIBUTING.zh-CN.md)

Thanks for your interest in improving EvolveRouter! This document covers everything you need to get productive.

## Development setup

**Requirements:**

- Rust stable (the project uses the 2024 edition — `rustup update stable` if you're behind)
- No database, no Docker, no Python. Everything is `cargo`.

```bash
git clone https://github.com/holny/evolve-router.git
cd evolve-router
cargo build            # debug build, fast iteration
cargo test             # full suite (94 tests)
cargo clippy           # CI gate: zero warnings
cargo fmt              # rustfmt on save
```

## Project layout

```
crates/
├── ev-core        # decision engine, scoring, catalog, plan registry, config
├── ev-decision    # decision-model backends (TypeSafe Jev / laya / heuristic) + translation
├── ev-discovery   # agent config scanning, remote /models, models.dev enrichment
├── ev-memory      # flywheel, quota ledger, health registry, sessions, event log
└── ev-server      # axum gateway, relay, dashboard, provider APIs, CLI
```

Data flow: `ev-server` receives a request → `ev-core` decides → `ev-server` relays → `ev-memory` records the outcome → `ev-core`'s next decision is shaped by it. When adding a feature, figure out which crate owns it — logic that shapes routing decisions belongs in `ev-core`; anything touching provider HTTP in `ev-server`/`ev-discovery`; anything that persists learning state in `ev-memory`.

## Ground rules

### 1. Session content is immutable

The gateway must never rewrite user conversation content. Request rewriting is limited to surgical field edits (the `model` field); everything else passes through byte-for-byte. If your change needs to touch message content, stop and open an issue first — this is a design invariant, not a preference.

### 2. Decision failure never blocks routing

Every path that can fail (decision model unreachable, judge timeout, translation error) must have a fallback that still produces a routing decision. If you add a new failure mode, add its fallback in the same PR.

### 3. Privacy: redact mode must hold

When `[telemetry] redact = true`, no request content may leave the process — decision features are bucketed/classified, never raw. Any new telemetry field must respect this switch. Tests for new telemetry should cover the redacted path.

### 4. Tests

- New behavior needs tests. Routing changes: add a case to `crates/ev-server/tests/proxy.rs` (it has mock upstreams for failover, circuit breaking, cross-protocol).
- Scoring changes: assert on ordering/selection in `ev-core` unit tests, not just "it compiles".
- Bug fixes: reproduce with a failing test first, then fix.
- `cargo clippy` must be warning-free; `cargo fmt` before pushing.

### 5. F64 for display math

Rust `f32` serialization produces precision artifacts (e.g. `1.4000000000000001`). All values destined for the dashboard/API are rounded in the f64 domain: `(v * 100.0).round() / 100.0`. Don't introduce raw f32 into JSON responses.

### 6. Axum gotcha

`Router::layer(...)` only wraps **already-registered** routes. Middleware (e.g. `DefaultBodyLimit`) placed before route registration fails silently. Register routes first, layer after.

## Pull requests

1. Fork, create a branch from `main` (`feat/xxx`, `fix/xxx`).
2. Keep PRs focused — one behavior change per PR. If you found an unrelated bug, file an issue instead of sneaking it in.
3. Commit messages follow [Conventional Commits](https://www.conventionalcommits.org): `feat:`, `fix:`, `refactor:`, `docs:`, `test:`. Keep the subject ≤ 72 chars; put context in the body.
4. CI must pass: build + test + clippy.
5. Describe **why**, not just what. Screenshots for dashboard changes are appreciated.

## Issues

When filing a bug, include:

- EvolveRouter version (`evo-router --version` or commit hash)
- Your OS and how you run the gateway (binary / LaunchAgent / systemd)
- The relevant `~/.evolve/gateway.log` tail and `x-ev-*` response headers if you have them
- Config snippet — **redact API keys and request content before pasting**

Feature requests: describe the routing problem you're hitting, not just the solution. "My subscription quota burns too fast on easy tasks" is more actionable than "add rate limiting".

## AI-assisted contributions

AI-assisted contributions are welcome, **with disclosure**: note in the PR description which parts were AI-generated. The maintainer reviews AI-generated code with the same rigor — unreviewed, untested AI output will be rejected on process grounds, not style grounds.

## Design decisions

Substantial routing/scoring changes start as a design note in an issue (problem → options → chosen tradeoff) before code. The existing decision record lives in `docs/` — read [DECISION-FLOW.md](docs/DECISION-FLOW.md) and [FACTORS.md](docs/FACTORS.md) before proposing changes to the scoring pipeline *(Chinese, English translation PRs welcome)*.

## License

By contributing, you agree that your contributions are licensed under [MIT](LICENSE).
