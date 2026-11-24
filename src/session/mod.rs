//! Per-account session runtime.
//!
//! P1 of the multi-account refactor: the handles that used to live directly
//! on `AppState` are grouped here, one set per account/session. The public
//! `AppState` fields remain as aliases for the single default session so the
//! existing IPC commands and task wiring keep working unchanged; later phases
//! switch commands to `AppState::sessions`.
//!
//! A `SessionRuntime` owns everything that must never be shared between two
//! accounts:
//!
//! - the log `Session` (and therefore the log directory),
//! - the eight event buses,
//! - the `GameTracker` and the analysis cache,
//! - the autoplay context and the capture control handle,
//! - the bot / autoplay "already started" flags,
//! - the pool cancel / soft-stop flags.
//!
//! What stays global: the config store, Python runtime, history store and the
//! updater. Those are either read-only for a session or explicitly shared.

use crate::analysis::runner::AnalysisCache;
use crate::autoplay::AutoplayContext;
use crate::config::AppConfig;
use crate::event_bus::{
    AnalysisBus, BotResponseBus, BotStatusBus, CaptureStatusBus, HistoryBus, MjaiBus, NotifyBus,
    PostTrackerBus,
};
use crate::game_state::GameTracker;
use crate::history::recorder::SharedPlatform;
use crate::history::HistoryStore;
use crate::logger::Session;
use crate::schema::mjai::MjaiEvent;
use crate::schema::{BotStatus, CaptureStatus};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::RwLock as StdRwLock;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex, Notify, RwLock};

/// Stable identifier for a session. For the slot model this is `slot1` …
/// `slot8`; the default single-session build uses [`DEFAULT_SESSION_ID`].
pub type SessionId = String;

/// The id used by the implicit single session until `[[sessions]]` exists.
pub const DEFAULT_SESSION_ID: &str = "slot1";

/// Per-running-capture-backend control handle.
///
/// `stop` is `Some` while the backend task is alive; sending `()`
/// triggers graceful shutdown and the task exits. After exit the
/// supervisor sets `stop = None` and updates `status`.
///
/// `force_close` is shared with the hudsucker handler when MITM is the
/// active backend; calling `notify_waiters()` kicks every in-flight
/// WebSocket flow so the game client actually disconnects. The Chromium
/// backend ignores it.
pub struct CaptureControl {
    pub status: CaptureStatus,
    pub stop: Option<oneshot::Sender<()>>,
    pub force_close: Arc<Notify>,
}

impl Default for CaptureControl {
    fn default() -> Self {
        Self {
            status: CaptureStatus::Stopped,
            stop: None,
            force_close: Arc::new(Notify::new()),
        }
    }
}

/// The eight broadcast senders belonging to one session.
///
/// Constructed by the caller (today `lib.rs`) so the task-spawning code can
/// subscribe before the runtime is wrapped in `AppState`.
#[derive(Clone)]
pub struct SessionBuses {
    pub mjai: MjaiBus,
    pub post_tracker: PostTrackerBus,
    pub bot_response: BotResponseBus,
    pub bot_status: BotStatusBus,
    pub capture_status: CaptureStatusBus,
    pub notify: NotifyBus,
    pub analysis: AnalysisBus,
    pub history: HistoryBus,
}

/// Game/pool signals derived from the session's `mjai_bus`, independent of
/// the log file. Sessions 2..N do not install a tracing subscriber, so their
/// `all.log` stays empty; the pool driver must not parse it.
#[derive(Default)]
pub struct SessionSignals {
    in_game: AtomicBool,
    finish_count: AtomicUsize,
    last_enter: StdRwLock<Option<String>>,
    last_finish: StdRwLock<Option<String>>,
}

/// One consistent read of [`SessionSignals`].
#[derive(Debug, Clone, Default)]
pub struct SignalSnapshot {
    pub in_game: bool,
    pub finish_count: usize,
    pub last_enter: Option<String>,
    pub last_finish: Option<String>,
}

impl SessionSignals {
    fn now() -> String {
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string()
    }

    pub fn apply(&self, ev: &MjaiEvent) {
        match ev {
            MjaiEvent::StartGame { .. } | MjaiEvent::StartKyoku { .. } => {
                self.in_game.store(true, Ordering::SeqCst);
                let mut slot = self.last_enter.write().unwrap();
                if slot.is_none() {
                    *slot = Some(Self::now());
                }
            }
            MjaiEvent::EndGame { .. } => {
                self.in_game.store(false, Ordering::SeqCst);
                self.finish_count.fetch_add(1, Ordering::SeqCst);
                *self.last_finish.write().unwrap() = Some(Self::now());
            }
            // A spurious `EndGame` (bridge emits one when the client briefly
            // leaves the table screen) must not keep `in_game=false` while the
            // match is still live: any real in-match event flips it back on.
            MjaiEvent::Tsumo { .. }
            | MjaiEvent::Dahai { .. }
            | MjaiEvent::Chi { .. }
            | MjaiEvent::Pon { .. }
            | MjaiEvent::Daiminkan { .. }
            | MjaiEvent::Kakan { .. }
            | MjaiEvent::Ankan { .. }
            | MjaiEvent::Dora { .. }
            | MjaiEvent::Reach { .. }
            | MjaiEvent::ReachAccepted { .. }
            | MjaiEvent::Hora { .. }
            | MjaiEvent::Ryukyoku { .. }
            | MjaiEvent::Kita { .. } => {
                self.in_game.store(true, Ordering::SeqCst);
            }
            _ => {}
        }
    }

    pub fn snapshot(&self) -> SignalSnapshot {
        SignalSnapshot {
            in_game: self.in_game.load(Ordering::SeqCst),
            finish_count: self.finish_count.load(Ordering::SeqCst),
            last_enter: self.last_enter.read().unwrap().clone(),
            last_finish: self.last_finish.read().unwrap().clone(),
        }
    }
}

/// Mirrors this session's `mjai_bus` into [`SessionSignals`].
pub async fn run_signal_tracker(mut rx: tokio::sync::broadcast::Receiver<MjaiEvent>, signals: Arc<SessionSignals>) {
    loop {
        match rx.recv().await {
            Ok(ev) => signals.apply(&ev),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Everything that belongs to one account/session and must not be shared.
pub struct SessionRuntime {
    pub id: SessionId,
    pub config: Arc<RwLock<AppConfig>>,
    pub config_path: Arc<PathBuf>,
    pub log_session: Arc<Session>,
    pub history_store: Arc<HistoryStore>,
    pub buses: SessionBuses,

    /// Latest `BotStatus` seen on this session's bus.
    pub bot_status: Arc<RwLock<BotStatus>>,
    pub capture_control: Arc<Mutex<CaptureControl>>,
    pub game_tracker: Arc<Mutex<GameTracker>>,
    pub analysis_cache: AnalysisCache,
    pub history_platform: SharedPlatform,
    pub signals: Arc<SessionSignals>,

    pub autoplay_context: Arc<AutoplayContext>,
    pub bot_manager_started: Arc<AtomicBool>,
    pub autoplay_manager_started: Arc<AtomicBool>,

    pub pool_cancel: Arc<AtomicBool>,
    pub pool_stop_after_game: Arc<AtomicBool>,
    /// Set by `pool_stop`, cleared by `pool_start`. The pool driver skips a
    /// paused session so "stop" keeps meaning stopped until you start again,
    /// without writing anything to disk.
    pub paused: Arc<AtomicBool>,
    /// Set at startup when `try_restore` finds a fresh, unfinished match for
    /// this session. Even if the global `start_capture_on_launch` is off, a
    /// restored live match needs its browser/CDP reattached or the bot can't
    /// actually play — so the launch code opens capture for these sessions.
    pub restore_pending_capture: Arc<AtomicBool>,
}

impl SessionRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: SessionId,
        config: Arc<RwLock<AppConfig>>,
        config_path: Arc<PathBuf>,
        log_session: Arc<Session>,
        history_store: Arc<HistoryStore>,
        buses: SessionBuses,
        game_tracker: Arc<Mutex<GameTracker>>,
        analysis_cache: AnalysisCache,
        history_platform: SharedPlatform,
    ) -> Self {
        Self {
            id,
            config,
            config_path,
            log_session,
            history_store,
            buses,
            bot_status: Arc::new(RwLock::new(BotStatus::Idle)),
            capture_control: Arc::new(Mutex::new(CaptureControl::default())),
            game_tracker,
            analysis_cache,
            history_platform,
            signals: Arc::new(SessionSignals::default()),
            autoplay_context: Arc::new(AutoplayContext::new()),
            bot_manager_started: Arc::new(AtomicBool::new(false)),
            autoplay_manager_started: Arc::new(AtomicBool::new(false)),
            pool_cancel: Arc::new(AtomicBool::new(false)),
            pool_stop_after_game: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            restore_pending_capture: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Short label for logs/status. P2 will populate this from the slot's
    /// account label; the id is enough for P1.
    pub fn label(&self) -> &str {
        &self.id
    }
}

/// Registry of live sessions.
///
/// P1 constructs exactly one session. The API is already async because P2
/// will create/remove slots at runtime from IPC commands while tasks hold
/// `Arc<SessionRuntime>` clones.
pub struct SessionManager {
    sessions: RwLock<HashMap<SessionId, Arc<SessionRuntime>>>,
    active: RwLock<SessionId>,
}

impl SessionManager {
    pub fn new(initial: Arc<SessionRuntime>) -> Self {
        let id = initial.id.clone();
        let mut sessions = HashMap::new();
        sessions.insert(id.clone(), initial);
        Self {
            sessions: RwLock::new(sessions),
            active: RwLock::new(id),
        }
    }

    pub async fn active_id(&self) -> SessionId {
        self.active.read().await.clone()
    }

    pub async fn active(&self) -> Option<Arc<SessionRuntime>> {
        let id = self.active_id().await;
        self.get(&id).await
    }

    pub async fn get(&self, id: &str) -> Option<Arc<SessionRuntime>> {
        self.sessions.read().await.get(id).cloned()
    }

    pub async fn all(&self) -> Vec<Arc<SessionRuntime>> {
        self.sessions.read().await.values().cloned().collect()
    }

    pub async fn insert(&self, session: Arc<SessionRuntime>) {
        self.sessions
            .write()
            .await
            .insert(session.id.clone(), session);
    }

    pub async fn set_active(&self, id: &str) -> bool {
        if !self.sessions.read().await.contains_key(id) {
            return false;
        }
        *self.active.write().await = id.to_string();
        true
    }

    pub async fn remove(&self, id: &str) -> Option<Arc<SessionRuntime>> {
        self.sessions.write().await.remove(id)
    }
}
