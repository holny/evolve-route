use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct EventLog {
    inner: Arc<Inner>,
}

struct Inner {
    file: Option<Mutex<File>>,
    path: Option<PathBuf>,
}

impl EventLog {
    pub fn open(dir: &str) -> Self {
        let expanded = shellexpand_home(dir);
        let path = expanded.join("events.jsonl");
        let file = std::fs::create_dir_all(&expanded)
            .ok()
            .and_then(|_| OpenOptions::new().create(true).append(true).open(&path).ok());
        if file.is_none() {
            tracing::warn!(?dir, "event log unavailable, running without persistence");
        }
        Self { inner: Arc::new(Inner { file: file.map(Mutex::new), path: Some(path) }) }
    }

    pub fn disabled() -> Self {
        Self { inner: Arc::new(Inner { file: None, path: None }) }
    }

    pub fn path(&self) -> Option<&PathBuf> {
        self.inner.path.as_ref()
    }

    pub fn record(&self, mut event: Value) {
        const ROTATE_BYTES: u64 = 32 * 1024 * 1024;
        // simple size-based rotation: events.jsonl -> events.jsonl.1
        // 锁守卫必须在独立作用域内释放：Rust2024 let-chain 守卫会存活到
        // 整条 if 结束，块内再 lock 同一把锁会永久自死锁（32MB 后网关挂死）
        let needs_rotate = match &self.inner.file {
            Some(f) => {
                let g = f.lock().unwrap_or_else(|p| p.into_inner());
                g.metadata().map(|m| m.len() > ROTATE_BYTES).unwrap_or(false)
            }
            None => false,
        };
        if needs_rotate
            && let Some(path) = &self.inner.path
        {
            let rotated = path.with_extension("jsonl.1");
            let _ = std::fs::rename(path, &rotated);
            if let Some(f) = &self.inner.file {
                let mut g = f.lock().unwrap_or_else(|p| p.into_inner());
                if let Ok(nf) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    *g = nf;
                }
            }
        }
        if let Some(ts) = now_millis() {
            event["ts"] = serde_json::json!(ts);
        }
        let line = event.to_string();
        if let Some(f) = &self.inner.file
            && let Ok(mut f) = f.lock() {
                let _ = writeln!(f, "{line}");
            }
        tracing::info!(target: "ev::event", "{line}");
    }
}

pub fn now_millis() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

pub fn shellexpand_home_pub(dir: &str) -> PathBuf {
    shellexpand_home(dir)
}

fn shellexpand_home(dir: &str) -> PathBuf {
    if dir.starts_with("~/")
        && let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(&dir[2..]);
        }
    PathBuf::from(dir)
}
