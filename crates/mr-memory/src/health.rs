//! Upstream health registry: classifies upstream failures into health kinds
//! and cools models down so the router avoids dead/unfunded ones.

use mr_core::types::*;
use std::collections::HashMap;
use std::sync::Mutex;

pub struct HealthRegistry {
    inner: Mutex<HashMap<String, HealthEntry>>,
}

pub struct Failure {
    pub kind: HealthKind,
    pub message: String,
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self { inner: Mutex::new(HashMap::new()) }
    }

    pub fn snapshot(&self) -> HealthMap {
        self.inner.lock().map(|m| m.clone()).unwrap_or_default()
    }

    pub fn mark_ok(&self, model_id: &str) {
        if let Ok(mut m) = self.inner.lock()
            && let Some(e) = m.get_mut(model_id)
        {
            // success resets the escalation counter so a recovered model
            // doesn't inherit multiplied cooldowns from its outage
            e.hits = 0;
            e.kind = HealthKind::Ok;
            e.until_epoch_ms = None;
            e.message.clear();
            e.updated_at = now();
        }
    }

    pub fn mark_failure(&self, model_id: &str, f: Failure) {
        let Ok(mut m) = self.inner.lock() else { return };
        let entry = m.entry(model_id.to_string()).or_insert_with(|| HealthEntry {
            kind: HealthKind::Ok,
            until_epoch_ms: None,
            message: String::new(),
            hits: 0,
            updated_at: 0,
        });
        entry.hits += 1;
        // escalate cooldown when repeat failures stack
        let base = f.kind.default_cooldown_ms();
        let cooldown = base.saturating_mul(entry.hits.min(4) as u64);
        entry.kind = f.kind;
        entry.message = f.message;
        entry.until_epoch_ms = Some(now() + cooldown);
        entry.updated_at = now();
    }
}

impl Default for HealthRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Classify an upstream error response into a health kind.
/// `body_snippet` is the (truncated) response body text; message keywords
/// refine 429s into quota-exhausted vs plain rate limits.
pub fn classify_failure(status: u16, body_snippet: &str, retry_after_ms: Option<u64>) -> Failure {
    let lower = body_snippet.to_lowercase();
    let no_credit = ["insufficient balance", "insufficient_quota", "balance", "arrears", "欠费", "余额不足", "no credit", "payment required", "billing"];
    let quota = ["quota", "exceeded your", "usage limit", "limit reached", "rate limit exceeded", "额度", "配额", "用量", "超限"];

    match status {
        404 => Failure {
            kind: HealthKind::Unsupported,
            message: first_hit(&lower, &["unsupported", "not support", "does not exist", "not found"])
                .unwrap_or("model not found on upstream")
                .into(),
        },
        401 | 403 => Failure {
            kind: HealthKind::AuthFailed,
            message: first_hit(&lower, &["invalid", "unauthorized", "forbidden", "无权", "鉴权"])
                .unwrap_or("auth rejected by upstream")
                .into(),
        },
        402 => Failure {
            kind: HealthKind::PaymentRequired,
            message: first_hit(&lower, &no_credit).unwrap_or("payment required").into(),
        },
        429 => {
            if no_credit.iter().any(|k| lower.contains(k)) {
                Failure { kind: HealthKind::PaymentRequired, message: "no credit".into() }
            } else if quota.iter().any(|k| lower.contains(k)) {
                Failure { kind: HealthKind::QuotaExhausted, message: "quota window exhausted".into() }
            } else {
                Failure {
                    kind: HealthKind::RateLimited,
                    message: match retry_after_ms {
                        Some(ms) => format!("retry after {}s", ms / 1000),
                        None => "rate limited".into(),
                    },
                }
            }
        }
        s if s >= 500 => Failure { kind: HealthKind::Transient, message: format!("upstream {s}") },
        _ => Failure { kind: HealthKind::Transient, message: format!("upstream {status}") },
    }
}

fn first_hit<'a>(lower: &str, keys: &[&'a str]) -> Option<&'a str> {
    keys.iter().find(|k| lower.contains(**k)).copied()
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_deepseek_balance_error() {
        let f = classify_failure(402, "{\"error\":{\"message\":\"Insufficient Balance\"}}", None);
        assert_eq!(f.kind, HealthKind::PaymentRequired);
    }

    #[test]
    fn classifies_quota_429() {
        let f = classify_failure(429, "You have exceeded your usage limit", None);
        assert_eq!(f.kind, HealthKind::QuotaExhausted);
    }

    #[test]
    fn classifies_plain_429_with_retry_after() {
        let f = classify_failure(429, "too many requests", Some(5000));
        assert_eq!(f.kind, HealthKind::RateLimited);
        assert!(f.message.contains("5s"));
    }

    #[test]
    fn classifies_auth_and_transient() {
        assert_eq!(classify_failure(401, "invalid api key", None).kind, HealthKind::AuthFailed);
        assert_eq!(classify_failure(503, "overloaded", None).kind, HealthKind::Transient);
        assert_eq!(classify_failure(529, "overloaded", None).kind, HealthKind::Transient);
    }

    #[test]
    fn repeat_failures_extend_cooldown() {
        let reg = HealthRegistry::new();
        for _ in 0..3 {
            reg.mark_failure("m", classify_failure(429, "rate limit exceeded", None));
        }
        let snap = reg.snapshot();
        let e = snap.get("m").unwrap();
        assert_eq!(e.hits, 3);
        assert!(e.cooldown_remaining_ms(now()).unwrap_or(0) >= 120_000, "3 hits x base 60s");
        reg.mark_ok("m");
        assert!(reg.snapshot().get("m").unwrap().available(now()));
    }
}
