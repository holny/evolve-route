<div align="center">

# EvolveRoute

**本地优先的 LLM 路由网关，内建决策模型，每次请求都在进化。**

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org)
[![Tests](https://img.shields.io/badge/tests-94%20passing-brightgreen.svg)](#开发)

[English](README.md) | [中文](README.zh-CN.md)

</div>

---

**EvolveRoute** 是一个单二进制网关，架在你的编码 Agent 与 LLM Provider 之间。把各 Agent 指向 `http://127.0.0.1:8787` 并使用 `model = "auto"`——对每个请求，**决策模型**实时判定任务（领域、难度、视觉、工具密度、风险……），路由到最**合适**的模型：闲聊落便宜模型、复杂重构落强模型、**大上下文绝不进小窗口模型**。全程可观测：为什么路由、路由到哪、花了多少。

它**本地优先**（路由不依赖云服务）、**懂经济账**（订阅配额、积分系数、每模型美元限额）、**自进化**（飞轮从每次结果中学习，自动改写自己的评分偏置）。

![EvolveRoute 架构](docs/assets/architecture.svg)

*各 Agent 指向单一端点 `model = "auto"`。双判官（TypeSafe Jev + 启发式）逐请求评分；飞轮把每次结果喂回排名；每个 Provider 方案按各自货币计价与预算。[编辑源文件](docs/assets/architecture.excalidraw)。*

## 三大支柱

1. **智能决策，而非前缀规则。** 每个请求都由决策模型——**[TypeSafe Jev](https://github.com/typesafe-ai)**——判定 8 维信号（领域、难度、视觉、trivial、工具密度、高风险、会话深度、体量）。内置多语言启发式判官作为独立第二意见（取严融合，补 Jev 的中文短板）；Jev 不可达时启发式接管，**路由永不阻塞**。可选 `laya` 后端插入同一接口。
2. **资金感知路由。** 每个候选先用其方案自己的货币计价——积分（智谱 / Z.ai / MiniMax）、美元（opencode zen Go）、AFP（火山）——再进入评分。配额预算三层保护：额度消耗 **>60%** 降权、**>85%** 断开会话粘性、**>95%** 直接出局——把余量留给真正需要的任务。
3. **大模型方案，一个面板管控。** 厂商订阅方案是一等公民：档位、额度、积分系数、每模型美元限额全部内置；面板按窗口展示 Provider 实测配额，一键对账。

## 内置方案

| 方案 | 厂商 | 类型 | 计价模型 | 档位 | 窗口 |
|---|---|---|---|---|---|
| `zhipu-coding` | 智谱 (bigmodel.cn) | Coding Plan | 积分，闲时 5 折 | lite / pro / max | 5h · 周 · 月 |
| `zai-devpack` | Z.ai | Coding Plan | 积分 | lite / pro / max | 5h · 周 · 月 |
| `opencode-go` | opencode zen | Go Plan | 每模型 $/1M + 每模型月度美元限额 | go / go-plus | 月（5h = 20%） |
| `volces-coding` | 火山方舟 | Coding Plan | AFP 积分 | lite / pro | 5h · 周 · 月 |
| `volces-agent` | 火山方舟 | Agent Plan | AFP 积分 | — | 5h · 周 · 月 |
| `minimax-coding` | MiniMax | Coding Plan | 积分 | lite / pro / max | 5h · 周 · 月 |
| 按量计费 | 任意 OpenAI / Anthropic | API | $ / token | — | — |

方案按 base URL 自动识别；每个窗口（5h / 周 / 月）均有跟踪，厂商下发配额头时以 Provider 实测为准。

## 为什么是 EvolveRoute

| | EvolveRoute | LiteLLM / OpenRouter | RouteLLM |
|---|---|---|---|
| **路由决策** | 决策模型**逐请求实判**（8 维信号）+ 启发式第二意见，判败不阻塞 | 配置/前缀规则，或离线训练的模型路由 | 离线训练路由模型 |
| **自进化** | 飞轮：四层反馈（传输/工具语法/语义回路/插件）自动改写模型级学习偏置与可靠性 | 静态权重 | 训练后即静态 |
| **订阅经济学** | 方案级积分系数、档位额度（5h/周/月）、配额预算三层保护（降权→断粘性→硬过滤） | 仅成本记录 | – |
| **部署** | 单 Rust 二进制（~7MB），零依赖，无 Docker/数据库 | Python + 数据库 | Python |
| **协议** | OpenAI ↔ Anthropic 双向翻译（含流式） | OpenAI 为主 | OpenAI |
| **多 Agent** | 原生识别 opencode / codex / claude-code / pi（会话感知粘性） | 通用 | 通用 |
| **故障处理** | 多 key 池化 → provider/端点级熔断 → 降级链 → **链外兜底扫描** | 重试 | – |

## 功能

- **决策模型参与路由**——判官模型对每个请求 8 维打分（领域/难度/视觉/trivial/工具密度/高风险/会话深度…），与多语言启发式判官融合（双判官取严）。决策失败永不阻塞路由。
- **两阶段评分**——先硬约束（上下文窗口含余量/视觉模态/凭据/已验证最大接受），再质量及格线，再复合权重（质量/速度/成本/稳定/余量）且**权重随难度联动**：简单任务偏便宜模型，难度升则解锁强模型。
- **飞轮**——每次请求都反馈：传输成败、工具调用语法合法性、会话确认的语义回路、插件上报结果。模型级 learned bias、实测可靠性（长期 50% + 近 30 次 30% + 近 10 次 20%）、token 校准自动进化排名。
- **订阅经济学**——内置方案注册表（智谱 coding、z.ai、opencode zen、火山 coding/agent、MiniMax、按量 API）含官方积分系数与档位额度。配额预算三层保护：>60% 降权、>85% 断粘性、>95% 出局——把余量留给硬任务。
- **故障韧性**——多 key 池化（model×key 记账）、provider 账户级与端点级熔断、排名降级链、上下文溢出重路由、全链灭时的**链外兜底扫描**。
- **跨协议**——OpenAI 入口请求透明翻译到 Anthropic 上游（响应回译），含流式事件映射与确定性 ID。Claude Code 开箱即用。
- **会话粘性**——稳定会话复用已选模型（零重判延迟）且有轮次上限；任务变化、预算压力、上下文暴涨自动断开。ε-greedy 探索（10%）持续采样次优模型，飞轮永不挨饿。
- **可观测**——内置零构建面板（`/`）：实时决策流（含大白话归因）、评分矩阵、逐跳原因的降级链、耗时瀑布（预处理/决策/TTFT/流式）、模型趋势、Provider 与配额管理、飞轮学习状态——6 种语言。另有 `x-ev-model / x-ev-reason / x-ev-decision-id` 响应头与 JSONL 事件日志。

## 快速开始

**要求：** Rust stable（2024 edition），任意 OpenAI 兼容或 Anthropic Provider 凭据。

```bash
git clone https://github.com/your-org/evolve-route.git
cd evolve-route
cargo build --release

# 二进制在 target/release/evolveroute
./target/release/evolveroute serve
# 网关监听 http://127.0.0.1:8787
```

配置位于 `~/.evolveroute/evolveroute.toml`（内置默认配置，首次运行自动写出）。填入 Provider 的 base URL 与 API key：

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
cost = { input = 0.0, output = 0.0 }   # 订阅方案：边际成本 ≈ 0
```

### 接入你的 Agent

**opencode**（`~/.config/opencode/opencode.jsonc`）：

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

**Claude Code：**

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8787
export ANTHROPIC_MODEL=auto
claude
```

**任意 OpenAI 兼容客户端：** base URL `http://127.0.0.1:8787/v1`，model `auto`。

### 以服务方式运行（macOS LaunchAgent）

```bash
evolveroute service install   # 另有：start / stop / restart / status
```

## 面板

打开 `http://127.0.0.1:8787/`：

![EvolveRoute 面板](docs/assets/dashboard.png)

- **实时决策流**——每个请求的 8 维判定、所选模型、大白话归因
- **评分矩阵**——头部候选的分因子得分（质量/速度/成本/可靠/余量）
- **降级链**——试了哪些模型、挂在哪、为什么
- **耗时瀑布**——预处理/决策/TTFT/流式，逐请求
- **Provider 管理**——增删/扫描 Provider、档位额度（5h/周/月）、Provider 实测配额、一键对账
- **飞轮卡**——各模型的学习偏置、实测可靠性、校准值、语义确认数

## 路由决策原理

```
请求 → token 估算 → 硬约束（窗口/视觉/凭据）
    → 决策模型判定（8 维，启发式兜底）
    → 质量及格线 → 复合评分（难度联动权重）
    → ε-greedy 探索（10%）→ 会话粘性检查
    → 路由 → 结果观测 → 喂给飞轮
```

深入阅读：[架构](docs/ARCHITECTURE.md) · [决策流](docs/DECISION-FLOW.md) · [评分因子](docs/FACTORS.md) · [模块](docs/MODULES.md)

## 路由决策公式
![EvolveRoute formula](docs/assets/formula-zh.png)
## 开发

```bash
cargo build --release
cargo test          # 94 个测试
cargo clippy        # 零警告策略
```

Workspace 结构：

| Crate | 职责 |
|---|---|
| `ev-core` | 决策引擎、评分、目录、方案注册表、配置 |
| `ev-decision` | 决策模型后端（TypeSafe Jev、laya、启发式）+ 协议翻译 |
| `ev-discovery` | Agent 配置扫描、远程 /models 拉取、models.dev 补全 |
| `ev-memory` | 飞轮、配额账本、健康注册表、会话存储、事件日志 |
| `ev-server` | Axum 网关、relay、面板、Provider API、CLI |

贡献指南见 [CONTRIBUTING.md](CONTRIBUTING.md) / [CONTRIBUTING.zh-CN.md](CONTRIBUTING.zh-CN.md)。

## 许可证

[MIT](LICENSE)
