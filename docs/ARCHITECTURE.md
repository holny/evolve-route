# EvolveRouter 架构总览

> 完整源码（5 个 crate / 11,081 行 Rust / 200 个公开函数 / 93 个测试 / 单二进制 ~6.5MB）。
> 本目录是设计文档：架构 / 决策 / 因子 / 模块。

## 三层视图

```
┌─────────────────────────────────────────────────────────────────────┐
│  Agent 层（不做任何修改，零适配接入）                                  │
│  opencode │ claude-code │ codex │ pi │ 任何 OpenAI/Anthropic 客户端   │
│     │ opencode 原生发 x-opencode-session 头；其余走原生协议            │
└─────┬───────────────────────────────────────────────────────────────┘
      ▼ http://127.0.0.1:8787
┌──────────────────────── ev-server（网关层）──────────────────────────┐
│  /v1/chat/completions (OpenAI 入口 relay.rs)                          │
│  /v1/messages         (Anthropic 入口 anthropic.rs)                   │
│  ├─ identity.rs    身份提取：agent(UA/originator) + session(4 级回退) │
│  ├─ 预处理：解析 → 特征提取 → token 估算 → digest                    │
│  ├─ Engine::decide 路由决策（ev-core）                                │
│  ├─ 降级链转发：key 池轮换 + 协议翻译 + 三级熔断                   │
│  ├─ stream.rs      流式遥测：TTFT/速率/工具捕获/质量分析              │
│  └─ finalize       事件落盘 + 飞轮学习 + 预算记账 + SSE 广播         │
│  meta.rs           /api/stats /api/plans /api/trends /api/providers  │
│  service.rs        evo-router service start/stop/restart              │
└─────┬──────────────────┬──────────────────┬─────────────────────────┘
      ▼                  ▼                  ▼
┌───────────┐   ┌───────────────┐   ┌──────────────────┐
│ ev-core   │   │ ev-memory     │   │ ev-discovery     │
│ 路由决策核心│   │ 持久化/学习    │   │ 目录四源发现      │
├───────────┤   ├───────────────┤   ├──────────────────┤
│ engine.rs │   │ flywheel.rs   │   │ opencode.rs 扫配置│
│ scoring.rs│   │  sessions.rs  │   │ remote.rs /models│
│ catalog.rs│   │  quota.rs     │   │ codex.rs / rest  │
│ plans.rs  │   │  health.rs    │   │ benchmarks.rs    │
│ types.rs  │   │  events.rs    │   │ modelsdev.rs     │
│ heuristic │   │               │   │                  │
└───────────┘   └───────────────┘   └──────────────────┘
      ▼
┌─────────────────────────────────────────────────────────────────────┐
│ 上游 LLM Provider（5+ 家）                                           │
│ zhipu Coding Plan / volces Coding+Agent Plan / MiniMax Coding Plan   │
│ opencode Go（Token Plan）/ z.ai Devpack / deepseek API               │
└─────────────────────────────────────────────────────────────────────┘
```

## 三条铁律（贯穿设计）

1. **会话内容不可变** — 协议翻译只外科改写路由字段，字节流透传
2. **决策失败永不阻塞路由** — Jev 超时/失败 → heuristic 兜底 → 公式兜底
3. **redact=true 原文不出机器** — 决策特征只发分桶/占位

## 关键文件导航

| 文件 | 职责 |
|---|---|
| `crates/ev-server/src/main.rs` | CLI + 启动入口 |
| `crates/ev-server/src/relay.rs` | OpenAI 协议入口 + 决策转发 |
| `crates/ev-server/src/anthropic.rs` | Anthropic 协议入口（与 relay 镜像） |
| `crates/ev-server/src/identity.rs` | 4 家 agent 原生会话头识别 |
| `crates/ev-server/src/translate.rs` | 跨协议格式翻译（外科式） |
| `crates/ev-server/src/stream.rs` | 流式遥测 + finalize（TTFT/速率/质量分析） |
| `crates/ev-server/src/meta.rs` | 面板 API（stats/plans/trends/providers） |
| `crates/ev-server/src/state.rs` | AppState + 定期目录扫描/远程重拉后台任务 |
| `crates/ev-server/src/service.rs` | CLI 服务管理子命令 |
| `crates/ev-core/src/engine.rs` | 决策管线（粘性/漏斗/探索/门控） |
| `crates/ev-core/src/scoring.rs` | 五因子评分 + 难度联动权重 + 可靠性分层融合 |
| `crates/ev-core/src/catalog.rs` | 四源目录合并 + 套餐级 tier 注入 |
| `crates/ev-core/src/plans.rs` | 套餐注册表 + 全局模型能力表 |
| `crates/ev-memory/src/flywheel.rs` | 四层反馈学习 + ReqSample 窗口环 |
| `crates/ev-memory/src/sessions.rs` | 粘性 + L3 反馈匹配 |
| `crates/ev-memory/src/quota.rs` | 限额头 + 套餐用量双账本 |
| `crates/ev-memory/src/health.rs` | 健康冷却账本（含重置时间） |
| `crates/ev-decision/src/backend.rs` | HybridBackend（双判官融合） |
| `crates/evolve-decision/src/decision_backend.rs` | 决策模型判定（System One 协议，可指向云端或本地服务） |
| `crates/ev-decision/src/heuristic.rs` | 本地启发式词表引擎 |
| `crates/ev-discovery/src/opencode.rs` | opencode 配置扫描 |
| `crates/ev-discovery/src/remote.rs` | 远端 /models 拉取 |
| `crates/ev-discovery/src/codex.rs` | Codex CLI 配置发现 |
| `crates/ev-discovery/src/agents_rest.rs` | agents REST 探测 |
| `crates/ev-discovery/src/modelsdev.rs` | models.dev 参考数据 |
| `adapters/opencode/dropin/evo-router.ts` | opencode 插件（已不必要：opencode 原生 x-opencode-session） |
</content>
