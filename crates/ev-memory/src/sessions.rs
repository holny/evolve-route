use ev_core::types::StickyState;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const CAPACITY: usize = 512;

#[derive(Debug, Clone)]
struct Entry {
    state: StickyState,
    last_used: u64,
}

/// Tool calls issued by a model in its last response, waiting to be
/// confirmed by role=tool messages in the next request (L3 signal).
#[derive(Debug, Clone)]
pub struct PendingCalls {
    pub model: String,
    pub call_ids: Vec<String>,
}

#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<InnerStore>,
}

struct InnerStore {
    map: Mutex<HashMap<String, Entry>>,
    pending: Mutex<HashMap<String, PendingCalls>>,
    clock: std::sync::atomic::AtomicU64,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(InnerStore {
                map: Mutex::new(HashMap::new()),
                pending: Mutex::new(HashMap::new()),
                clock: std::sync::atomic::AtomicU64::new(0),
            }),
        }
    }

    pub fn set_pending(&self, key: &str, model: &str, call_ids: Vec<String>) {
        if call_ids.is_empty() {
            return;
        }
        if let Ok(mut p) = self.inner.pending.lock() {
            p.insert(
                key.to_string(),
                PendingCalls { model: model.to_string(), call_ids },
            );
        }
    }

    pub fn take_pending(&self, key: &str) -> Option<PendingCalls> {
        self.inner.pending.lock().ok()?.remove(key)
    }

    pub fn get(&self, key: &str) -> Option<StickyState> {
        let mut map = self.inner.map.lock().ok()?;
        let tick = self.inner.clock.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        map.get_mut(key).map(|e| {
            e.last_used = tick;
            e.state.clone()
        })
    }

    /// 反馈解析：插件通常不知道 agent，按候选 agent 前缀逐个尝试，
    /// 最后回退旧式纯 session 键（兼容 2026-10-06 之前写入的状态）
    pub fn get_by_session(&self, agents: &[String], session: &str) -> Option<StickyState> {
        for a in agents {
            if let Some(s) = self.get(&format!("{}\u{1f}{}", a, session)) {
                return Some(s);
            }
        }
        self.get(session)
    }

    pub fn put(&self, key: &str, state: StickyState) {
        let Ok(mut map) = self.inner.map.lock() else { return };
        if map.len() >= CAPACITY && !map.contains_key(key)
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        let tick = self.inner.clock.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        map.insert(key.to_string(), Entry { state, last_used: tick });
    }

    pub fn decrement_turns(&self, key: &str) {
        let Ok(mut map) = self.inner.map.lock() else { return };
        if let Some(e) = map.get_mut(key) {
            e.state.turns_left = e.state.turns_left.saturating_sub(1);
        }
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}
