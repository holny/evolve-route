//! ⑧ 能力卡蒸馏（FlyRoute 灵感）：观测驱动的模型能力卡自刷新。
//!
//! Provider 悄悄更新模型（同名不同版本）会让静态 tiers 过期。飞轮积累的
//! 观测统计达标后（每 50 个成功），由决策模型重估 tiers；结果经保守收缩
//! （单次变化 ≤ 0.15）写入 ~/.evolve/distilled.json，叠加到 catalog。
//! 红线：用户显式声明（tiers_explicit）不被蒸馏覆盖；回滚 = 删除该文件。

use crate::types::{ModelRecord, Tiers};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 触发阈值：每积累 50 个成功请求蒸馏一次
pub const DISTILL_EVERY_SUCCESS: u64 = 50;
/// 两次蒸馏之间的最小间隔（毫秒）：防统计噪声驱动反复重写
pub const DISTILL_MIN_INTERVAL_MS: u64 = 7 * 24 * 3600 * 1000;
/// 蒸馏只能微调：单次 tier 变化上限
pub const MAX_TIER_SHIFT: f32 = 0.15;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistilledCard {
    pub tiers: Tiers,
    /// 首次蒸馏时的用户/发现声明值——review#5 修复：conservative_merge 的
    /// 基准固定为原始声明（否则每轮以上一轮蒸馏值为基准，多轮几何漂移
    /// 无上界）
    pub declared: Tiers,
    /// 客观摘要（本地拼接，非 LLM 生成）：蒸馏依据的观测形态
    pub note: String,
    /// epoch ms
    pub at: u64,
    /// 蒸馏时的累计成功数（下次触发的差值基准）
    pub basis_success: u64,
    pub version: u32,
}

pub type DistilledStore = HashMap<String, DistilledCard>;

fn store_path(dir: &str) -> PathBuf {
    let expanded = if let Ok(home) = std::env::var("HOME") {
        dir.replace("~", &home)
    } else {
        dir.to_string()
    };
    Path::new(&expanded).join("distilled.json")
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn load_store(dir: &str) -> DistilledStore {
    std::fs::read_to_string(store_path(dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save_store(dir: &str, store: &DistilledStore) {
    let path = store_path(dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(store) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// 是否达到蒸馏里程碑：成功数过阈值，且（新增成功 ≥ 阈值 或 距上次 ≥ 7 天）
pub fn due_for_distill(success: u64, card: Option<&DistilledCard>) -> bool {
    if success < DISTILL_EVERY_SUCCESS {
        return false;
    }
    match card {
        None => true,
        Some(c) => {
            success.saturating_sub(c.basis_success) >= DISTILL_EVERY_SUCCESS
                || now_ms().saturating_sub(c.at) >= DISTILL_MIN_INTERVAL_MS
        }
    }
}

/// 保守收缩：蒸馏建议与**原始声明**五五开，且相对原始声明的累计偏移
/// ≤ 2×MAX_TIER_SHIFT——review#5 修复：基准若用上一轮蒸馏值，多轮会
/// 几何漂移到建议值；固定声明基准 + 全局偏移上限才真正"只微调"
pub fn conservative_merge(declared: &Tiers, suggested: &Tiers) -> Tiers {
    let shrink = |dec: f32, sug: f32| -> f32 {
        let merged = 0.5 * dec + 0.5 * sug;
        let capped = merged.clamp(dec - 2.0 * MAX_TIER_SHIFT, dec + 2.0 * MAX_TIER_SHIFT);
        capped.clamp(0.0, 1.0)
    };
    Tiers {
        reasoning: shrink(declared.reasoning, suggested.reasoning),
        coding: shrink(declared.coding, suggested.coding),
        vision: shrink(declared.vision, suggested.vision),
        agentic: shrink(declared.agentic, suggested.agentic),
    }
}

/// 将蒸馏卡叠加到 catalog：用户显式声明（tiers_explicit）不覆盖
pub fn apply_distilled(models: &mut [ModelRecord], store: &DistilledStore) -> usize {
    let mut applied = 0;
    for m in models.iter_mut() {
        if m.tiers_explicit {
            continue;
        }
        if let Some(card) = store.get(&m.id) {
            m.tiers = card.tiers.clone();
            applied += 1;
        }
    }
    applied
}

/// 观测摘要的客观描述（本地拼接，无 LLM 文案）
pub fn stats_note(
    success: u64,
    success_rate: f32,
    avg_total_ms: f64,
    degenerate: u64,
    empty: u64,
) -> String {
    format!(
        "distilled from {} successes ({:.0}% ok, avg {}ms, degenerate {}, empty {})",
        success,
        success_rate * 100.0,
        avg_total_ms as u64,
        degenerate,
        empty
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiers(v: f32) -> Tiers {
        Tiers { reasoning: v, coding: v, vision: v, agentic: v }
    }

    #[test]
    fn conservative_merge_limits_cumulative_shift() {
        let declared = tiers(0.5);
        // 建议剧烈上修到 0.95：累计偏移相对原始声明封顶 2×MAX_TIER_SHIFT
        let merged = conservative_merge(&declared, &tiers(0.95));
        assert!((merged.reasoning - 0.5).abs() <= 2.0 * MAX_TIER_SHIFT + 1e-4);
        assert!(merged.reasoning > 0.5, "suggestion above declared should pull up");
        // 五五开理想值 0.725（在 0.5±0.30 封顶内，故不被截断）
        assert!((merged.reasoning - 0.725).abs() < 1e-4);
        // 即使 suggested 极端反向，累计偏移也被同一上界封顶
        let down = conservative_merge(&declared, &tiers(0.0));
        assert!((down.reasoning - 0.5).abs() <= 2.0 * MAX_TIER_SHIFT + 1e-4);
    }

    #[test]
    fn due_gate_respects_threshold_and_interval() {
        assert!(!due_for_distill(30, None), "below success threshold");
        assert!(due_for_distill(60, None), "first milestone reached");
        let fresh = DistilledCard {
            tiers: tiers(0.5),
            declared: tiers(0.5),
            note: String::new(),
            at: now_ms(),
            basis_success: 60,
            version: 1,
        };
        assert!(!due_for_distill(80, Some(&fresh)), "only 20 new successes");
        assert!(due_for_distill(120, Some(&fresh)), "50 new successes since last");
        let stale = DistilledCard { at: now_ms() - DISTILL_MIN_INTERVAL_MS - 1, ..fresh };
        assert!(due_for_distill(61, Some(&stale)), "interval elapsed");
    }

    #[test]
    fn apply_skips_user_explicit_tiers() {
        let mut store = DistilledStore::new();
        store.insert(
            "m".into(),
            DistilledCard { tiers: tiers(0.9), declared: tiers(0.3), note: String::new(), at: 0, basis_success: 50, version: 1 },
        );
        // 非 explicit：被蒸馏覆盖
        let mut free = ModelRecord { id: "m".into(), ..Default::default() };
        free.tiers = tiers(0.3);
        let mut models = vec![free];
        apply_distilled(&mut models, &store);
        assert!((models[0].tiers.reasoning - 0.9).abs() < 1e-4, "non-explicit tiers get distilled");
        // explicit：用户声明优先，不覆盖
        let mut locked = ModelRecord { id: "m".into(), ..Default::default() };
        locked.tiers = tiers(0.3);
        locked.tiers_explicit = true;
        let mut models = vec![locked];
        apply_distilled(&mut models, &store);
        assert!((models[0].tiers.reasoning - 0.3).abs() < 1e-4, "user-explicit tiers are never overridden");
    }
}
