# 决策流程：请求全链路 + 公式 + 因子

## 1. 请求全链路（一次请求的生命周期）

```
Agent ──► 网关入口 ──► 身份提取 ──► 预处理 ──► 决策 ──► 转发 ──► 上游 ──► 流式 ──► finalize
  0ms        1ms         <1ms          ~15ms    0~3s   网络RTT    生成      SSE      落盘
```

### ① 身份提取（identity.rs）

| 头 | 优先级 | 用途 |
|---|---|---|
| `x-ev-client` | 1 | 用户插件或自定义 |
| `originator`（codex）| 2 | Codex CLI |
| `user-agent` 映射 | 3 | claude-cli→claude-code, codex-cli→codex, opencode-cli→opencode |
| `x-ev-session` | 1 | 用户插件 |
| `session_id`（codex）| 2 | Codex |
| `x-opencode-session` | 1 | **opencode 原生**（官方协议要求） |
| `metadata.user_id` `_session_` 段 | 3 | claude-code |
| `x-session-id` / `x-session` | 4 | 通用 |
| 消息首段 SHA256 截断 | 兜底 | 任何 |

### ② 预处理（~15ms）

- 解析 body（OpenAI / Anthropic 双协议）→ `RequestFeatures` 8 维量化（不含原文）
- token 估算：消息×字符比 + 工具参数 + 系统提示
- 会话 digest：首末消息头 200/400 字符 + 重叠率 + 指示代词 + 话题转移标记

### ③ 路由决策（`Engine::decide`）

**两阶段决策**：

```
阶段 1：判定                                       阶段 2：评分
8 量化特征 → 任务画像                              5 因子 → 排序
       │                                                  │
       ▼ HybridBackend 双判官                          ▼ score_all
  Jev(LLM,1-3s) + Heuristic(0ms)               q  s  c  r  h
       │ 融合（红/严/均值/分歧取 heuristic）              │
       ▼ JudgmentSet(8 维)                          ▼
  domain difficulty visual 决策                cost × √(权重×偏置) × 预算压制
  tool_heavy  high_stakes                       │
  session_relevance/depth                        ▼
       │                                  排序 + ε探索 + 置信门控
       ├──► 质量底线（弱模型出局）                          │
       ├──► 成本权重联动（难度→权重）                       │
       └──► 评分 tier 域匹配                              ▼
                                                  chosen + chain + scoreboard
```

## 2. 完整决策管线

```
目录 195 个模型
  │
  ▼ 硬检查（8 类，任一失败则淘汰）
  ├── 健康冷却（model×key，model_available() 走 model_available
  │   → plan_available API）
  ├── 凭据（has_credential：api_key Some 或 base_url 127.0.0.1）
  ├── 视觉需求（j.needs_vision > 0.5 → 过滤无 vision 字段的模型）
  ├── 窗口（context_fits：est×1.1+max_output ≤ context_window；
  │   或 proven-bound：max_accepted_tokens ≥ est）
  ├── 配额（quota_best_remaining_by_models ≥ est）
  └── 预算压力（>0.95 → 硬过滤，剩 1.5%-5% 余量给硬任务）
  │
候选（约 18）
  │
  ▼ 质量底线 quality_floor(difficulty_eff)
  d=0 → floor=1.0（谁都行）│d=3 → floor=0.55（仅强模型）
  │
合格（约 9-15）
  │
  ▼ 综合评分 score_all
  │
  ▼ ε 探索（10% 默认）
  欠采样模型优先（samples<10），打破"老牌模型富者愈富"
  │
  ▼ 置信门控
  jev+heuristic 都低置信 → 沿用上一会话模型
  │
selected = chosen + chain
```

## 3. 公式全集

### 3.1 质量分（`scoring.rs::quality`）

```rust
requirement = difficulty_eff / 3.0
steep = 1.0 + 2.0 * requirement     // 难度放大器
tier_st = tier^steep
quality = 1.0 - requirement * (1.0 - tier_st)
```

**tier 来自**（按模型名匹配全局 MODEL_TIERS 表，与 provider 无关）：
- zhipu-glm-5.3-flash → 0.70/0.85/0.85/0.85
- glm-5.3 → 0.85/0.95/0.85/0.95
- ...

| difficulty | 0.5 | 1.0 | 1.5 | 2.0 | 2.5 | 3.0 |
|---|---|---|---|---|---|---|
| requirement | 0.17 | 0.33 | 0.50 | 0.67 | 0.83 | 1.0 |
| steep | 1.33 | 1.67 | 2.0 | 2.33 | 2.67 | 3.0 |
| tier 0.5 (弱) q | 0.88 | 0.79 | 0.50 | 0.31 | 0.13 | 0.0 |
| tier 0.75 (中) q | 0.97 | 0.94 | 0.88 | 0.81 | 0.69 | 0.50 |
| tier 0.95 (强) q | 0.99 | 0.98 | 0.97 | 0.96 | 0.94 | 0.88 |

**指数惩罚**：难度高时弱模型 q 迅速坍塌，硬任务只能强模型上。

### 3.2 五因子评分（`scoring.rs::score_all`）

```rust
score = W_q·q + W_s·s + W_c·c + W_r·r + W_h·h × √(用户权重·学习偏置) × 预算压制 × 1
```

| 因子 | 来源 | 含义 | 时间口径 |
|---|---|---|---|
| q 质量 | `quality()` 全局模型能力表 | 难度匹配后能力分 | 静态（官方基准） |
| s 速度 | 目录先验 7: 实测 3 混合 | 目录速度×实测 | 对数衰减（偏近期） |
| c 成本 | 套餐积分系数 | 配额消耗速率 | 实时（每次请求计） |
| r 可靠 | flywheel `success/requests` | 实例实测成功率 | **分层窗口**：长期 50% + 近30次 30% + 近10次 20% |
| h 余量 | `context_window - est - max_output` | 窗口剩余比 | 实时 |

### 3.3 难度联动权重（`scoring.rs::difficulty_weights`）

```rust
let d = difficulty_eff / 3.0;          // 0~1
let cost_boost   = 2.0 - 1.6 * d;        // 难度0 → 2.0；难度3 → 0.4
let quality_scale = 0.75 + 0.25 * d;     // 难度0 → 0.75；难度3 → 1.0

let v = [
    w_q * quality_scale,  // 质量权重反向
    w_s,                   // 速度不变
    w_c * cost_boost,      // 成本：简单任务×2省钱，复杂任务×0.4质量主导
    w_r,                   // 可靠不变
    w_h,                   // 余量不变
]
// 再归一化到 sum=1.0
```

**用户裁决的策略对应**：
- 难度 0（寒暄/查文档）：cost 权重×2 → flash 这类轻量便宜模型胜
- 难度 3（架构重构/支付流程）：cost 权重×0.4 → 质量主导，glm-5.3 / MiniMax-M3 胜

### 3.4 可靠性分层窗口融合（`scoring.rs::r` 求解）

```rust
r_long = min(success, requests) / requests        // 累计
r_30   = 近30条样本 success/total
r_10   = 近10条样本 success/total

r = (5·r_long + 3·r_30 + 2·r_10) / 10
       缺层归并（r_30 None → 6·r_long+4·r_30 / 10; …）
```

**窗口**：`rel_window(recent, take, min_samples, window_ms, now_ms)` 三重约束
- 条数上限（30/10）
- 回溯期限（≤6h/≤30min）——防"太老无法感知波动"
- 最小样本门槛（≥5/≥3）——防"太新统计不稳"

### 3.5 成本分（`scoring.rs::c` 求解）

```rust
if m.plan:
    if Some(hint) = plan_credit_intensity(base_url, model_id):
        min_hint = 同方案内所有 hint 的最小值
        c = min_hint / hint
    else:
        c = 1.0                       // 套餐无精确系数 → 视为最优
else:
    if m.cost Some:
        c = min_price / price        // 按量计费比值
    else:
        c = 0.5                       // 未知价格 → 中性
```

`plan_credit_intensity` 用官方系数折算的合成价格：
- zhipu/zai glm-5.3-flash → 2.3 + 3×8 = 26.3
- glm-5.3 → 6.9 + 3×24 = 78.9
- volces afp kimi-k3 → 10 + 3×10 = 40

同方案内 min_hint = flash=26.3 → 5.3 c=26.3/78.9=0.33

### 3.6 预算压制（引擎层）

```rust
if p > soft_pct/100:               // soft_pct 默认 60
    damp = (1.0 - 0.9·(p - soft)/(0.95 - soft))·(1.0 - 0.6·difficulty/3)
    score *= max(0.05, damp)
```

难度越高压制越轻，硬任务仍可用订阅内模型。

### 3.7 任务判定（ev-decision 双判官）

`HybridBackend = Jev(LLM, 1-3s) + Heuristic(0ms)`，按 8 维 prompt：

| 维度 | 选项 | 用处 |
|---|---|---|
| task_domain | code/math/writing/lookup/data/chitchat/agent_ops/other | tier_for_domain 选维度 + 5 因子 q 维度 |
| difficulty | L1=0.5, L2=1.4, L3=2.2, L4=3.0 | 质量底线 + 成本权重联动 + 黏性资格 |
| needs_vision | 0/1 | 过滤无视觉模型 |
| is_trivial | 0/1 | 寒暄加权便宜 |
| tool_heavy | 0/1 | agentic 维度加权 |
| high_stakes | 0/1 | >0.6 → 禁探索 + 质量加严 |
| session_relevance | 0/1 | 进 difficulty_eff 调整 |
| session_depth | 0/3 | 同上 |

**输入**：
- `task: { length_bucket, code_density, tool_count, has_images, turn_count, est_tokens }`
- `session: { first_task_head, current_head, continues_topic, topic_shift, tools_seen }`（redact=true 时 head 替换为 `[text]` 占位）

**融合**：域冲突 → 取 heuristic；难度 → 取均；高危 → 取严；restful defaults to heuristic。

### 3.8 黏性重试条件（8 项全部满足才复用）

```rust
size_ok = est ≤ 2·prev_est AND prev_est ≤ 2·est
low_diff = prev_difficulty ≤ 1.6                // L2 上限
pressure_ok = plan_pressure ≤ soft_pct/100 + 0.25
// 加上：窗口装得下 / 健康 / 配额够 / 工具未变 / turns_left > 0
```

外加 **10% 探索逃逸**：黏性命中时以 explore_ratio 概率走完整评估（打破会话躺平）。
</content>
