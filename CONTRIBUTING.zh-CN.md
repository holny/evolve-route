# 参与 EvolveRoute 贡献

[English](CONTRIBUTING.md) | [中文](CONTRIBUTING.zh-CN.md)

感谢你有兴趣改进 EvolveRoute！本文档涵盖上手所需的全部内容。

## 开发环境

**要求：**

- Rust stable（项目使用 2024 edition——版本落后先 `rustup update stable`）
- 无数据库、无 Docker、无 Python。一切皆 `cargo`。

```bash
git clone https://github.com/your-org/evolve-route.git
cd evolve-route
cargo build            # debug 构建，快速迭代
cargo test             # 全量测试（94 个）
cargo clippy           # CI 门禁：零警告
cargo fmt              # 保存即 rustfmt
```

## 项目结构

```
crates/
├── ev-core        # 决策引擎、评分、目录、方案注册表、配置
├── ev-decision    # 决策模型后端（TypeSafe Jev / laya / 启发式）+ 协议翻译
├── ev-discovery   # Agent 配置扫描、远程 /models、models.dev 补全
├── ev-memory      # 飞轮、配额账本、健康注册表、会话存储、事件日志
└── ev-server      # axum 网关、relay、面板、Provider API、CLI
```

数据流：`ev-server` 收到请求 → `ev-core` 决策 → `ev-server` 转发 → `ev-memory` 记录结果 → 反哺 `ev-core` 下一次决策。加功能先想清楚归属：影响路由决策的逻辑放 `ev-core`；触碰 Provider HTTP 的放 `ev-server`/`ev-discovery`；持久化学习状态的放 `ev-memory`。

## 铁律

### 1. 会话内容不可变

网关绝不改写用户会话内容。请求改写仅限外科手术式字段编辑（`model` 字段），其余字节逐字节透传。如果你的改动需要触碰消息内容，先停下来开 issue——这是设计不变量，不是偏好。

### 2. 决策失败永不阻塞路由

一切可能失败的路径（决策模型不可达、判官超时、翻译错误）都必须有兜底，仍然产出路由决策。新增失败模式时，兜底必须在同一个 PR 里。

### 3. 隐私：redact 模式必须成立

`[telemetry] redact = true` 时，任何请求内容不得离开本进程——决策特征走分桶/分类，不发原文。新增遥测字段必须尊重这个开关。新遥测的测试要覆盖 redact 路径。

### 4. 测试

- 新行为需要测试。路由改动：在 `crates/ev-server/tests/proxy.rs` 加用例（自带 mock 上游，覆盖故障转移、熔断、跨协议）。
- 评分改动：在 `ev-core` 单测里断言排序/选择结果，不是"能编译"。
- Bug 修复：先写失败测试复现，再修。
- `cargo clippy` 必须零警告；推送前 `cargo fmt`。

### 5. 展示层算术用 f64

Rust `f32` 序列化有精度毛刺（如 `1.4000000000000001`）。所有进面板/API 的值在 f64 域取整：`(v * 100.0).round() / 100.0`。不要把裸 f32 带进 JSON 响应。

### 6. axum 陷阱

`Router::layer(...)` 只包住**已注册**的路由。中间件（如 `DefaultBodyLimit`）放在路由注册之前会静默失效。先注册路由，后挂 layer。

## Pull Request

1. Fork，从 `main` 拉分支（`feat/xxx`、`fix/xxx`）。
2. PR 保持聚焦——一个行为变更一个 PR。发现无关 bug，开 issue 而不是顺手夹带。
3. 提交信息遵循 [Conventional Commits](https://www.conventionalcommits.org)：`feat:`、`fix:`、`refactor:`、`docs:`、`test:`。标题 ≤ 72 字符，上下文写进正文。
4. CI 必须通过：build + test + clippy。
5. 描述**为什么**，不只是改了什么。面板类改动欢迎截图。

## Issue

提 bug 请附：

- EvolveRoute 版本（`evolveroute --version` 或 commit hash）
- 操作系统与运行方式（二进制 / LaunchAgent / systemd）
- `~/.evolveroute/gateway.log` 尾部日志；如有 `x-ev-*` 响应头一并附上
- 配置片段——**粘贴前隐去 API key 与请求内容**

功能请求：描述你遇到的路由问题，而不是直接给方案。"我的订阅配额在简单任务上烧太快"比"加个限流"更有信息量。

## AI 辅助贡献

欢迎 AI 辅助的贡献，**但须披露**：在 PR 描述中注明哪些部分由 AI 生成。维护者对 AI 生成代码一视同仁地严格审查——未经审阅、未经验证的 AI 产出会因流程问题被拒，而非风格问题。

## 设计决策

重大路由/评分改动先在 issue 里写设计说明（问题 → 备选 → 抉择与取舍），再动代码。既有决策记录在 `docs/`——动评分管线前先读 [DECISION-FLOW.md](docs/DECISION-FLOW.md) 与 [FACTORS.md](docs/FACTORS.md)。

## 许可证

提交贡献即表示同意你的贡献以 [MIT](LICENSE) 许可证发布。
