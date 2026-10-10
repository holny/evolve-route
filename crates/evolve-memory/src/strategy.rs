//! ⑦ 策略版本化（MERA 式回放验证的参数载体）。
//!
//! 飞轮的学习产物（learned_bias）由一组显式参数驱动；参数可经
//! evolve.toml 的 [strategy] 段覆盖。回滚 = 删除配置段（回到编译默认）。
//! 是否采纳新参数由 `evo-router backtest` 在历史事件流上 A/B 回放后裁决。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyParams {
    /// 策略版本号：公式语义变更时递增
    pub version: u32,
    /// outcome reward 中成功率信号权重（延迟信号取 1 − 此值）
    pub ok_weight: f32,
    /// bias = 1 + (reward − cross-model median) × bias_gain
    pub bias_gain: f32,
}

impl Default for StrategyParams {
    fn default() -> Self {
        // v2：Phase 1 两阶段归一化版（成功率 0.7 + 延迟 0.3，gain 0.6）
        Self { version: 2, ok_weight: 0.7, bias_gain: 0.6 }
    }
}

impl StrategyParams {
    pub fn lat_weight(&self) -> f32 {
        1.0 - self.ok_weight
    }

    /// 校验并钳制到安全区间：飞轮只能微调，参数越界一律拉回
    pub fn sanitized(mut self) -> Self {
        self.ok_weight = self.ok_weight.clamp(0.3, 0.95);
        self.bias_gain = self.bias_gain.clamp(0.1, 1.0);
        self
    }
}
