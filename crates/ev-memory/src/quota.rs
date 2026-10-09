//! Quota window ledger: learns per-model token/request windows from
//! upstream rate-limit headers on successful responses, plus explicit
//! quota-error cooldowns. Feeds the engine's pre-check (never route a
//! request larger than remaining budget) and the dashboard quota card.
//!
//! Reference-only sources (decision record #22): header facts only, never
//! user config.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WindowState {
    pub scope: String,
    pub remaining: Option<u64>,
    pub limit: Option<u64>,
    pub reset_epoch_ms: Option<u64>,
    pub updated_epoch_ms: u64,
}

/// (ts, 入, 缓存读, 出) 单条用量记录
pub type UsageRecord = (u64, f64, f64, f64);
type UsageMap = HashMap<String, Vec<UsageRecord>>;

pub struct QuotaLedger {
    inner: Mutex<HashMap<String, Vec<WindowState>>>,
    /// 订阅方案用量账本（模型 → token 记录，滚动保留 24h）
    plan_usage: Mutex<UsageMap>,
    /// provider 测的实时配额（plan_key + window_scope → 最近一次响应头里的剩余/限额）。
    /// 面板配额展示以此为权威源——本地按 token 累加的估算容易与 provider 真实值漂移
    /// （错误响应消耗配额但不返回 usage / 计费精度差异 / 并发令牌预留等）
    observed_provider: Mutex<HashMap<String, ObservedProviderQuota>>,
}

/// provider 响应头里的实时配额快照（每个 plan_key 一份；面板展示窗口维度）
#[derive(Debug, Clone, Default)]
pub struct ObservedProviderQuota {
    /// ("5h" / "7d" / "tokens" / ...) → 剩余 + 限额（令牌/请求，原始单位由 provider 决定）
    pub windows: HashMap<String, ObservedWindow>,
}

#[derive(Debug, Clone, Default)]
pub struct ObservedWindow {
    pub remaining: Option<u64>,
    pub limit: Option<u64>,
    pub reset_epoch_ms: Option<u64>,
    pub updated_epoch_ms: u64,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl QuotaLedger {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            plan_usage: Mutex::new(HashMap::new()),
            observed_provider: Mutex::new(HashMap::new()),
        }
    }

    /// 记录 provider 实时配额（每次响应 parse_headers 后调用）。plan_key 由
    /// 调用方根据当前路由的 plan_key_for(base_url) 解析——本结构不做归属。
    pub fn observe_provider(&self, plan_key: &str, windows: &[WindowState]) {
        if windows.is_empty() || plan_key.is_empty() { return; }
        let Ok(mut m) = self.observed_provider.lock() else { return };
        let slot = m.entry(plan_key.to_string()).or_default();
        for w in windows {
            if w.remaining.is_none() && w.limit.is_none() { continue; }
            slot.windows.insert(w.scope.clone(), ObservedWindow {
                remaining: w.remaining,
                limit: w.limit,
                reset_epoch_ms: w.reset_epoch_ms,
                updated_epoch_ms: w.updated_epoch_ms,
            });
        }
    }

    /// 取某 plan_key 某窗口的最新观察值与年龄。max_age_ms 默认 10 分钟（Provider
    /// 测的时效性窗口——超过则退化到本地估算）
    pub fn observed_window(&self, plan_key: &str, scope: &str, now_ms: u64, max_age_ms: u64) -> Option<ObservedWindow> {
        let m = self.observed_provider.lock().ok()?;
        let slot = m.get(plan_key)?;
        let w = slot.windows.get(scope)?.clone();
        if now_ms.saturating_sub(w.updated_epoch_ms) > max_age_ms { return None; }
        Some(w)
    }

    /// 记录一次订阅方案请求的 token 用量（24h 滚动保留）
    pub fn note_plan_usage(&self, model: &str, in_tok: f64, cached_tok: f64, out_tok: f64, now_ms: u64) {
        const RETAIN_MS: u64 = 24 * 3600 * 1000;
        let Ok(mut m) = self.plan_usage.lock() else { return };
        let e = m.entry(model.to_string()).or_default();
        e.push((now_ms, in_tok, cached_tok, out_tok));
        e.retain(|(ts, _, _, _)| now_ms.saturating_sub(*ts) <= RETAIN_MS);
    }

    /// 窗口内各模型 token 用量合计（默认 5h 窗口）
    pub fn plan_usage_sums(&self, now_ms: u64, window_ms: u64) -> HashMap<String, (f64, f64, f64)> {
        let mut out: HashMap<String, (f64, f64, f64)> = HashMap::new();
        let Ok(m) = self.plan_usage.lock() else { return out };
        for (model, recs) in m.iter() {
            let mut acc = (0.0f64, 0.0f64, 0.0f64);
            for (ts, i, c, o) in recs {
                if now_ms.saturating_sub(*ts) <= window_ms {
                    acc.0 += i;
                    acc.1 += c;
                    acc.2 += o;
                }
            }
            out.insert(model.clone(), acc);
        }
        out
    }

    /// Extract quota windows from response headers (any protocol family).
    pub fn parse_headers(headers: &http::HeaderMap) -> Vec<WindowState> {
        let mut out: Vec<WindowState> = Vec::new();
        let now = now_ms();
        let get = |name: &str| -> Option<u64> {
            headers.get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok())
        };
        // anthropic unified windows
        for (scope, tag) in [("5h", "anthropic-ratelimit-unified-5h-token"), ("7d", "anthropic-ratelimit-unified-7d-token")] {
            let remaining = get(&format!("{tag}-remaining"));
            let limit = get(&format!("{tag}-limit"));
            let reset = headers
                .get(format!("{tag}-reset"))
                .and_then(|v| v.to_str().ok())
                .and_then(parse_rfc3339_ms);
            if remaining.is_some() || limit.is_some() {
                out.push(WindowState {
                    scope: scope.into(),
                    remaining,
                    limit,
                    reset_epoch_ms: reset,
                    updated_epoch_ms: now,
                });
            }
        }
        // openai token window
        if let Some(remaining) = get("x-ratelimit-remaining-tokens") {
            // reset-tokens 是相对时长（"6m0s"）——换算为绝对纪元，消费端按 epoch 比较
            let reset = headers
                .get("x-ratelimit-reset-tokens")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_duration_ms)
                .map(|ms| now.saturating_add(ms));
            out.push(WindowState {
                scope: "tokens".into(),
                remaining: Some(remaining),
                limit: get("x-ratelimit-limit-tokens"),
                reset_epoch_ms: reset,
                updated_epoch_ms: now,
            });
        }
        // openai request window (tracked separately)
        if let Some(remaining) = get("x-ratelimit-remaining-requests") {
            out.push(WindowState {
                scope: "requests".into(),
                remaining: Some(remaining),
                limit: get("x-ratelimit-limit-requests"),
                reset_epoch_ms: None,
                updated_epoch_ms: now,
            });
        }
        out
    }

    pub fn observe(&self, health_id: &str, windows: Vec<WindowState>) {
        if windows.is_empty() {
            return;
        }
        let Ok(mut m) = self.inner.lock() else { return };
        let slot = m.entry(health_id.to_string()).or_default();
        for w in windows {
            if let Some(existing) = slot.iter_mut().find(|e| e.scope == w.scope) {
                *existing = w;
            } else {
                slot.push(w);
            }
        }
    }

    /// Model-level view: best remaining across all key slots (any key may
    /// still carry budget). `None` when nothing is known.
    pub fn model_remaining_tokens(&self, model_id: &str) -> Option<u64> {
        let m = self.inner.lock().ok()?;
        let now = now_ms();
        let mut best: Option<u64> = None;
        for (k, slots) in m.iter() {
            if health_model_id(k) != model_id {
                continue;
            }
            for w in slots {
                if w.scope == "requests" {
                    continue;
                }
                // expired window = budget refreshed; treat as unknown/ok
                if let Some(r) = w.reset_epoch_ms
                    && r <= now {
                        continue;
                    }
                if let Some(rem) = w.remaining {
                    best = Some(match best {
                        Some(b) => b.max(rem),
                        None => rem,
                    });
                }
            }
        }
        best
    }

    /// Bulk model view for the engine pre-check (one pass over the ledger).
    pub fn best_remaining_by_models(&self) -> std::collections::HashMap<String, u64> {
        let mut out = std::collections::HashMap::new();
        let m = self.inner.lock();
        let Ok(m) = m else { return out };
        let now = now_ms();
        // stale-window guards: a window older than 24h is garbage; a
        // zero-remaining window with unknown reset only blocks for 30min
        // (otherwise a header-parse miss locks the model until restart)
        const MAX_WINDOW_AGE_MS: u64 = 24 * 3600 * 1000;
        const UNKNOWN_RESET_BLOCK_MS: u64 = 30 * 60 * 1000;
        for (k, slots) in m.iter() {
            let model_id = health_model_id(k);
            for w in slots {
                if w.scope == "requests" {
                    continue;
                }
                if now.saturating_sub(w.updated_epoch_ms) > MAX_WINDOW_AGE_MS {
                    continue;
                }
                match w.reset_epoch_ms {
                    Some(r) if r <= now => continue,
                    None if w.remaining == Some(0)
                        && now.saturating_sub(w.updated_epoch_ms) > UNKNOWN_RESET_BLOCK_MS =>
                    {
                        continue
                    }
                    _ => {}
                }
                if let Some(rem) = w.remaining {
                    let e = out.entry(model_id.to_string()).or_insert(0);
                    *e = (*e).max(rem);
                }
            }
        }
        out
    }

    pub fn snapshot(&self) -> Value {
        let m = self.inner.lock();
        match m {
            Ok(m) => serde_json::to_value(&*m).unwrap_or(serde_json::Value::Null),
            Err(_) => serde_json::Value::Null,
        }
    }
}

/// The model portion of a composite health id ("model\x1fN" -> "model").
pub fn health_model_id(health_id: &str) -> &str {
    health_id.split('\u{1f}').next().unwrap_or(health_id)
}

/// RFC3339 (Z suffix) -> epoch ms. Minimal parser: 2026-09-29T12:34:56Z.
fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    // Tolerant RFC3339: accepts fractional seconds and +HH:MM/-HH:MM/Z offsets.
    // 偏移只会出现在时间部分（index ≥ 10）——从日期分隔符之后起找，
    // 否则 "2026-10-07" 的 '-' 命中 index 4，时区偏移被永久丢弃
    let (main, offset_ms) = match s[10..].find(['+', '-']).map(|p| p + 10) {
        Some(pos) => {
            let (m, off) = s.split_at(pos);
            // off: +HH:MM or -HH:MM
            let sign = if off.starts_with('-') { -1i64 } else { 1i64 };
            let h: i64 = off.get(1..3)?.parse().ok()?;
            let mi: i64 = off.get(4..6).unwrap_or("00").parse().ok()?;
            (m.to_string(), sign * (h * 3600 + mi * 60) * 1000)
        }
        _ => (s.to_string(), 0),
    };
    let bytes = main.as_bytes();
    // find 'Z' or strip fractional seconds before it
    let main = if bytes.len() > 19 && bytes[19] == b'Z' {
        main[..19].to_string()
    } else if bytes.len() > 19 && bytes[19] == b'.' {
        // fractional seconds: find terminator
        let z = main[19..].find(['Z', '+', '-'])? + 19;
        main[..z].to_string()
    } else {
        main
    };
    let bytes = main.as_bytes();
    if (bytes.len() < 19 || (bytes.len() == 20 && bytes[19] != b'Z'))
        && bytes.len() < 19 {
            return None;
        }
    let num = |a: usize, b: usize| main.get(a..b)?.parse::<i64>().ok();
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let min = num(14, 16)?;
    let sec = num(17, 19)?;
    // days since epoch (civil algorithm)
    let days = |y: i64, m: i64, d: i64| -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    };
    let epoch = days(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec;
    let base = (epoch.max(0) as u64).saturating_mul(1000);
    Some(base.saturating_add_signed(-offset_ms))
}

/// OpenAI reset duration ("1s", "6m0s", "1h2m3s") -> ms.
fn parse_duration_ms(s: &str) -> Option<u64> {
    let mut total_ms = 0u64;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let v: u64 = num.parse().ok()?;
            num.clear();
            total_ms += match c {
                's' => v * 1000,
                'm' => v * 60_000,
                'h' => v * 3_600_000,
                'd' => v * 86_400_000,
                _ => return None,
            };
        }
    }
    Some(total_ms)
}

impl Default for QuotaLedger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;

    fn hdr(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn parses_anthropic_unified_windows() {
        let h = hdr(&[
            ("anthropic-ratelimit-unified-5h-token-remaining", "45000"),
            ("anthropic-ratelimit-unified-5h-token-limit", "100000"),
            ("anthropic-ratelimit-unified-5h-token-reset", "2026-09-29T12:34:56Z"),
            ("anthropic-ratelimit-unified-7d-token-remaining", "900000"),
        ]);
        let w = QuotaLedger::parse_headers(&h);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].scope, "5h");
        assert_eq!(w[0].remaining, Some(45_000));
        assert!(w[0].reset_epoch_ms.is_some());
        assert_eq!(w[1].scope, "7d");
    }

    #[test]
    fn parses_openai_windows() {
        let h = hdr(&[
            ("x-ratelimit-remaining-tokens", "12000"),
            ("x-ratelimit-limit-tokens", "90000"),
            ("x-ratelimit-reset-tokens", "6m0s"),
            ("x-ratelimit-remaining-requests", "58"),
        ]);
        let w = QuotaLedger::parse_headers(&h);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].scope, "tokens");
        // 相对时长换算为绝对纪元（now + 6m），消费端按 epoch 比较
        let now = crate::quota::now_ms();
        match w[0].reset_epoch_ms {
            Some(t) => assert!(
                t >= now + 350_000 && t <= now + 370_000,
                "reset 应为 now+6m，实际 {t} (now={now})"
            ),
            None => panic!("reset_epoch_ms 应存在"),
        }
        assert_eq!(w[1].scope, "requests");
    }

    #[test]
    fn model_view_takes_best_key_and_ignores_expired() {
        let led = QuotaLedger::new();
        led.observe(
            "mini\u{1f}0",
            QuotaLedger::parse_headers(&hdr(&[("x-ratelimit-remaining-tokens", "1000")])),
        );
        led.observe(
            "mini\u{1f}1",
            QuotaLedger::parse_headers(&hdr(&[("x-ratelimit-remaining-tokens", "80000")])),
        );
        led.observe(
            "standard\u{1f}0",
            QuotaLedger::parse_headers(&hdr(&[("x-ratelimit-remaining-tokens", "50")])),
        );
        assert_eq!(led.model_remaining_tokens("mini"), Some(80_000));
        assert_eq!(led.model_remaining_tokens("standard"), Some(50));
        assert_eq!(led.model_remaining_tokens("frontier"), None);
    }
}


#[cfg(test)]
mod plan_usage_tests {
    use super::*;

    #[test]
    fn note_and_sum_roundtrip() {
        let l = QuotaLedger::new();
        let now = now_ms();
        l.note_plan_usage("m1", 13.0, 0.0, 10.0, now);
        let sums = l.plan_usage_sums(now, 5 * 3600 * 1000);
        assert_eq!(sums.get("m1"), Some(&(13.0, 0.0, 10.0)), "sums={:?}", sums);
    }
}
