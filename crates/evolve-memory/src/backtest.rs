//! ⑦ 策略回放验证（MERA 灵感）：在历史事件流上 A/B 对比策略参数。
//!
//! 原理：events.jsonl 里每条路由事件带完整 scoreboard（公式分）与实际 outcome。
//! bandit 反馈的根本局限是"只有被选模型有该次结果"——因此模型真实质量用
//! **回放窗内的全局表现**（成功率 + 延迟的融合 proxy）作 ground truth 代理。
//! 策略收益 = Σ(策略重排后 top1 模型的全局质量 proxy)；两策略在同一事件集上
//! 对比，均势或更优才建议采纳（宁保守，不回滚到更差）。

use crate::flywheel::outcome_reward;
use crate::strategy::StrategyParams;
use serde::Serialize;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;

const MIN_MODEL_SAMPLES: usize = 5;
const BIAS_RECENT: usize = 30;
const SPEED_LN_MAX: f32 = 11.512_925; // ln(100_000)

#[derive(Debug, Serialize)]
pub struct StrategyScore {
    pub version: u32,
    pub ok_weight: f32,
    pub bias_gain: f32,
    /// 重放路由的平均质量 proxy（0-1）
    pub avg_quality: f32,
    /// A1 PGR（RouteLLM Performance Gap Recovered）：策略恢复了多少
    /// "oracle 最优与 oracle 最差"之间的质量差距，[0,1] 归一。
    /// 比裸均值更有说服力：0 = 与最差选择同劣，1 = 达到事后最优
    pub pgr: f32,
    /// 策略 top1 与实际 chosen 不同的比例（干预率）
    pub intervention_rate: f32,
    pub events_scored: u32,
}

#[derive(Debug, Serialize)]
pub struct ReplayReport {
    pub events_replayed: u32,
    pub models_with_outcomes: usize,
    /// A = 当前/默认参数，B = 候选参数
    pub a: StrategyScore,
    pub b: StrategyScore,
    pub verdict: String,
    /// A3b LLM-as-a-Judge 评分统计（未启用 judge 时全零）
    pub judge: JudgeOutcome,
}

struct EvRow {
    board: Vec<(String, f32)>,
    chosen: String,
    ok: bool,
    total_ms: u64,
    /// A3b LLM-as-a-Judge：resp_digest 摘要（opt-in 落盘才有）
    resp_digest: Option<String>,
}

fn parse_events(path: &Path, limit: usize) -> anyhow::Result<Vec<EvRow>> {
    use std::fs::File;
    let file = File::open(path)
        .map_err(|e| anyhow::anyhow!("无法打开事件流 {}: {e}", path.display()))?;
    let mut rows = Vec::new();
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("kind").and_then(|k| k.as_str()) != Some("chat") {
            continue;
        }
        let Some(board_v) = v.get("scoreboard").and_then(|s| s.as_array()) else {
            continue;
        };
        let board: Vec<(String, f32)> = board_v
            .iter()
            .filter_map(|s| {
                let id = s.get("model_id")?.as_str()?.to_string();
                let score = s.get("score")?.as_f64()? as f32;
                Some((id, score))
            })
            .collect();
        if board.is_empty() {
            continue;
        }
        let Some(chosen) = v.get("chosen").and_then(|c| c.as_str()).map(String::from) else {
            continue;
        };
        let status = v.get("status").and_then(|s| s.as_u64()).unwrap_or(0) as u16;
        let total_ms = v.get("total_ms").and_then(|s| s.as_u64()).unwrap_or(0);
        let bad_quality = v
            .get("quality")
            .and_then(|q| q.get("flags"))
            .and_then(|f| f.as_array())
            .map(|flags| {
                flags.iter().filter_map(|f| f.as_str()).any(|f| {
                    f == "degenerate" || f == "empty_stop" || f == "no_choices"
                })
            })
            .unwrap_or(false);
        rows.push(EvRow {
            board,
            chosen,
            ok: status == 200 && !bad_quality,
            total_ms,
            resp_digest: v
                .get("resp_digest")
                .and_then(|d| d.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
        });
    }
    // review#2 修复：events.jsonl 按时间序追加——取尾部才是"最近 N 条"，
    // 文件头是最旧的流量（大日志下回放陈旧数据会让 A/B 裁决失真）
    if rows.len() > limit {
        rows.drain(..rows.len() - limit);
    }
    Ok(rows)
}

/// 每模型 outcome 序列（回放窗内被选时的结果，按时间序）
fn model_outcomes(rows: &[EvRow]) -> HashMap<String, Vec<(bool, u64)>> {
    let mut map: HashMap<String, Vec<(bool, u64)>> = HashMap::new();
    for r in rows {
        map.entry(r.chosen.clone()).or_default().push((r.ok, r.total_ms));
    }
    map
}

/// 全局质量 proxy（ground truth 代理）：0.7·成功率 + 0.3·速度归一（与 speed_obs 同式）
fn quality_proxy(outcomes: &HashMap<String, Vec<(bool, u64)>>) -> HashMap<String, f32> {
    let mut map = HashMap::new();
    for (id, seq) in outcomes {
        if seq.len() < MIN_MODEL_SAMPLES {
            continue;
        }
        let rate = seq.iter().filter(|(ok, _)| *ok).count() as f32 / seq.len() as f32;
        let mut lat: Vec<f32> = seq.iter().map(|(_, ms)| (*ms).max(1) as f32).collect();
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let med_ms = lat[lat.len() / 2];
        let speed = (1.0 - med_ms.ln() / SPEED_LN_MAX).clamp(0.0, 1.0);
        map.insert(id.clone(), 0.7 * rate + 0.3 * speed);
    }
    map
}

/// 策略 bias：与运行时 telemetry_snapshot 同构——近 30 条 reward + 跨模型 median 中心化
fn strategy_bias(
    outcomes: &HashMap<String, Vec<(bool, u64)>>,
    params: &StrategyParams,
) -> HashMap<String, f32> {
    let mut rewards: Vec<(String, f32)> = Vec::new();
    for (id, seq) in outcomes {
        let recent: Vec<evolve_core::types::ReqSample> = seq
            .iter()
            .rev()
            .take(BIAS_RECENT)
            .map(|(ok, ms)| evolve_core::types::ReqSample {
                ts: 0,
                ttft_ms: 0,
                total_ms: *ms,
                in_tok: 0,
                cached_tok: 0,
                out_tok: 0,
                ok: *ok,
                tools_total: 0,
                tools_ok: 0,
            })
            .collect();
        if let Some(r) = outcome_reward(&recent, params) {
            rewards.push((id.clone(), r));
        }
    }
    let mut bias = HashMap::new();
    if rewards.len() < 3 {
        // 统计不足：中性 bias（回放退化为纯公式序，诚实反映数据不够）
        for (id, _) in rewards {
            bias.insert(id, 1.0);
        }
        return bias;
    }
    let mut rs: Vec<f32> = rewards.iter().map(|(_, r)| *r).collect();
    rs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = rs[rs.len() / 2];
    for (id, r) in rewards {
        bias.insert(id, (1.0 + (r - median) * params.bias_gain).clamp(0.7, 1.3));
    }
    bias
}

fn score_strategy(
    rows: &[EvRow],
    bias: &HashMap<String, f32>,
    proxy: &HashMap<String, f32>,
    params: &StrategyParams,
) -> StrategyScore {
    let mut total_q = 0.0f32;
    let mut total_opt = 0.0f32;
    let mut total_pess = 0.0f32;
    let mut scored = 0u32;
    let mut intervened = 0u32;
    for r in rows {
        // 计分事件集：board 中至少一个模型有全局质量 proxy
        let mut best: Option<(&String, f32)> = None;
        let mut opt: Option<f32> = None;
        let mut pess: Option<f32> = None;
        for (id, score) in &r.board {
            let b = bias.get(id).copied().unwrap_or(1.0);
            let noisy = score * b;
            if best.map_or(true, |(_, bs)| noisy > bs) {
                best = Some((id, noisy));
            }
            if let Some(q) = proxy.get(id) {
                opt = Some(match opt {
                    Some(v) => v.max(*q),
                    None => *q,
                });
                pess = Some(match pess {
                    Some(v) => v.min(*q),
                    None => *q,
                });
            }
        }
        let Some((top, _)) = best else { continue };
        if *top != r.chosen {
            intervened += 1;
        }
        let Some((o, p)) = opt.zip(pess) else { continue };
        // top1 无 proxy（样本不足）时取中性 0.5，与 oracle 同集计分
        let sq = proxy.get(top).copied().unwrap_or(0.5);
        total_q += sq;
        total_opt += o;
        total_pess += p;
        scored += 1;
    }
    let avg_quality = if scored > 0 { total_q / scored as f32 } else { 0.0 };
    let avg_opt = if scored > 0 { total_opt / scored as f32 } else { 0.0 };
    let avg_pess = if scored > 0 { total_pess / scored as f32 } else { 0.0 };
    let pgr = if avg_opt - avg_pess > 1e-4 {
        ((avg_quality - avg_pess) / (avg_opt - avg_pess)).clamp(0.0, 1.0)
    } else {
        // oracle 差距过小（候选同质）：PGR 无意义，取中性
        0.5
    };
    StrategyScore {
        version: params.version,
        ok_weight: params.ok_weight,
        bias_gain: params.bias_gain,
        avg_quality,
        pgr,
        intervention_rate: if scored > 0 {
            intervened as f32 / scored as f32
        } else {
            0.0
        },
        events_scored: scored,
    }
}

/// 在 events.jsonl 上 A/B 回放两套策略参数，产出对比报告与采纳建议。
/// judge 配置存在时（A3b），对带 resp_digest 的采样事件用 LLM-as-a-Judge
/// 做 point-wise 质量评分（"是否充分回答"），修正规则 ok 的误判——
/// 偏见消除：judge 与被评模型天然异构、point-wise 无位置偏差、
/// 评分维度单一明确（充分性）。
pub fn replay(
    events_path: &Path,
    a: &StrategyParams,
    b: &StrategyParams,
    limit: usize,
) -> anyhow::Result<ReplayReport> {
    replay_inner(events_path, a, b, limit, None, 0)
}

/// 带 judges 的回放（`backtest --judge-samples N` 时启用）。
/// asker: 闭包（query 摘要, 响应摘要）→ 充分性概率 [0,1]；None 表示本次未评分。
pub fn replay_with_judge(
    events_path: &Path,
    a: &StrategyParams,
    b: &StrategyParams,
    limit: usize,
    asker: &dyn Fn(&str, &str) -> Option<f32>,
    judge_samples: usize,
) -> anyhow::Result<ReplayReport> {
    replay_inner(events_path, a, b, limit, Some(asker), judge_samples)
}

fn replay_inner(
    events_path: &Path,
    a: &StrategyParams,
    b: &StrategyParams,
    limit: usize,
    asker: Option<&dyn Fn(&str, &str) -> Option<f32>>,
    judge_samples: usize,
) -> anyhow::Result<ReplayReport> {
    let mut rows = parse_events(events_path, limit)?;
    anyhow::ensure!(
        rows.len() >= 50,
        "事件流样本不足（{} 条，需 ≥50）：先积累生产流量再回放",
        rows.len()
    );
    let mut judge_report = JudgeOutcome::default();
    if let Some(ask) = asker {
        judge_report = apply_judge_scores(&mut rows, ask, judge_samples);
    }
    let outcomes = model_outcomes(&rows);
    let proxy = quality_proxy(&outcomes);
    let sa = score_strategy(&rows, &strategy_bias(&outcomes, a), &proxy, a);
    let sb = score_strategy(&rows, &strategy_bias(&outcomes, b), &proxy, b);
    // 均势保守：B 未显著优于 A（≥0.5% 相对差）则建议保持当前——
    // 策略变更的收益必须超过换策略本身的风险。裁决用 PGR（归一恢复率）
    let margin = 0.005f32 * sa.pgr.max(0.01);
    let verdict = if sb.pgr > sa.pgr + margin {
        format!("建议采纳 B（PGR +{:.2}pp：{:.3} → {:.3}）", (sb.pgr - sa.pgr) * 100.0, sa.pgr, sb.pgr)
    } else if sb.pgr >= sa.pgr - margin {
        "B 与 A 均势：保持当前参数（变更收益不显著）".into()
    } else {
        format!("保持 A：B 更差（PGR −{:.2}pp），拒绝采纳", (sa.pgr - sb.pgr) * 100.0)
    };
    Ok(ReplayReport {
        events_replayed: rows.len() as u32,
        models_with_outcomes: outcomes.len(),
        a: sa,
        b: sb,
        verdict,
        judge: judge_report,
    })
}

/// A3b judge 评分结果统计
#[derive(Debug, Default, Serialize)]
pub struct JudgeOutcome {
    /// 实际评分的事件数
    pub judged: u32,
    /// judge 判"不充分"（<0.5）但规则判 ok 的事件数——规则漏判被纠正
    pub rule_missed: u32,
    /// judge 判"充分"但规则判 fail 的事件数——规则误杀被纠正
    pub rule_overstrict: u32,
    /// judge 与规则 ok 判定的一致率（[0,1]；不足为可信度参考）
    pub agreement: f32,
}

/// 对带 resp_digest 的事件均匀采样并调 judge 评分，事件 ok 就地修正。
/// 评分维度单一：响应是否充分回答了查询（充分性，无风格项——防风格偏见）。
fn apply_judge_scores(
    rows: &mut [EvRow],
    ask: &dyn Fn(&str, &str) -> Option<f32>,
    samples: usize,
) -> JudgeOutcome {
    // 采样：带 resp_digest 的事件均匀取前 N 条（避免全量调用成本）
    let digest_idx: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.resp_digest.is_some())
        .map(|(i, _)| i)
        .take(samples)
        .collect();
    let mut out = JudgeOutcome::default();
    let mut agree = 0u32;
    for i in digest_idx {
        let r = &mut rows[i];
        let query_head = format!("event#{}", i); // 事件流无 query 正文（redact），以摘要上下文代之
        let Some(score) = ask(&query_head, r.resp_digest.as_deref().unwrap_or("")) else {
            continue;
        };
        let judge_ok = score >= 0.5;
        if judge_ok == r.ok {
            agree += 1;
        } else if !judge_ok && r.ok {
            out.rule_missed += 1;
        } else {
            out.rule_overstrict += 1;
        }
        r.ok = judge_ok; // 事件 ok 就地修正——下游 proxy/bias 全部继承
        out.judged += 1;
    }
    out.agreement = if out.judged > 0 { agree as f32 / out.judged as f32 } else { 1.0 };
    out
}

/// 报告的人类可读输出
pub fn print_report(r: &ReplayReport) {
    println!(
        "策略回放验证：{} 条事件，{} 个模型有 outcome 样本\n",
        r.events_replayed, r.models_with_outcomes
    );
    println!(
        "{:<6} {:<10} {:<10} {:<12} {:<10} {:<12} {:<8}",
        "策略", "ok_weight", "bias_gain", "平均质量", "PGR", "干预率", "计分事件"
    );
    for (name, s) in [("A(当前)", &r.a), ("B(候选)", &r.b)] {
        println!(
            "{:<6} {:<10.2} {:<10.2} {:<12.4} {:<10.3} {:<11.2}% {:<8}",
            name, s.ok_weight, s.bias_gain, s.avg_quality, s.pgr, s.intervention_rate * 100.0, s.events_scored
        );
    }
    println!("\n裁决：{}", r.verdict);
    if r.judge.judged > 0 {
        println!(
            "\nLLM-as-a-Judge：评分 {} 条，与规则判定一致率 {:.1}%（规则漏判 {}、误杀 {}）",
            r.judge.judged,
            r.judge.agreement * 100.0,
            r.judge.rule_missed,
            r.judge.rule_overstrict
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_strategy_prefers_bias_toward_quality_model() {
        // 模型 g 全成功且快（proxy 高）、模型 b 常失败（proxy 低）
        let mut outcomes = HashMap::new();
        outcomes.insert("g".to_string(), vec![(true, 500u64); 10]);
        outcomes.insert("b".to_string(), vec![(false, 9000u64); 10]);
        let proxy = quality_proxy(&outcomes);
        let rows = vec![
            EvRow { board: vec![("b".into(), 0.6), ("g".into(), 0.59)], chosen: "b".into(), ok: false, total_ms: 9000, resp_digest: None },
            EvRow { board: vec![("b".into(), 0.6), ("g".into(), 0.59)], chosen: "b".into(), ok: false, total_ms: 9000, resp_digest: None },
        ];
        // 策略给 g 强 bias：重排后 top1 = g，平均质量应显著高于纯公式序（top1=b）
        let mut params = StrategyParams::default();
        params.ok_weight = 1.0;
        params.bias_gain = 1.0;
        let mut bias = HashMap::new();
        bias.insert("g".to_string(), 1.3f32);
        bias.insert("b".to_string(), 0.7f32);
        let s = score_strategy(&rows, &bias, &proxy, &params);
        assert!(s.intervention_rate > 0.9, "bias 应翻转 top1: {s:?}");
        let neutral = score_strategy(&rows, &HashMap::new(), &proxy, &params);
        assert!(s.avg_quality > neutral.avg_quality, "偏向高质量模型的策略应得分更高");
    }

    #[test]
    fn quality_proxy_requires_min_samples() {
        let mut outcomes = HashMap::new();
        outcomes.insert("thin".to_string(), vec![(true, 100u64); 2]);
        assert!(quality_proxy(&outcomes).is_empty());
    }
}
