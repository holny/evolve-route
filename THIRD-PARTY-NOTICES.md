# Third-Party Notices

This project includes code and depends on the following third-party components:

## NVIDIA-NeMo/Switchyard (planned, M1+ integration)

- Source: https://github.com/NVIDIA-NeMo/Switchyard
- License: Apache License 2.0
- Copyright (c) 2024-2026 NVIDIA CORPORATION & AFFILIATES
- Usage: wire-format codecs (OpenAI <-> Anthropic), protocol types, and LLM
  client transport crates are planned for embedding from M1/M3 onward.
- When embedded binaries are distributed, the Switchyard LICENSE and NOTICE
  texts ship alongside this file.

## Crates.io dependencies

Runtime dependencies are declared in the workspace Cargo.toml files and are
distributed under MIT / Apache-2.0 / BSD terms. Run `cargo license` for a
full per-crate listing. Notable ones:

- axum, tokio, hyper, tower: MIT
- reqwest, hyper-rustls, rustls: MIT / Apache-2.0
- serde, serde_json: MIT / Apache-2.0
- clap: MIT / Apache-2.0
- sha2, hex: MIT / Apache-2.0

## Upstream references (research baseline, not redistributed)

- TypeSafe (System One decision model API): https://docs.typesafe.ai
- laya (multilingual System 1 decision engine, Apache-2.0):
  https://github.com/NandhaKishorM/laya
