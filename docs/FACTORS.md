# 路由因子详解

8 量化特征 + 5 评分因子 + 套餐数据 + 分层窗口融合。

## 1. 8 量化特征（`RequestFeatures`，不含原文）

| 特征 | 类型 | 含义 | 用途 |
|---|---|---|---|
| est_input_tokens | u64 | 估算输入 tokens | 窗口过滤、配额检查、budget_pressure |
| user_text_chars | usize | 用户文本字符数 | tier 域匹配权重 |
| code_density | f32 | 代码字符占比 0-1 | domain 推断辅助 |
| tool_count | usize | 请求带的工具定义数 | agentic 维度加权 |
| tool_ratio | f32 | 工具参数/总文本比 | tool_heavy 推断 |
| has_images | bool | 是否带图片 | 过滤无视觉模型 |
| turn_count | usize | 会话第几轮 | session_depth 推断 |
| cjk_ratio | f32 | 中日韩字符占比 | CJK 弱模型救援启发式 |

**关键**：这些是**量化特征**，不包含任何原文。redact=true 时连会话 digest 的 first/last_head 也只发占位 `[text]`。

## 2. 8 维 JudgmentSet（判定输出 → 路由消费）

| 维度 | 路由用途 |
|---|---|
| domain（8 类） | 选 tier 的哪个维度（Code→coding 为主，Other→vision 弱相关） |
| difficulty (L1-L4 → 0.5-3.0) | ①质量底线 ②成本权重联动 ③黏性资格 |
| needs_vision | >0.5 过滤无视觉字段的模型 |
| is_trivial | 寒暄类 → 便宜模型加权 |
| tool_heavy | agentic 维度加权 |
| high_stakes | >0.6 → 禁探索 + 质量加严 |
| session_relevance/depth | 进 difficulty_eff 调整 |

## 3. 5 评分因子（`scoring.rs::score_all` 核心）

| 因子 | 来源 | 时间口径 | 备注 |
|---|---|---|---|
| q 质量 | 全局 MODEL_TIERS 表（按模型名） | 静态 | 难度惩罚后能力分 |
| s 速度 | 目录先验 7 : 实测 3 混合 | 对数衰减 | 偏近期（分钟级波动） |
| c 成本 | 套餐积分系数折算 / 按量比价 | 实时（每次请求计） | 同方案内 min_hint 锚定 |
| r 可靠 | flywheel 实测 success/requests | **分层窗口** | 长期 50% + 近30次 30% + 近10次 20% |
| h 余量 | `(context_window - est - max_output) / context_window` | 实时 | 防"刚好装下"的脆弱 |

`score = W_q·q + W_s·s + W_c·c + W_r·r + W_h·h × √(用户权重·学习偏置) × 预算压制`

## 4. 套餐积分系数（`plans.rs`）

### zhipu/zai Coding Plan

| 模型 | 输入 | 缓存 | 输出 | hint 合成 |
|---|---|---|---|---|
| GLM-5.3-Flash | 2.3 | 0.56 | 8 | 26.3 |
| GLM-5.3 | 6.9 | 1.7 | 24 | 78.9 |

公式 `hint = 输入 + 3×输出`（输出对额度压力 3 倍权重）；同方案 min_hint=26.3 → 5.3 c=26.3/78.9=**0.33**。

### MiniMax Coding Plan

| 模型 | 输入 | 缓存 | 输出 | hint 合成 |
|---|---|---|---|---|
| MiniMax-M3 | 6.9 | 1.7 | 24 | 78.9 |
| MiniMax-M2.7 | 2.3 | 0.56 | 8 | 26.3 |

### volces Agent Plan（AFP）

| 模型 | 输入 | 输出 | hint 合成 |
|---|---|---|---|
| kimi-k3 | 10 | 10 | 40 |
| glm-5.3 | 4.5 | 4.5 | 18 |
| kimi-k2.8-preview | 8 | 8 | 32 |
| doubao-2.0-mini | 0.5 | 0.5 | 2 |

### 全局模型能力表（30 模型，与 provider 无关）

| 模型 | r | c | v | a |
|---|---|---|---|---|
| glm-5.3-flash | 0.70 | 0.85 | 0.85 | 0.85 |
| glm-5.3 | 0.85 | 0.95 | 0.85 | 0.95 |
| glm-5.2 | 0.80 | 0.88 | 0.85 | 0.90 |
| glm-5.1 | 0.75 | 0.82 | 0.80 | 0.85 |
| glm-latest | 0.85 | 0.93 | 0.85 | 0.93 |
| minimax-m3 | 0.85 | 0.90 | 0.85 | 0.90 |
| minimax-m2.7 | 0.72 | 0.78 | 0.85 | 0.78 |
| minimax-m2.7-highspeed | 0.70 | 0.75 | 0.85 | 0.75 |
| kimi-k3 | 0.88 | 0.92 | 0.85 | 0.92 |
| kimi-k2.7 | 0.78 | 0.85 | 0.80 | 0.85 |
| kimi-k2.8-preview | 0.80 | 0.85 | 0.80 | 0.85 |
| deepseek-v4-pro | 0.90 | 0.93 | 0.75 | 0.90 |
| deepseek-v4-flash | 0.70 | 0.80 | 0.75 | 0.80 |
| deepseek-v4.1-flash | 0.72 | 0.82 | 0.75 | 0.82 |
| deepseek-flash | 0.65 | 0.75 | 0.70 | 0.75 |
| doubao-seed-2.1-pro | 0.85 | 0.88 | 0.90 | 0.88 |
| doubao-seed-2.1-lite | 0.70 | 0.78 | 0.80 | 0.76 |
| doubao-seed-2.0-mini | 0.60 | 0.70 | 0.70 | 0.70 |
| doubao-seed-evolving | 0.85 | 0.90 | 0.90 | 0.92 |
| gpt-6-luna | 0.92 | 0.95 | 0.85 | 0.92 |
| gpt-5.6-luna | 0.88 | 0.90 | 0.80 | 0.88 |
| grok-4.7 | 0.93 | 0.94 | 0.85 | 0.93 |
| grok-4.6 | 0.90 | 0.92 | 0.85 | 0.90 |
| qwen3.8-max | 0.88 | 0.90 | 0.85 | 0.88 |
| qwen3.8-flash | 0.68 | 0.78 | 0.80 | 0.78 |
| qwen3.7-plus | 0.75 | 0.80 | 0.85 | 0.80 |
| longcat-2.0 | 0.65 | 0.72 | 0.60 | 0.75 |
| hy4 | 0.78 | 0.82 | 0.75 | 0.82 |
| hy3 | 0.65 | 0.72 | 0.60 | 0.72 |
| ark-code-latest | 0.82 | 0.90 | 0.75 | 0.90 |

**来源优先级**：
1. 用户 TOML `tiers = { coding = 0.9, ... }` 显式声明
2. 全局 MODEL_TIERS 表（最长名优先匹配，glm-5.3-flash 不会被 glm-5.3 吞）
3. benchmarks 学习覆盖（未接线，预留）
4. discovery 猜测值（opencode config reasoning 标志 + 硬编码 0.6）

## 5. 分层窗口融合（rel 求解）

```
r_long = 累计 success/requests
r_30   = recent 环尾 30 条 sample 中 ok/total（≤6h 期限内）
r_10   = recent 环尾 10 条 sample 中 ok/total（≤30min 期限内）

r = 5·r_long + 3·r_30 + 2·r_10  →  /10
       缺层归并（r_30 None → 6·r_long+4·r_10 / 10; ...）
```

**`rel_window` 三重约束**：
- 条数上限（30/10）
- 回溯期限（≤6h/≤30min）——"不能太老无法感知波动"
- 最小样本门槛（≥5/≥3）——"不能太新统计不稳"

用户裁决："不既要看着太久，也要稳定"。短窗+长窗混合，缺层自动回退到上一层。
</content>
