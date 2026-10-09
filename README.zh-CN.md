<div align="center">

# EvolveRoute

**本地优先的 LLM 路由网关，内建决策模型，每次请求都在进化。**

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org)
[![Tests](https://img.shields.io/badge/tests-94%20passing-brightgreen.svg)](#开发)

[English](README.md) | [中文](README.zh-CN.md)

</div>

---

**EvolveRoute** 是一个本地部署的智能路由网关，架在你的编码 Agent 与所有 LLM Provider 之间。它不把每个请求都发给「最强」的模型——而是**逐请求实时判定，送到此刻最合适的那一个**：

- **任务画像**：决策模型（[TypeSafe Jev](https://github.com/typesafe-ai) + 多语言启发式第二意见，取严融合）按 8 维信号判定（领域、难度、视觉、工具密度、高风险…）
- **方案画像**：每个候选按其方案自己的货币先计价再评分（积分/美元/AFP），叠加订阅配额窗口和 5 因子科学公式
- **可归因**：每条边、每个分、每道降级都带名带值，全程可审计

一个 Rust 单二进制装在本地，**用户数据不离开主机**，决策日志全量留痕，飞轮从每次结果中学习、越用越准。
## 功能

1. **本地部署，数据安全** — 一个 Rust 单二进制（~7 MB），零运行时依赖，无 Docker、无 Python、无云。整条路由链全在主机内：用户数据、会话内容、遥测信号**不出网**。每一次决策落入本地 JSONL 事件日志——审计、迁移、删除全部可控。
2. **决策模型智能判断，懂经济账** — TypeSafe Jev 主判 + 多语言启发式独立第二意见（取严融合，补 Jev 在 CJK 场景的盲区），判 8 维信号；Jev 不可达时启发式接管，**路由永不阻塞**。每个候选评分前先按**其方案自有货币**计价（智谱 / Z.ai / MiniMax 积分、opencode Go 美元、volces AFP），再复合质量 / 速度 / 成本 / 可靠 / 余量五因子。三层预算保护：>60% 降权、>85% 断粘性、>95% 出局。
3. **科学公式归因** — 5 因子 + 难度联动权重 + 飞轮偏置，
4. **自进化飞轮** — 每次请求喂入飞轮，4 层反馈（传输成败 / 工具调用语法 / 会话语义回路 / 插件显式上报）改写各模型的 learned bias、可靠性、token 校准——长记忆 + 短期 + 即时三层融合（50% / 30% / 20%）。**用量越大、路由越准**。
5. **多 Agent 多 Provider, OpenAI ↔ Anthropic 无损** — opencode / Claude Code / Codex / Pi 任一接入（OpenAI 或 Anthropic 协议），路由到任何 OpenAI 兼容 **或** Anthropic 上游；Switchyard **双向流式翻译**（含事件映射与确定性 ID），Claude Code 开箱即用。订阅方案（zhipu coding / Z.ai / opencode zen / volces / MiniMax）与按量 API 共用同一端点。
6. **极速决策，零依赖部署** — 单 Rust 二进制 6.7 MB；冷启动 <30 ms；单请求仅多 <300 ms（一次 Jev 评分 + 启发式兜底）。多 key 池化轮换、provider / 端点双层熔断、链外兜底扫描——单家 Provider 全挂也不会整体哑掉。
7. **本地面板管控，全程决策可见** — 零构建面板 `http://127.0.0.1:8787`：实时决策流（每行附大白话归因）、评分矩阵、降级链与每跳剔除原因、耗时瀑布、模型趋势、Provider 与配额管理、飞轮学习状态。所有响应携带 `x-ev-model / x-ev-reason / x-ev-decision-id` 头；`/api/events` 暴露 JSONL 决策日志，可查询可回放。

<div align="center">

# EvolveRoute

**本地优先的 LLM 路由网关，内建决策模型，每次请求都在进化。**

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org)
[![Tests](https://img.shields.io/badge/tests-94%20passing-brightgreen.svg)](#开发)

[English](README.md) | [中文](README.zh-CN.md)

</div>

---

**EvolveRoute** 是一个本地部署的智能路由网关，架在你的编码 Agent 与所有 LLM Provider 之间。它不把每个请求都发给「最强」的模型——而是**逐请求实时判定，送到此刻最合适的那一个**：

- **任务画像**：决策模型（[TypeSafe Jev](https://github.com/typesafe-ai) + 多语言启发式第二意见，取严融合）按 8 维信号判定（领域、难度、视觉、工具密度、高风险…）
- **方案画像**：每个候选按其方案自己的货币先计价再评分（积分/美元/AFP），叠加订阅配额窗口和 5 因子科学公式
- **可归因**：每条边、每个分、每道降级都带名带值，全程可审计

一个 Rust 单二进制装在本地，**用户数据不离开主机**，决策日志全量留痕，飞轮从每次结果中学习、越用越准。

