//! Durable JSONL log for the test-mode stress harness.
//!
//! One directory per run:
//!   run-<timestamp>/
//!     events.jsonl          every event, global order
//!     accounts/<slot>.jsonl per-page/account slice (same records)
//!     state.json            restart checkpoint (atomic replace)
//!     summary.json          final counters / error tail
//!
//! Nothing here is opened unless `[test].enabled = true`; production builds
//! keep the default `TestConfig` and never touch this module.

use anyhow::{Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CurrentMatch {
    pub match_id: String,
    pub round: u32,
    pub slot: String,
    pub account: String,
    pub phase: String,
    pub started_at: String,
    pub completed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Checkpoint {
    pub run_id: String,
    pub run_dir: String,
    pub status: String,
    pub next_round: u32,
    pub next_account: usize,
    pub completed_matches: u32,
    pub current: Option<CurrentMatch>,
    pub updated_at: String,
}

impl Default for Checkpoint {
    fn default() -> Self {
        Self {
            run_id: String::new(),
            run_dir: String::new(),
            status: "running".to_string(),
            next_round: 1,
            next_account: 0,
            completed_matches: 0,
            current: None,
            updated_at: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestEvent {
    pub seq: u64,
    pub ts: String,
    pub ts_ms: i64,
    pub run_id: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_game: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

impl TestEvent {
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            seq: 0,
            ts: String::new(),
            ts_ms: 0,
            run_id: String::new(),
            kind: kind.into(),
            round: None,
            slot: None,
            account: None,
            match_id: None,
            phase: None,
            screen: None,
            url: None,
            ready_state: None,
            in_game: None,
            finish_count: None,
            detail: None,
            error: None,
            extra: None,
        }
    }

    pub fn round(mut self, round: u32) -> Self {
        self.round = Some(round);
        self
    }
    pub fn slot(mut self, slot: impl Into<String>) -> Self {
        self.slot = Some(slot.into());
        self
    }
    pub fn account(mut self, account: impl Into<String>) -> Self {
        self.account = Some(account.into());
        self
    }
    pub fn match_id(mut self, match_id: impl Into<String>) -> Self {
        self.match_id = Some(match_id.into());
        self
    }
    pub fn phase(mut self, phase: impl Into<String>) -> Self {
        self.phase = Some(phase.into());
        self
    }
    pub fn screen(mut self, screen: impl Into<String>) -> Self {
        self.screen = Some(screen.into());
        self
    }
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }
    pub fn ready_state(mut self, ready: impl Into<String>) -> Self {
        self.ready_state = Some(ready.into());
        self
    }
    pub fn in_game(mut self, in_game: bool) -> Self {
        self.in_game = Some(in_game);
        self
    }
    pub fn finish_count(mut self, finish_count: usize) -> Self {
        self.finish_count = Some(finish_count);
        self
    }
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
    pub fn error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }
    pub fn extra(mut self, extra: serde_json::Value) -> Self {
        self.extra = Some(extra);
        self
    }
}

struct Inner {
    global: File,
    per_slot: HashMap<String, File>,
    seq: u64,
}

pub struct TestLogger {
    run_id: String,
    run_dir: PathBuf,
    inner: Mutex<Inner>,
    events_written: AtomicU64,
}

impl TestLogger {
    pub fn open(root: &Path, resume: bool) -> Result<(std::sync::Arc<Self>, Option<Checkpoint>)> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("create test log root {}", root.display()))?;

        let mut checkpoint = None;
        let run_dir = if resume {
            match read_latest(root) {
                Some(dir) if dir.exists() => {
                    let state = read_checkpoint(&dir);
                    if state
                        .as_ref()
                        .map(|c| c.status == "running")
                        .unwrap_or(false)
                    {
                        checkpoint = state;
                        dir
                    } else {
                        new_run_dir(root)
                    }
                }
                _ => new_run_dir(root),
            }
        } else {
            new_run_dir(root)
        };
        std::fs::create_dir_all(run_dir.join("accounts"))
            .with_context(|| format!("create test run dir {}", run_dir.display()))?;

        let run_id = run_dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "run".to_string());
        let events_path = run_dir.join("events.jsonl");
        let global = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&events_path)
            .context("open test events.jsonl")?;
        // Appending to a resumed run: continue the global sequence instead of
        // restarting at 1 (duplicate seq values make a merged log ambiguous).
        let seq = if checkpoint.is_some() {
            last_event_seq(&events_path).unwrap_or(0)
        } else {
            0
        };

        let logger = std::sync::Arc::new(Self {
            run_id: run_id.clone(),
            run_dir: run_dir.clone(),
            inner: Mutex::new(Inner {
                global,
                per_slot: HashMap::new(),
                seq,
            }),
            events_written: AtomicU64::new(0),
        });

        // `latest.json` is a tiny pointer for humans/tools; state.json is the
        // actual checkpoint. Both are harmless if the process dies mid-write.
        let latest = serde_json::json!({
            "run_id": run_id,
            "run_dir": run_dir.to_string_lossy(),
            "updated_at": now(),
        });
        write_atomic(&root.join("latest.json"), &latest.to_string())?;

        if let Some(cp) = checkpoint.as_mut() {
            cp.run_dir = run_dir.to_string_lossy().to_string();
            cp.run_id = run_id.clone();
        } else {
            checkpoint = None;
        }
        if checkpoint.is_none() {
            let cp = Checkpoint {
                run_id: run_id.clone(),
                run_dir: run_dir.to_string_lossy().to_string(),
                status: "running".into(),
                next_round: 1,
                next_account: 0,
                completed_matches: 0,
                current: None,
                updated_at: now(),
            };
            logger.checkpoint(&cp)?;
        }
        Ok((logger, checkpoint))
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    pub fn events_written(&self) -> u64 {
        self.events_written.load(Ordering::Relaxed)
    }

    pub fn log(&self, mut event: TestEvent) -> Result<()> {
        let mut inner = self.inner.lock().expect("test logger mutex");
        inner.seq += 1;
        event.seq = inner.seq;
        event.run_id = self.run_id.clone();
        event.ts = now();
        event.ts_ms = Local::now().timestamp_millis();

        let line = serde_json::to_string(&event).context("serialize test event")?;
        writeln!(inner.global, "{line}").context("write test events.jsonl")?;
        if let Some(slot) = event.slot.as_deref() {
            if !inner.per_slot.contains_key(slot) {
                let path = self.run_dir.join("accounts").join(format!("{slot}.jsonl"));
                let file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .with_context(|| format!("open per-account test log {}", path.display()))?;
                inner.per_slot.insert(slot.to_string(), file);
            }
            if let Some(file) = inner.per_slot.get_mut(slot) {
                writeln!(file, "{line}").context("write per-account test log")?;
            }
        }
        self.events_written.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn checkpoint(&self, cp: &Checkpoint) -> Result<()> {
        let body = serde_json::to_string_pretty(cp).context("serialize test checkpoint")?;
        write_atomic(&self.run_dir.join("state.json"), &body)
    }

    pub fn summary(&self, summary: &serde_json::Value) -> Result<()> {
        let body = serde_json::to_string_pretty(summary).context("serialize test summary")?;
        write_atomic(&self.run_dir.join("summary.json"), &body)
    }
}

fn new_run_dir(root: &Path) -> PathBuf {
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut candidate = root.join(format!("run-{stamp}"));
    let mut n = 2;
    while candidate.exists() {
        candidate = root.join(format!("run-{stamp}-{n}"));
        n += 1;
    }
    candidate
}

fn read_latest(root: &Path) -> Option<PathBuf> {
    let body = std::fs::read_to_string(root.join("latest.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
    value
        .get("run_dir")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
}

fn last_event_seq(path: &Path) -> Option<u64> {
    let body = std::fs::read_to_string(path).ok()?;
    body.lines()
        .rev()
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .and_then(|v| v.get("seq").and_then(|s| s.as_u64()))
}

fn read_checkpoint(run_dir: &Path) -> Option<Checkpoint> {
    let body = std::fs::read_to_string(run_dir.join("state.json")).ok()?;
    serde_json::from_str(&body).ok()
}

pub fn now() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

fn write_atomic(path: &Path, body: &str) -> Result<()> {
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    ));
    std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}
