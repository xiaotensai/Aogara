//! Test-mode-only stress harness.
//!
//! Enabled exclusively by `[test].enabled = true` in the loaded config. It
//! runs five (or a configured subset of) sessions sequentially, records each
//! browser's page state, and can hard-exit a match at a chosen phase or on a
//! manual `test_force_exit` command. A checkpoint in `state.json` lets the
//! same run continue after an app restart.
//!
//! The production paths (`pool_start_inner`, capture supervisor, Tauri IPC)
//! are reused; this module owns only orchestration, logging, and forced
//! shutdown.

mod logger;
mod monitor;

pub use logger::{Checkpoint, CurrentMatch, TestEvent, TestLogger};

use crate::config::{ForceExitMode, TestConfig};
use crate::ipc::AppState;
use crate::pool;
use crate::session::SessionRuntime;
use crate::testing::monitor::{run_page_monitor, PageSnapshot, PageStates};
use anyhow::{anyhow, Context, Result};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::AppHandle;
use tokio::sync::{Mutex, RwLock};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TestStatus {
    pub running: bool,
    pub enabled: bool,
    pub run_id: String,
    pub run_dir: String,
    pub rounds: u32,
    pub accounts: Vec<String>,
    pub completed_matches: u32,
    pub current: Option<CurrentMatch>,
    pub events_written: u64,
    pub last_error: Option<String>,
}

pub struct TestController {
    running: AtomicBool,
    stop: Arc<AtomicBool>,
    force_slot: Arc<Mutex<Option<String>>>,
    status: RwLock<TestStatus>,
    logger: RwLock<Option<Arc<TestLogger>>>,
    current: Arc<RwLock<Option<CurrentMatch>>>,
    pages: PageStates,
}

impl Default for TestController {
    fn default() -> Self {
        Self::new()
    }
}

impl TestController {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            stop: Arc::new(AtomicBool::new(false)),
            force_slot: Arc::new(Mutex::new(None)),
            status: RwLock::new(TestStatus::default()),
            logger: RwLock::new(None),
            current: Arc::new(RwLock::new(None)),
            pages: Arc::new(RwLock::new(std::collections::HashMap::new())),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub async fn status(&self, state: &AppState) -> TestStatus {
        let mut status = self.status.read().await.clone();
        status.enabled = state.config.read().await.test.enabled;
        status.running = self.running.load(Ordering::SeqCst);
        status.current = self.current.read().await.clone();
        if let Some(logger) = self.logger.read().await.as_ref() {
            status.events_written = logger.events_written();
        }
        status
    }

    pub async fn start(self: &Arc<Self>, app: AppHandle, state: AppState) -> Result<TestStatus> {
        let cfg = state.config.read().await.test.clone();
        if !cfg.enabled {
            return Err(anyhow!(
                "测试模式未启用：在配置中设置 [test] enabled = true 后再启动"
            ));
        }
        if self.running.load(Ordering::SeqCst) {
            return Ok(self.status(&state).await);
        }

        let root = crate::util::resolve_dir(&cfg.log_dir);
        let (logger, checkpoint) = TestLogger::open(&root, cfg.resume)?;
        let run_id = logger.run_id().to_string();
        let run_dir = logger.run_dir().to_string_lossy().to_string();
        *self.logger.write().await = Some(logger.clone());
        self.stop.store(false, Ordering::SeqCst);
        self.running.store(true, Ordering::SeqCst);
        state
            .auto_next_global
            .store(false, Ordering::SeqCst);

        {
            let mut status = self.status.write().await;
            status.enabled = true;
            status.running = true;
            status.run_id = run_id.clone();
            status.run_dir = run_dir.clone();
            status.rounds = cfg.rounds;
            status.completed_matches = checkpoint
                .as_ref()
                .map(|c| c.completed_matches)
                .unwrap_or(0);
            status.last_error = None;
        }

        let controller = self.clone();
        let state_for_run = state.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = run_all(
                app,
                state_for_run.clone(),
                cfg.clone(),
                logger.clone(),
                checkpoint,
                controller.clone(),
            )
            .await
            {
                let text = format!("{e:#}");
                tracing::error!("[test] runner failed: {text}");
                let _ = logger.log(TestEvent::new("test_failed").error(text.clone()));
                controller.status.write().await.last_error = Some(text);
            }
            controller.running.store(false, Ordering::SeqCst);
            controller.stop.store(false, Ordering::SeqCst);
            controller.status.write().await.running = false;
            if let Some(log) = controller.logger.read().await.clone() {
                let st = controller.status(&state_for_run).await;
                let _ = log.summary(&json!({
                    "run_id": st.run_id,
                    "run_dir": st.run_dir,
                    "completed_matches": st.completed_matches,
                    "events_written": st.events_written,
                    "last_error": st.last_error,
                    "finished_at": logger::now(),
                }));
            }
        });

        Ok(self.status(&state).await)
    }

    pub async fn stop(self: &Arc<Self>, state: &AppState) -> Result<TestStatus> {
        if !self.running.load(Ordering::SeqCst) {
            return Ok(self.status(state).await);
        }
        self.stop.store(true, Ordering::SeqCst);
        if let Some(cur) = self.current.read().await.clone() {
            *self.force_slot.lock().await = Some(cur.slot.clone());
        }
        if let Some(log) = self.logger.read().await.clone() {
            let _ = log.log(TestEvent::new("test_stopping").detail("requested by IPC"));
        }
        Ok(self.status(state).await)
    }

    pub async fn force_exit(
        self: &Arc<Self>,
        slot: Option<String>,
        reason: Option<String>,
        state: &AppState,
    ) -> Result<TestStatus> {
        if !self.running.load(Ordering::SeqCst) {
            return Err(anyhow!("测试未在运行"));
        }
        let slot = match slot {
            Some(s) => s,
            None => self
                .current
                .read()
                .await
                .as_ref()
                .map(|c| c.slot.clone())
                .ok_or_else(|| anyhow!("当前没有进行中的测试对局"))?,
        };
        *self.force_slot.lock().await = Some(slot.clone());
        if let Some(log) = self.logger.read().await.clone() {
            let _ = log.log(
                TestEvent::new("force_exit_requested")
                    .slot(slot)
                    .detail(reason.unwrap_or_else(|| "manual".into())),
            );
        }
        Ok(self.status(state).await)
    }
}

#[derive(Clone)]
struct TestAccount {
    slot: String,
    account: String,
}

async fn run_all(
    app: AppHandle,
    state: AppState,
    cfg: TestConfig,
    logger: Arc<TestLogger>,
    checkpoint: Option<Checkpoint>,
    ctrl: Arc<TestController>,
) -> Result<()> {
    let resumed = checkpoint.is_some();
    let accounts = resolve_accounts(&state, &cfg).await?;
    if accounts.is_empty() {
        return Err(anyhow!("测试账号列表为空：配置 [test].accounts 或先创建会话"));
    }
    ctrl.status.write().await.accounts = accounts.iter().map(|a| a.slot.clone()).collect();
    tracing::info!(
        "[test] run {} accounts={:?} rounds={} force_exit={:?}",
        logger.run_id(),
        accounts.iter().map(|a| a.slot.as_str()).collect::<Vec<_>>(),
        cfg.rounds,
        cfg.force_exit
    );

    let mut cp = checkpoint.unwrap_or_else(|| Checkpoint {
        run_id: logger.run_id().to_string(),
        run_dir: logger.run_dir().to_string_lossy().to_string(),
        status: "running".into(),
        next_round: 1,
        next_account: 0,
        completed_matches: 0,
        current: None,
        updated_at: logger::now(),
    });
    cp.status = "running".into();
    cp.run_id = logger.run_id().to_string();
    cp.run_dir = logger.run_dir().to_string_lossy().to_string();
    logger.checkpoint(&cp)?;

    if let Some(cur) = cp.current.clone() {
        let _ = logger.log(
            TestEvent::new("test_recovered")
                .round(cur.round)
                .slot(cur.slot.clone())
                .account(cur.account.clone())
                .match_id(cur.match_id.clone())
                .detail("重启后发现未完成对局，先强制关闭再重试"),
        );
        if let Some(s) = state.sessions.get(&cur.slot).await {
            let _ = close_slot_browser(s).await;
        }
        cp.current = None;
        cp.next_round = cur.round;
        cp.next_account = account_index(&accounts, &cur.slot);
        logger.checkpoint(&cp)?;
    }

    let _ = logger.log(
        TestEvent::new("test_started")
            .detail(format!(
                "rounds={} accounts={} force_exit={:?}",
                cfg.rounds,
                accounts.len(),
                cfg.force_exit
            ))
            .extra(json!({
                "resume": resumed,
                "accounts": accounts.iter().map(|a| json!({"slot":a.slot,"account":a.account})).collect::<Vec<_>>(),
                "config": &cfg,
            })),
    );

    // One monitor per account, independent of match orchestration. It keeps
    // recording page_load/login/lobby/in_game/settlement and CDP errors.
    let mut monitor_handles = Vec::new();
    for acct in &accounts {
        let Some(session) = state.sessions.get(&acct.slot).await else {
            continue;
        };
        let port = {
            let cfg = session.config.read().await;
            pool::game_cdp_port(&cfg.capture.chromium.extra_args)
        };
        let Some(port) = port else {
            let _ = logger.log(
                TestEvent::new("error")
                    .slot(acct.slot.clone())
                    .account(acct.account.clone())
                    .error("会话没有固定 CDP 调试端口"),
            );
            continue;
        };
        monitor_handles.push(tauri::async_runtime::spawn(run_page_monitor(
            acct.slot.clone(),
            acct.account.clone(),
            port,
            Duration::from_millis(cfg.monitor_interval_ms.max(100)),
            Duration::from_secs(cfg.heartbeat_secs),
            logger.clone(),
            ctrl.current.clone(),
            ctrl.pages.clone(),
            session.signals.clone(),
            ctrl.stop.clone(),
        )));
    }

    let start_round = cp.next_round.max(1);
    let start_account = cp.next_account;
    let mut rng = StdRng::seed_from_u64(
        cfg.seed
            .unwrap_or_else(|| chrono::Local::now().timestamp_millis() as u64),
    );

    'rounds: for round in start_round..=cfg.rounds {
        if ctrl.stop.load(Ordering::SeqCst) {
            break;
        }
        let _ = logger.log(TestEvent::new("round_started").round(round));
        for (idx, acct) in accounts.iter().enumerate() {
            if ctrl.stop.load(Ordering::SeqCst) {
                break 'rounds;
            }
            if round == start_round && idx < start_account {
                continue;
            }
            let match_no = cp.completed_matches + 1;
            let match_id = format!(
                "{}-r{:02}-a{:02}-m{:03}",
                compact_run_id(logger.run_id()),
                round,
                idx,
                match_no
            );
            let outcome = run_one_match(
                &app,
                &state,
                &cfg,
                &logger,
                &ctrl,
                acct,
                round,
                &match_id,
                &mut cp,
                &mut rng,
            )
            .await;
            match outcome {
                Ok(()) => {}
                Err(e) => {
                    let text = format!("{e:#}");
                    let _ = logger.log(
                        TestEvent::new("error")
                            .round(round)
                            .slot(acct.slot.clone())
                            .account(acct.account.clone())
                            .match_id(match_id.clone())
                            .error(text.clone()),
                    );
                    ctrl.status.write().await.last_error = Some(text);
                }
            }
            cp.completed_matches += 1;
            cp.next_round = round;
            cp.next_account = idx + 1;
            if cp.next_account >= accounts.len() {
                cp.next_account = 0;
                cp.next_round = round + 1;
            }
            cp.current = None;
            cp.updated_at = logger::now();
            let _ = logger.checkpoint(&cp);
            ctrl.status.write().await.completed_matches = cp.completed_matches;
            let _ = logger.log(
                TestEvent::new("round_progress")
                    .round(round)
                    .slot(acct.slot.clone())
                    .account(acct.account.clone())
                    .match_id(match_id)
                    .detail(format!(
                        "{}/{} matches completed",
                        cp.completed_matches,
                        cfg.rounds as usize * accounts.len()
                    )),
            );
        }
        let _ = logger.log(TestEvent::new("round_finished").round(round));
    }

    let stopped = ctrl.stop.load(Ordering::SeqCst);
    cp.status = if stopped {
        "stopped".into()
    } else {
        "finished".into()
    };
    cp.current = None;
    cp.updated_at = logger::now();
    logger.checkpoint(&cp)?;
    let _ = logger.log(
        TestEvent::new(if stopped { "test_stopped" } else { "test_finished" })
            .detail(format!("{} matches", cp.completed_matches)),
    );

    for handle in monitor_handles {
        handle.abort();
    }
    Ok(())
}

fn compact_run_id(run_id: &str) -> String {
    run_id
        .strip_prefix("run-")
        .unwrap_or(run_id)
        .replace('-', "")
}

async fn resolve_accounts(state: &AppState, cfg: &TestConfig) -> Result<Vec<TestAccount>> {
    let mut sessions = state.sessions.all().await;
    sessions.sort_by(|a, b| a.id.cmp(&b.id));
    let mut out = Vec::new();
    if cfg.accounts.is_empty() {
        for s in sessions {
            let account = s.config.read().await.pool.selected_id.clone();
            if !account.is_empty() {
                out.push(TestAccount {
                    slot: s.id.clone(),
                    account,
                });
            }
        }
    } else {
        for spec in &cfg.accounts {
            let spec = spec.trim();
            if spec.is_empty() {
                continue;
            }
            // Support slot ids first; account ids are resolved in the async
            // loop below because `selected_id` lives behind a config lock.
            let session = sessions.iter().find(|s| s.id == spec).cloned();
            if let Some(s) = session {
                let account = s.config.read().await.pool.selected_id.clone();
                out.push(TestAccount {
                    slot: s.id.clone(),
                    account,
                });
                continue;
            }
            let mut found = None;
            for s in &sessions {
                if s.config.read().await.pool.selected_id == spec {
                    found = Some(s.clone());
                    break;
                }
            }
            if let Some(s) = found {
                out.push(TestAccount {
                    slot: s.id.clone(),
                    account: spec.to_string(),
                });
            } else {
                let _ = logger_placeholder_warn(spec);
            }
        }
    }
    // Keep configured order but drop accidental duplicates.
    let mut seen = std::collections::HashSet::new();
    out.retain(|a| seen.insert(a.slot.clone()));
    Ok(out)
}

fn logger_placeholder_warn(spec: &str) {
    tracing::warn!("[test] configured account/slot not found: {spec}");
}

fn account_index(accounts: &[TestAccount], slot: &str) -> usize {
    accounts
        .iter()
        .position(|a| a.slot == slot)
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
async fn run_one_match(
    app: &AppHandle,
    state: &AppState,
    cfg: &TestConfig,
    logger: &Arc<TestLogger>,
    ctrl: &Arc<TestController>,
    acct: &TestAccount,
    round: u32,
    match_id: &str,
    cp: &mut Checkpoint,
    rng: &mut StdRng,
) -> Result<()> {
    let session = state
        .sessions
        .get(&acct.slot)
        .await
        .ok_or_else(|| anyhow!("没有会话 {}", acct.slot))?;
    let session_cfg = session.config.read().await.clone();
    let account_id = if acct.account.is_empty() {
        session_cfg.pool.selected_id.clone()
    } else {
        acct.account.clone()
    };
    if account_id.is_empty() {
        return Err(anyhow!("会话 {} 没有选中账号", acct.slot));
    }
    let room = if cfg.room.trim().is_empty() {
        session_cfg.pool.target.room.clone()
    } else {
        cfg.room.clone()
    };
    let mode = if cfg.mode.trim().is_empty() {
        session_cfg.pool.target.mode.clone()
    } else {
        cfg.mode.clone()
    };
    let initial_finish = session.signals.snapshot().finish_count;
    let cur = CurrentMatch {
        match_id: match_id.to_string(),
        round,
        slot: acct.slot.clone(),
        account: account_id.clone(),
        phase: "starting".into(),
        started_at: logger::now(),
        completed: false,
    };
    *ctrl.current.write().await = Some(cur.clone());
    ctrl.status.write().await.current = Some(cur.clone());
    // Drop any phase left over from the previous match so a random
    // `settlement`/`lobby` target cannot fire before this match starts.
    ctrl.pages.write().await.insert(
        acct.slot.clone(),
        PageSnapshot {
            slot: acct.slot.clone(),
            account: account_id.clone(),
            phase: "starting".into(),
            screen: "starting".into(),
            updated_at: logger::now(),
            ..PageSnapshot::default()
        },
    );
    cp.current = Some(cur.clone());
    cp.next_round = round;
    cp.updated_at = logger::now();
    logger.checkpoint(cp)?;
    let _ = logger.log(
        TestEvent::new("match_starting")
            .round(round)
            .slot(acct.slot.clone())
            .account(account_id.clone())
            .match_id(match_id.to_string())
            .phase("starting")
            .detail(format!("{room}/{mode}")),
    );

    // Reset stale stop flags from a previous forced exit. This must happen
    // before the force watcher starts so a new match is not aborted instantly.
    session.pool_cancel.store(false, Ordering::SeqCst);
    session.paused.store(false, Ordering::SeqCst);
    *ctrl.force_slot.lock().await = None;

    let (watcher, forced) = spawn_exit_watcher(
        cfg,
        logger.clone(),
        ctrl.clone(),
        acct,
        session.clone(),
        match_id.to_string(),
        rng,
    )
    .await;

    let start_future = crate::ipc::commands::pool_start_inner(
        account_id.clone(),
        room.clone(),
        mode.clone(),
        false,
        app.clone(),
        state.with_session(session.clone()),
    );
    let start_deadline = Duration::from_secs(cfg.max_match_secs.max(60));
    let mut start_error = None;
    // Race the start sequence against the force watcher: `ensure_capture_running`
    // may wait for a CDP port that the watcher is about to close, and we do
    // not want a forced page_load exit to block this match for a full timeout.
    tokio::select! {
        res = tokio::time::timeout(start_deadline, start_future) => match res {
            Ok(Ok(result)) => {
                if !result.entered {
                    start_error = Some(result.message.clone());
                }
            }
            Ok(Err(e)) => start_error = Some(e),
            Err(_) => start_error = Some("开始对局超时".into()),
        },
        _ = wait_for_forced(forced.clone()) => {
            start_error = Some("强制退出中断进房流程".into());
        }
    }

    // Wait for either a natural finish, a forced exit, or the match timeout.
    let wait_deadline = Instant::now() + Duration::from_secs(cfg.max_match_secs.max(60));
    let mut exit_reason = None;
    let mut entered_logged = false;
    let mut last_sig = session.signals.snapshot();
    while !ctrl.stop.load(Ordering::SeqCst) {
        // Read the signal before the forced flag: a forced exit fired at the
        // exact in_game transition must still produce a match_entered event.
        let sig = session.signals.snapshot();
        last_sig = sig.clone();
        if sig.in_game && !entered_logged {
            entered_logged = true;
            let page = page_snapshot(ctrl, &acct.slot).await;
            let _ = logger.log(
                TestEvent::new("match_entered")
                    .round(round)
                    .slot(acct.slot.clone())
                    .account(account_id.clone())
                    .match_id(match_id.to_string())
                    .phase("in_game")
                    .in_game(true)
                    .finish_count(sig.finish_count)
                    .extra(json!({"page": page})),
            );
        }
        if forced.load(Ordering::SeqCst) {
            exit_reason = Some("force_exit");
            break;
        }
        if start_error.is_some() && !sig.in_game {
            exit_reason = Some("start_error");
            break;
        }
        if !sig.in_game && sig.finish_count > initial_finish {
            exit_reason = Some("natural");
            break;
        }
        if Instant::now() >= wait_deadline {
            exit_reason = Some("timeout");
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if exit_reason.is_none() {
        exit_reason = Some(if ctrl.stop.load(Ordering::SeqCst) {
            "test_stop"
        } else {
            "unknown"
        });
    }
    let exit_reason = exit_reason.unwrap_or("unknown");
    let before = page_snapshot(ctrl, &acct.slot).await;

    if let Some(reason) = start_error {
        // A start error caused by our own forced exit is expected, not an
        // exception; the force_exit_completed event carries the detail.
        if !forced.load(Ordering::SeqCst) {
            let _ = logger.log(
                TestEvent::new("error")
                    .round(round)
                    .slot(acct.slot.clone())
                    .account(account_id.clone())
                    .match_id(match_id.to_string())
                    .phase("entering")
                    .error(reason)
                    .extra(json!({"page": before})),
            );
        }
    }

    if exit_reason == "timeout" {
        let _ = logger.log(
            TestEvent::new("match_timeout")
                .round(round)
                .slot(acct.slot.clone())
                .account(account_id.clone())
                .match_id(match_id.to_string())
                .error("超过 max_match_secs，执行强制退出"),
        );
        let _ = close_slot_browser(session.clone()).await;
    } else if exit_reason == "natural" {
        // Keep the browser briefly so the monitor can observe 終局/大厅; then
        // close it so the next account starts from a clean profile.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let _ = close_slot_browser(session.clone()).await;
    } else if matches!(exit_reason, "start_error" | "test_stop" | "unknown") {
        // No watcher or a start failure: make sure the next account is not
        // inheriting a half-open browser/profile.
        let _ = close_slot_browser(session.clone()).await;
    }

    // Let the force watcher finish writing `force_exit_completed` and settle
    // the page snapshot before we record `match_exited`; aborting it here
    // would lose the exact exit page state.
    if forced.load(Ordering::SeqCst) {
        if let Some(w) = watcher {
            let _ = tokio::time::timeout(Duration::from_secs(6), w).await;
        }
    } else if let Some(w) = watcher {
        w.abort();
    }

    let after = page_snapshot(ctrl, &acct.slot).await;
    let _ = logger.log(
        TestEvent::new("match_exited")
            .round(round)
            .slot(acct.slot.clone())
            .account(account_id.clone())
            .match_id(match_id.to_string())
            .phase("exited")
            .in_game(last_sig.in_game)
            .finish_count(last_sig.finish_count)
            .detail(exit_reason)
            .extra(json!({
                "reason": exit_reason,
                "page_before": before,
                "page_after": after,
                "in_game_before_close": last_sig.in_game,
            })),
    );

    *ctrl.force_slot.lock().await = None;
    *ctrl.current.write().await = None;
    ctrl.status.write().await.current = None;
    session.pool_cancel.store(false, Ordering::SeqCst);
    session.paused.store(false, Ordering::SeqCst);
    Ok(())
}

fn phase_matches(target: &str, current: &str) -> bool {
    if target == current {
        return true;
    }
    // The loading state is often gone before the first 250ms sample (the
    // login canvas is already complete). Treat "page_load" as "the browser
    // page exists" so an immediate page-load exit still fires.
    target == "page_load"
        && matches!(
            current,
            "page_load" | "login" | "lobby" | "entering" | "unknown"
        )
}

async fn wait_for_forced(forced: Arc<AtomicBool>) {
    loop {
        if forced.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn page_snapshot(ctrl: &Arc<TestController>, slot: &str) -> PageSnapshot {
    ctrl.pages
        .read()
        .await
        .get(slot)
        .cloned()
        .unwrap_or_else(|| PageSnapshot {
            slot: slot.to_string(),
            phase: "unknown".into(),
            screen: "unknown".into(),
            updated_at: logger::now(),
            ..PageSnapshot::default()
        })
}

async fn spawn_exit_watcher(
    cfg: &TestConfig,
    logger: Arc<TestLogger>,
    ctrl: Arc<TestController>,
    acct: &TestAccount,
    session: Arc<SessionRuntime>,
    match_id: String,
    rng: &mut StdRng,
) -> (Option<tauri::async_runtime::JoinHandle<()>>, Arc<AtomicBool>) {
    let forced = Arc::new(AtomicBool::new(false));
    // Even with force_exit = none, the manual `test_force_exit` command and
    // an IPC stop need a watcher, so always spawn one.
    let phases = if cfg.phases().is_empty() {
        vec!["in_game".to_string()]
    } else {
        cfg.phases()
    };
    let (target_phase, delay_ms) = match cfg.force_exit {
        ForceExitMode::None => (String::new(), 0),
        ForceExitMode::Immediate => (phases.first().cloned().unwrap_or_default(), 0),
        ForceExitMode::Random => {
            let phase = phases[rng.random_range(0..phases.len())].clone();
            let lo = cfg.exit_delay_min_ms.min(cfg.exit_delay_max_ms);
            let hi = cfg.exit_delay_min_ms.max(cfg.exit_delay_max_ms);
            let delay = if hi > lo {
                rng.random_range(lo..=hi)
            } else {
                lo
            };
            (phase, delay)
        }
    };
    let slot = acct.slot.clone();
    let account = acct.account.clone();
    let stop = ctrl.stop.clone();
    let force_slot = ctrl.force_slot.clone();
    let pages = ctrl.pages.clone();
    let log = logger.clone();
    let forced_for_task = forced.clone();
    let handle = tauri::async_runtime::spawn(async move {
        let mut phase_seen: Option<Instant> = None;
        loop {
            if stop.load(Ordering::SeqCst) || forced_for_task.load(Ordering::SeqCst) {
                return;
            }
            let manual = {
                let mut slot_guard = force_slot.lock().await;
                if slot_guard.as_deref() == Some(slot.as_str()) {
                    slot_guard.take();
                    true
                } else {
                    false
                }
            };
            let phase = pages
                .read()
                .await
                .get(&slot)
                .map(|p| p.phase.clone())
                .unwrap_or_else(|| "unknown".into());
            let auto_hit = !target_phase.is_empty() && phase_matches(&target_phase, &phase);
            let hit = manual || auto_hit;
            if hit {
                if phase_seen.is_none() {
                    phase_seen = Some(Instant::now());
                }
                let elapsed = phase_seen.map(|t| t.elapsed()).unwrap_or_default();
                if manual || elapsed >= Duration::from_millis(delay_ms) {
                    let before = pages.read().await.get(&slot).cloned();
                    let _ = log.log(
                        TestEvent::new("force_exit_requested")
                            .slot(slot.clone())
                            .account(account.clone())
                            .match_id(match_id.clone())
                            .phase(phase.clone())
                            .detail(if manual {
                                "manual".to_string()
                            } else {
                                format!("phase={target_phase} delay={delay_ms}ms")
                            })
                            .extra(json!({"page": before})),
                    );
                    forced_for_task.store(true, Ordering::SeqCst);
                    // Abort the in-flight pool_start future at its next
                    // cancellation boundary as well as closing the browser.
                    session.pool_cancel.store(true, Ordering::SeqCst);
                    session.paused.store(true, Ordering::SeqCst);
                    let result = close_slot_browser(session.clone()).await;
                    let after = pages.read().await.get(&slot).cloned();
                    let mut ev = TestEvent::new(if result.is_ok() {
                        "force_exit_completed"
                    } else {
                        "force_exit_failed"
                    })
                    .slot(slot.clone())
                    .account(account.clone())
                    .match_id(match_id.clone())
                    .phase(phase.clone())
                    .extra(json!({"page_after": after}));
                    if let Err(e) = &result {
                        ev = ev.error(format!("{e:#}"));
                    }
                    let _ = log.log(ev);
                    return;
                }
            } else {
                phase_seen = None;
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
    });
    (Some(handle), forced)
}

pub async fn close_slot_browser(session: Arc<SessionRuntime>) -> Result<()> {
    {
        let mut ctl = session.capture_control.lock().await;
        if let Some(tx) = ctl.stop.take() {
            let _ = tx.send(());
        }
        ctl.force_close.notify_waiters();
    }
    for _ in 0..10 {
        let running = { session.capture_control.lock().await.stop.is_some() };
        if !running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    let profile = session.config.read().await.capture.chromium.user_data_dir.clone();
    let pd = crate::util::resolve_dir(Path::new(&profile));
    let result = tokio::task::spawn_blocking(move || {
        crate::capture::chromium::profile::reclaim_singleton(&pd)
    })
    .await
    .context("join browser reclaim task")?;
    if let Err(e) = result {
        return Err(anyhow!("强退浏览器失败: {e:#}"));
    }
    let mut ctl = session.capture_control.lock().await;
    ctl.stop = None;
    ctl.status = crate::schema::CaptureStatus::Stopped;
    Ok(())
}
