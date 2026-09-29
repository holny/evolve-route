# ModelRoute

**多 Agent LLM 智能路由网关** —— 把任务交给最适合的模型，而不是最好的模型。

一个本地单二进制网关：各编码 Agent（opencode / codex / claude-code / pi / dsh / openclaw / hermes）把模型指向 `http://127.0.0.1:8787` 并使用 `model = "auto"`，网关按请求实时决策——简单问答落便宜模型、复杂重构落强模型、**大上下文绝不进小窗口模型**，并全程可观测（为什么路由、路由到哪、花了多少）。

## 当前状态：M1 已交付

- ✅ OpenAI 协议网关（`/v1/chat/completions` 流式/非流式透传）
- ✅ heuristic 决策引擎：8 信号判定 + 两阶段评分（质量及格线 → 成本/速度决胜）
- ✅ 硬约束安全网：上下文窗口（含 10% 余量）/ 视觉模态 / 凭据 / 未知窗口禁入
- ✅ 会话粘性：同会话零决策调用复用（防抖动、省成本），任务变化自动解除
- ✅ 外科手术式字节改写：仅改 `model` 字段，请求其余字节逐字节保留（会话内容不可变铁律）
- ✅ 可观测：`x-mr-model / x-mr-reason / x-mr-decision-id` 响应头 + JSONL 决策日志（TTFT/usage/缓存命中捕获）
- ✅ 多语言 heuristic：语言无关信号（代码密度/工具/长度）为主力 + ASCII 技术词跨语言命中 + 多脚本日常词表
- ✅ M2：飞轮（token 校准/实测可靠性/决策回填，snapshot 持久化）+ TypeSafe Jev / laya 决策后端 + Web 面板（/ 内嵌，SSE 实时）+ stats CLI
- ✅ M3（提前）：discovery（opencode/codex）+ 死模型侦测（402/404/429/配额冷却）+ 同请求降级链 + opencode 原生插件 + 用户权重/学习偏置
- ✅ M3：Anthropic 入口（/v1/messages + count_tokens 实装）+ **Switchyard 跨协议双向翻译**（OpenAI↔Anthropic，含流式事件映射与确定性 ID）——claude-code 已可接入，7 家 Agent 全通
- ✅ M4：**多 key 池化**（model×key 记账，402/配额自动轮换）+ **配额窗口账本**（成功响应头学习，`/api/quota`，引擎余量预判拦截）+ **models.dev 参考元数据**（仅补缺失字段）+ discovery 补齐（openclaw/hermes/dsh）
- ⏳ M4 余项：pi/dsh/openclaw/hermes 原生插件（当前为配置发现接入）

## claude-code 接入

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8787
export ANTHROPIC_MODEL=auto        # 走智能路由
claude
```
网关自动：anthropic 请求 → （跨协议翻译）→ 任意 OpenAI 兼容上游；响应回译为 anthropic 形状。

## opencode 插件

零依赖 drop-in：`cp adapters/opencode/dropin/modelroute.ts <项目>/.opencode/plugins/`
网关自动获得精确会话粘性 + 工具成败真值上报（飞轮 L4 信号）。`MODELROUTE_FEEDBACK=0` 可关。

## 多 key 池化与配额窗口

```toml
[[models]]
id = "claude"
api_keys_env = ["ANTHROPIC_KEY_1", "ANTHROPIC_KEY_2"]   # 轮换池
```
某 key 余额不足/配额耗尽 → 网关按 model×key 记账冷却并自动轮换下一把；
成功响应的限额头（anthropic unified 5h/7d、openai ratelimit）持续校准窗口余量，
余量 < 请求预估时该模型在硬约束层被预判拦截。`/api/quota` 查看窗口账本。

## 决策后端

```toml
[decision]
backend = "auto"      # 探测 TYPESAFE_API_KEY → Jev；否则 heuristic
backend = "laya"      # 本地 laya sidecar（scripts/laya_server.py，uvicorn --port 8321）
```
判定失败自动回退 heuristic，永不阻塞请求。

## 快速开始

```bash
cargo build
# 1. 起一个假上游（演示用，真实使用时换成各家 provider）
./target/debug/modelroute mock-upstream --port 9101
# 2. 起网关（默认读取 ./modelroute.toml 或 ~/.modelroute/modelroute.toml）
./target/debug/modelroute serve --port 8787
```

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'x-mr-session: my-session' \
  -d '{"model":"auto","messages":[{"role":"user","content":"你好"}]}'
# 响应头: x-mr-model: mini
#         x-mr-reason: domain=Chitchat diff=0.4 est=5tok -> mini | filtered: ...
```

## 配置模型目录

编辑 `modelroute.toml`（模板见 `config/modelroute.default.toml`）：

```toml
[[models]]
id = "deepseek-chat"
provider = "deepseek"
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"
context_window = 64000
cost = { input = 0.27, output = 1.1 }        # $/Mtok
tiers = { reasoning = 0.45, coding = 0.8, vision = 0.0, agentic = 0.75 }
speed_tier = 0.85
```

`modelroute models` 查看目录（含来源标记）；`modelroute doctor` 体检配置与凭据。

## 决策管线（一句话版）

四本账（粘性/缓存/配额/摘要）→ 粘性快路径 → 8 信号判定（heuristic 兜底，M2 升级 Jev/laya）→ 硬约束过滤 → 质量及格线过滤 → 成本/速度/可靠性复合评分 → 置信门控 → 决策+自然语言解释 → 执行 → 遥测回写飞轮。

完整设计文档（21 项决策记录、架构、模块设计）见设计评审稿；关键原则：

1. **会话内容不可变**：同协议仅外科改写路由字段；跨协议翻译语义 1:1 且字节确定性；永不摘要/截断/压缩。
2. **安全网优先于智能**：上下文装不下就换模型，而不是压缩内容。
3. **缓存是货币化考虑项而非壁垒**：换模滞回按真金白银折算，hard/high-stakes 任务清零。
4. **决策全程可观测**：每条路由带 reason，事件全落本地 JSONL。

## 基线

```
scripts/bench.sh   # 网关新增延迟（debug 构建, 100 req）
direct upstream:    p50=0.33ms
via gateway:        p50=0.64ms
added latency:      0.31ms  (budget p50 < 5ms: PASS)
```

## 接入各 Agent（M1 手工配置）

### opencode

```jsonc
// opencode.json
{
  "provider": {
    "modelroute": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "ModelRoute",
      "options": { "baseURL": "http://127.0.0.1:8787/v1" },
      "models": {
        "auto": { "name": "Auto (smart routing)" }
      }
    }
  },
  "model": "modelroute/auto"
}
```

### codex

```toml
# ~/.codex/config.toml
model = "auto"
model_provider = "modelroute"

[model_providers.modelroute]
name = "ModelRoute"
base_url = "http://127.0.0.1:8787/v1"
wire_api = "chat"
env_key = "MODELROUTE_KEY"   # 任意非空值即可，网关不校验
```

claude-code 接入需 Anthropic 协议入口（M3）。pi/dsh/openclaw/hermes 同理走 OpenAI 兼容 baseURL，M4 提供逐家文档与 discovery 自动扫描。

## License

Apache-2.0。见 `LICENSE` 与 `THIRD-PARTY-NOTICES.md`（Switchyard/laya 归属）。
