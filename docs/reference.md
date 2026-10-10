# References — 学术论文引用与借鉴记录

> 本文档记录 EvolveRouter 设计中借鉴的学术论文、关键洞察和实现状态。
> 目标：先完整记录所有论文阅读成果，再基于此文档统一实施改进，避免反复修改代码。

## 引用合规性说明

- 论文思想借鉴（Pareto 过滤、Thompson 采样、置信度级联）属于通用算法范式，不涉及版权
- README 中引用论文使用格式：`Inspired by [Author et al. (Year)](arxiv_link)`
- 差异化表述原则：明确说明我们与引用论文在赛道、协议、部署方式上的区别

---

## 阅读状态图例

- ✅ 已读全文（方法论 + 实验结果）
- 📖 已读摘要（只了解核心思想，未读实现细节）
- ⬜ 未读

---

## 一、同赛道核心参考（必读，已完成 ✅）

### 1. EvoRoute: Experience-Driven Self-Routing LLM Agent Systems

- **arXiv**: [2601.02695](https://arxiv.org/abs/2601.02695) · **状态**: ✅ 已读全文
- **作者**: Guibin Zhang et al. (NUS + Tongyi Lab)
- **核心思想**: Agent System Trilemma（性能-成本-延迟三难），三阶段路由：
  1. Self-Evolving Experience Base 𝒦 — 子任务级记录 ⟨agent, llm, subtask, embedding, tools, cost, duration, success, task_success⟩
  2. Multi-Faceted Retrieval — 三通道 UNION 检索（agent role / semantic similarity ≥ 0.85 / tool congruence）
  3. Pareto-Optimal Filtration + Thompson Sampling — 剔除被支配模型后 NIG 后验采样选 max
- **实验数据**: GAIA 63.19%（vs Claude-4 58.28%），成本降 76%（$85 vs $359），延迟降 14%
- **消融结果**: w/o 冷启动 -13.21%，w/o 多通道检索 -11.22%，w/o Thompson -6.52%，w/o Pareto -1.52%
- **EvolveRouter 借鉴**:
  - ✅ Pareto 过滤 → 已实现（scoring.rs `filter_pareto_dominated`）
  - ✅ Thompson 探索 → 已实现（engine.rs 不确定性比例噪声，简化版）
  - ⬜ 子任务级 embedding 检索 → 需本地 MiniLM (~80MB)，v0.2 考虑
  - ⬜ 冷启动 tree-based 探索 → 适合 benchmark 不适合生产，暂不采纳
- **差异化**: EvoRoute 面向 Agent 多步子任务路由（Python 研究框架），Turbine/evolve-router 面向本地网关级可审计路由（Rust 生产工程）。EvoRoute 不记决策日志、不能回滚——**可审计性和可回滚性是我们的差异化方向**

### 2. EvolveRouter: Co-Evolving Routing and Prompt for Multi-Agent QA

- **arXiv**: [2604.05149](https://arxiv.org/abs/2604.05149) · **状态**: 📖 已读摘要
- **核心思想**: KG-based router + 指令精炼闭环协同进化，自适应协作规模 K(q)
- **EvolveRouter 借鉴**: 面向多智能体问答，进化的是 prompt 不是策略版本——与我们赛道不同。不做。

---

## 二、学习机制论文（最该抄的算法，⬜ 待深入阅读）

### 3. BaRP: Learning to Route LLMs from Bandit Feedback

- **arXiv**: [2510.07429](https://arxiv.org/abs/2510.07429) · **状态**: 📖 已读摘要
- **核心思想**: Partial-feedback 设定——部署时只能观测被选中模型的输出，不能看到反事实（如果选了另一个模型会怎样）
- **实验数据**: 优于离线 router ≥12.46%，优于最大 LLM ≥2.45%
- **EvolveRouter 借鉴**:
  - 我们的飞轮已经天然是 partial feedback（只观察被选模型）✓
  - **改进方向**: 在文档中明确这个约束——飞轮是 bandit learning 不是 supervised learning，不要假设反事实
  - **注意**: 离线全标注 router 在线上会失效，因为线上你无法同时运行所有模型来比较

### 4. BayesianRouter: Offline Prior + Online Thompson Sampling

- **arXiv**: [2510.02850](https://arxiv.org/abs/2510.02850) · **状态**: 📖 已读摘要
- **核心思想**: 两阶段——离线学 per-RM reliability 先验，在线 Thompson 采样更新后验
- **EvolveRouter 借鉴**:
  - **离线先验**: 我们的 plans.rs 注册表就是离线先验（tier/allowance/credit multipliers）✓
  - **在线后验**: 我们的飞轮 learned_bias + reliability 就是后验更新 ✓
  - **改进方向**: 将 Thompson 探索从简化版（score + noise ∝ 1/√samples）升级为完整 NIG 后验
  - **priors.json 概念**: `~/.evolve/priors.json` 存储离线先验，启动时加载——类似我们的 snapshot.json 但更结构化

### 5. RouteLLM (ICLR)

- **状态**: 📖 已读摘要
- **核心思想**: 用人类偏好数据训 router，强/弱模型二选一
- **实验数据**: 成本降 2× 且不降质
- **EvolveRouter 借鉴**: 最简单的起步形态——二元路由（强/弱）。我们的 quality floor + difficulty 联动已经实现了这个模式，但更精细

### 6. LLM Routing with Dueling Feedback (FGTS.CDB)

- **状态**: ⬜ 未读
- **核心思想**: pairwise 偏好反馈比绝对打分更省标签；category-calibrated fine-tuning 解决模型质量量纲不一
- **EvolveRouter 借鉴**: 我们的模型池里不同 Provider 的模型质量量纲不同（智谱积分 vs Go 美元 vs API 美元），category-calibrated 思路可用于归一化

---

## 三、架构设计论文（五组件框架，⬜ 待深入阅读）

### 7. LLMRouter: Unified Infrastructure for LLM Routers

- **arXiv**: [2608.06867](https://arxiv.org/abs/2608.06867) · **状态**: ✅ 已读全文
- **核心思想**: 路由统一形式化为五组件顺序决策过程：
  1. **Context encoder** E_q — 编码路由状态（query, user, history）
  2. **Model encoder** E_m — 编码每个 LLM 候选
  3. **Scoring function** g — 度量 state-candidate 兼容性
  4. **Decision rule** d — 将评分转为路由动作（argmax / threshold / cascade / sample）
  5. **Learning signal** L — 拟合组件向最优策略（pointwise / pairwise / RL）
- **三家族**: Single-turn / Multi-turn (agentic) / Personalized
- **关键实验发现**:
  - **无单一 router 全域统治**——最佳 router 因任务和成本预算而异
  - **学到的 router 优于最强固定模型 14.6%**（因为固定最大模型成本最高但性能平庸）
  - **多轮路由不稳定优于单轮**（多轮分解聚合增加成本和冗余信息）
  - **Cost 约束下轻量 router 排名反超**（验证了我们的难度联动权重设计）
  - **Personalization 有收益但取决于 user context 建模质量**
- **EvolveRouter 五组件映射**:
  | LLMRouter 组件 | Turbine 对应 | 状态 |
  |---|---|---|
  | Context encoder | 8 信号判定（Jev + 启发式） | ✅ |
  | Model encoder | 目录（tier/window/cost/plan） | ✅ |
  | Scoring function | 5 因子复合评分（难度联动权重） | ✅ |
  | Decision rule | 降级链 + Pareto + Thompson + 粘性 | ✅ |
  | Learning signal | 飞轮 4 层反馈 | ✅ |
- **关键结论**: Turbine 架构完整覆盖五组件模型，每个组件的实现都比论文中的 16 种 router 更精细（决策模型参与、订阅经济学、跨协议翻译）

### 8. LLMRouterBench

- **状态**: 📖 已读结论
- **关键发现**:
  - 所有 router 表现差距 <2% → **瓶颈在 decision mechanism，不在 query representation**
  - 换 embedding 模型差异 <2%（gte-qwen2 vs MiniLM）→ 本地小模型够用
  - **Gap@Oracle 20-33%**：只有 1-3 个模型能答对的 query（11.9%）上 router 准确率仅 24% → **越是难路由的场景越容易失效** → 我们的多维判定 + 域分类为此设计
  - **Top-4 模型 ≈ 20 模型池** → 推荐用户配 4-6 个 Provider
- **README 素材**: 直接引用这些数据说明 EvolveRoute 解决的是"决策机制"而非"表征"问题

---

## 四、工程实现参考（Rust 技术栈，⬜ 待深入阅读）

### 9. vLLM Semantic Router

- **arXiv**: [2510.08731](https://arxiv.org/abs/2510.08731) · **状态**: 📖 已读摘要
- **NeurIPS 2025 MLForSys Workshop**
- **核心思想**: ModernBERT 意图分类器 + Rust/Candle 分类核心 + Go+Envoy 集成
- **实验数据**: MMLU-Pro +10.2pp，延迟 -47.1%，token -48.5%
- **EvolveRouter 借鉴**:
  - **本地 ModernBERT 分类器替代 Jev API 调用**——消除 API 延迟
  - **简单请求跳过决策模型**：启发式置信度高时直接路由，不调 Jev
  - 技术栈（Rust/Candle）与我们一致，源码值得逐文件看
  - **v0.2 考虑**: 集成 Candle 推理做本地意图分类

### 10. FrugalGPT

- **状态**: 📖 已读摘要
- **核心思想**: 级联路由——先便宜后贵，置信度不够再升级
- **EvolveRouter 借鉴**: 我们的降级链已实现错误触发的 fallback；**改进方向：加质量触发的升级**——便宜模型返回了但质量信号差（截断/退化/空响应），升级到更好的模型重试

### 11. Router-R1

- **arXiv**: [2506.09033](https://arxiv.org/abs/2506.09033) · **状态**: 📖 已读摘要
- **核心思想**: Router 本身是 policy LLM，Think/Route/Aggregate 多轮切换
- **EvolveRouter 借鉴**: "多轮路由"未来方向——当前不做

---

## 五、其他值得关注（⬜ 待阅读）

| 论文 | 核心思想 | 与 EvolveRouter 关系 |
|---|---|---|
| ACRouter (2606.22902) | C-A-F 循环（上下文→动作→反馈），反馈闭环 + 记忆库 | 反馈闭环设计参考 |
| FlyRoute (2605.22057) | 数据飞轮 + 模型能力画像 + 冷启动 | 模型能力画像可借鉴 |
| MERA (2608.10333) | 离线回放验证策略更新 + verifier 校验 + 降级兜底 | 策略更新安全机制 |
| RouteNLP (2602.08620) | 查询难度感知 + conformal prediction 置信度校准 | 置信度校准方法 |
| SEMIROUTER (2607.10041) | 稀疏数据路由 + 新模型热接入 | 新模型接入场景 |
| RAR (2411.09837) | 持续自适应路由，50.2% 请求到便宜模型，保持 90.5% 质量 | 成本优化数据点 |
| CARvE (ICML) | 持续分类 + 对比嵌入评分 + 结构化负样本回放 | 防灾难性遗忘 |
| HierRouter (2511.09873) | PPO RL 多跳路由，质量提升 2.4× | RL 路由参考 |

---

## 六、命名注意事项

**命名空间饱和**：EvoRoute、EvolveRouter、xRouter、RouteLLM、Router-R1、RouterBench、PersonalizedRouter 等名称在学术界已饱和。

**当前命名**：
- GitHub repo: `holny/evolve-router`
- binary: `evolve`
- 数据: `~/.evolve/`
- 面板标题: EvolveRouter

**建议**: binary 可用 `evolve`（短且不在学术命名泥潭中）。GitHub repo 名 `evolve-router` 保品类识别。

---

## 七、实施优先级

**已完成 ✅**:
1. Pareto 过滤（scoring.rs `filter_pareto_dominated`）
2. Thompson 探索（engine.rs 不确定性比例噪声，简化版）
3. 置信度门控探索（domain_confidence < gate 时不探索）
4. 失败路径捕获 provider 配额头
5. weekly/monthly 窗口识别
6. 429 分类带窗口标签
7. route_advisor 管道接通
8. 400 响应体日志

**待实现（按优先级）**:
1. ⬜ 质量级联（FrugalGPT）：便宜模型先答，质量信号差→升级重路由（quality.rs 信号已有）
2. ⬜ 策略回放验证（MERA）：flywheel 修改 bias 前用历史流量回放验证
3. ⬜ per-domain 置信度追踪（Jev cookbook）：飞轮记住各域 Jev 平均置信度
4. ⬜ 本地 ModernBERT 分类器（vLLM Semantic Router）：Candle 推理替代 Jev API
5. ⬜ 语义相似检索（EvoRoute）：本地 MiniLM embedding 匹配历史相似请求
6. ⬜ 评测框架（LLMRouterBench）：`evolve eval` 命令回放历史对比随机路由

**暂不实现**:
- 多轮分解聚合路由（LLMRouter 发现不稳定优于单轮）
- RL-based router 训练（需大规模轨迹数据）
- 完整 router 网络训练（所有 router 差距 <2%）

---

## 八、README 引用建议

在 README.md 的 `## Why not another proxy` 节后加 `## Related Work` 节：

> EvolveRouter draws on ideas from the growing literature on LLM routing. Unlike [EvoRoute](https://arxiv.org/abs/2601.02695) (agent sub-step routing, Python research framework) and [EvolveRouter](https://arxiv.org/abs/2604.05149) (prompt co-evolution for multi-agent QA), Turbine/evolve-router focuses on **local gateway-level auditable routing** in Rust. Every routing decision is logged, auditable, and reversible — a gap in all existing routing systems.
>
> Our difficulty-linked weight modulation is inspired by [FrugalGPT](https://arxiv.org/abs/2305.05176)'s confidence cascade. Our exploration strategy draws from [BayesianRouter](https://arxiv.org/abs/2510.02850)'s Thompson sampling approach. Our five-factor composite scoring follows the [composite scoring pattern](https://arxiv.org/abs/2608.06867) from the LLMRouter unified formulation.

---

## 更新记录

| 日期 | 更新 |
|---|---|
| 2026-10-09 | 初始创建，记录 EvoRoute 全文阅读笔记 |
| 2026-10-09 | 补充 LLMRouter 全文阅读 + 全部摘要级论文 |
| 2026-10-10 | 补充阅读状态图例、实施优先级、README 引用建议 |
