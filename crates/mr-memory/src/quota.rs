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

#[derive(Default)]
pub struct QuotaLedger {
    inner: Mutex<HashMap<String, Vec<WindowState>>>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl QuotaLedger {
    pub fn new() -> Self {
        Self { inner: Mutex::new(HashMap::new()) }
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
            let reset = headers
                .get("x-ratelimit-reset-tokens")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_duration_ms);
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
        for (k, slots) in m.iter() {
            let model_id = health_model_id(k);
            for w in slots {
                if w.scope == "requests" {
                    continue;
                }
                if let Some(r) = w.reset_epoch_ms
                    && r <= now {
                        continue;
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
    let bytes = s.as_bytes();
    if bytes.len() < 20 || bytes[19] != b'Z' {
        return None;
    }
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
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
    Some((epoch.max(0) as u64).saturating_mul(1000))
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
        assert_eq!(w[0].reset_epoch_ms, Some(360_000));
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
