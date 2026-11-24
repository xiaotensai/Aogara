//! Shared state held by Tauri as `tauri::State<AppState>`.
//!
//! P1 of the multi-account refactor: the per-session handles now live in
//! [`crate::session::SessionRuntime`]. `AppState` keeps them as aliases for
//! the single default session so existing IPC commands and task wiring keep
//! working unchanged; later phases route through `AppState::sessions`.
//!
//! Snapshots vs streams: the `*_bus` channels are the canonical event
//! stream, but a frontend that opens a window mid-game needs a one-shot
//! "what's the current state?" answer. To serve that, the IPC forwarder
//! task mirrors every status event into the `bot_status` /
//! `capture_control.status` slots, and the `get_status` command reads
//! them back. Commands that *change* state (set_active_bot,
//! start/stop_capture) also write the snapshot synchronously so a
//! follow-up `get_status` is always consistent with the action that just
//! succeeded.

use crate::analysis::runner::AnalysisCache;
use crate::autoplay::AutoplayContext;
use crate::bot::PythonRuntime;
use crate::config::AppConfig;
use crate::event_bus::{
    AnalysisBus, BotResponseBus, BotStatusBus, CaptureStatusBus, HistoryBus, MjaiBus, NotifyBus,
    PostTrackerBus,
};
use crate::game_state::GameTracker;
use crate::history::recorder::SharedPlatform;
use crate::history::HistoryStore;
use crate::logger::Session;
use crate::schema::BotStatus;
use crate::session::{SessionBuses, SessionManager, SessionRuntime, DEFAULT_SESSION_ID};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

pub use crate::session::CaptureControl;

#[derive(Clone)]
pub struct AppState {
    /// Registry of live sessions. P1 holds exactly one (`default`); P2 will
    /// add/remove slots at runtime.
    pub sessions: Arc<SessionManager>,
    /// The session whose handles the legacy fields below alias.
    pub session: Arc<SessionRuntime>,

    pub config: Arc<RwLock<AppConfig>>,
    pub config_path: Arc<PathBuf>,
    pub log_session: Arc<Session>,

    pub mjai_bus: MjaiBus,
    /// Events re-emitted after the tracker applied them. The bot manager
    /// runs off this rather than `mjai_bus` — see `event_bus::TrackedEvent`.
    pub post_tracker_bus: PostTrackerBus,
    pub bot_response_bus: BotResponseBus,
    pub bot_status_bus: BotStatusBus,
    pub capture_status_bus: CaptureStatusBus,
    pub notify_bus: NotifyBus,
    pub analysis_bus: AnalysisBus,
    pub history_bus: HistoryBus,

    /// Latest BotStatus seen on the bus. Forwarder writes; commands read.
    pub bot_status: Arc<RwLock<BotStatus>>,
    pub capture_control: Arc<Mutex<CaptureControl>>,
    /// Live game-state mirror. Future IPC commands lock this and call
    /// `snapshot()` to expose hands/scores/dora to the frontend.
    pub game_tracker: Arc<Mutex<GameTracker>>,
    /// Latest analysis result, populated by the analysis runner. Read by
    /// the `get_analysis` Tauri command for one-shot queries.
    pub analysis_cache: AnalysisCache,
    /// Persistent game-history store. Written by the recorder task,
    /// read by `list_game_history` / `get_game_history_*` IPC commands.
    pub history_store: Arc<HistoryStore>,
    /// Shared cell holding the platform tag the history recorder stamps
    /// onto each finalised game. `update_config` updates this when the
    /// user switches bridges so subsequent records pick up the new tag
    /// without a relaunch.
    pub history_platform: SharedPlatform,

    /// Bundled-or-system Python + uv. `None` on dev boxes lacking both —
    /// install/sync commands surface a friendly error instead of panicking.
    /// The bot manager still starts: the built-in native bot needs no Python,
    /// and only a `mjai_bot/*` subprocess bot fails to spawn without a runtime.
    pub runtime: Option<PythonRuntime>,
    /// Names of bots whose `uv sync` is currently in flight. Both the
    /// `sync_bot_deps` command and `BotManager::spawn_runner` acquire-or-bail
    /// so two parallel syncs against the same venv can't trample each other.
    pub syncs_in_flight: Arc<Mutex<HashSet<String>>>,
    /// Set to `true` once a `BotManager` has been spawned for this process.
    /// Used by `update_config` to start the manager on a runtime
    /// false→true flip of `bot.enabled` (e.g. when the first-run wizard
    /// finishes) without ever spawning twice. There is no off-switch:
    /// once started, the manager runs for the lifetime of the process.
    pub bot_manager_started: Arc<AtomicBool>,
    /// Shared with the chromium capture backend (which writes the page
    /// handle on Majsoul WS open) and the `AutoplayManager` (which reads
    /// it to dispatch input). Always present; `enabled = false` in
    /// config simply suppresses the manager spawn — the context still
    /// exists so a runtime toggle works without re-plumbing.
    pub autoplay_context: Arc<AutoplayContext>,
    /// Set to `true` once an `AutoplayManager` has been spawned for this
    /// process. Used by `update_config` to start the manager on a
    /// runtime false→true flip of `autoplay.enabled` without re-spawning.
    pub autoplay_manager_started: Arc<AtomicBool>,
    /// Pool: set by `pool_stop`, cleared by `pool_start`. Long-running pool
    /// operations check it at step boundaries so "停止任务" really aborts a
    /// switch/enter sequence instead of only flipping UI state.
    pub pool_cancel: Arc<AtomicBool>,
    /// Pool: soft-stop requested while a match is in progress. The pool
    /// driver turns autoplay off once that match ends, so the current hand is
    /// never abandoned mid-game (which would AFK the player).
    pub pool_stop_after_game: Arc<AtomicBool>,
    /// 自动连打 switches once for the whole process, not per account: the pool
    /// page toggle writes the launch config only and every session's driver
    /// reads this shared flag. Cloned (shared) by `with_session`.
    pub auto_next_global: Arc<AtomicBool>,
    /// Alias of this session's restore flag: true when startup restored a
    /// fresh unfinished match, so launch must reattach the browser even if
    /// `start_capture_on_launch` is off.
    pub restore_pending_capture: Arc<AtomicBool>,
    /// Serialises in-app update operations. `check_for_update` and
    /// `apply_update` both `try_lock()` it so the user mashing buttons
    /// can't race two HTTP fetches or — worse — two binary swaps.
    pub updater_lock: Arc<Mutex<()>>,
    /// Anti-top-out: global account→slot ownership map. A UID (or email when
    /// UID is absent) may be actively started by only ONE session at a time.
    /// `session_pool_start` acquires before switching and releases on exit so
    /// two concurrent "one-click start all" calls can never log the same
    /// account in twice. Shared across every `with_session` clone (Arc).
    pub account_locks: Arc<Mutex<HashMap<String, String>>>,
    /// Result of the last successful `check_for_update`, and the ONLY
    /// input `apply_update` acts on. The webview never round-trips an
    /// `UpdateInfo` back to us — fields like `asset_url` and
    /// `meta_source` are security policy inputs, and a compromised
    /// frontend must not get to assert them.
    pub pending_update: Arc<RwLock<Option<crate::updater::UpdateInfo>>>,
    /// Inert unless `[test].enabled`; see `src/testing/`.
    pub testing: Arc<crate::testing::TestController>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: AppConfig,
        config_path: PathBuf,
        log_session: Arc<Session>,
        mjai_bus: MjaiBus,
        post_tracker_bus: PostTrackerBus,
        bot_response_bus: BotResponseBus,
        bot_status_bus: BotStatusBus,
        capture_status_bus: CaptureStatusBus,
        notify_bus: NotifyBus,
        analysis_bus: AnalysisBus,
        history_bus: HistoryBus,
        game_tracker: Arc<Mutex<GameTracker>>,
        analysis_cache: AnalysisCache,
        history_store: Arc<HistoryStore>,
        history_platform: SharedPlatform,
        runtime: Option<PythonRuntime>,
    ) -> Self {
        let auto_next_global = Arc::new(AtomicBool::new(config.pool.auto_next));
        let config = Arc::new(RwLock::new(config));
        let config_path = Arc::new(config_path);
        let buses = SessionBuses {
            mjai: mjai_bus,
            post_tracker: post_tracker_bus,
            bot_response: bot_response_bus,
            bot_status: bot_status_bus,
            capture_status: capture_status_bus,
            notify: notify_bus,
            analysis: analysis_bus,
            history: history_bus,
        };
        let session = Arc::new(SessionRuntime::new(
            DEFAULT_SESSION_ID.to_string(),
            config.clone(),
            config_path.clone(),
            log_session.clone(),
            history_store.clone(),
            buses,
            game_tracker.clone(),
            analysis_cache.clone(),
            history_platform.clone(),
        ));
        let sessions = Arc::new(SessionManager::new(session.clone()));

        Self {
            // Aliases for the implicit single session. They point at the same
            // Arc-backed handles as `session`, so existing commands keep
            // working; P2 replaces direct field access with `sessions`.
            config: session.config.clone(),
            config_path: session.config_path.clone(),
            log_session: session.log_session.clone(),
            mjai_bus: session.buses.mjai.clone(),
            post_tracker_bus: session.buses.post_tracker.clone(),
            bot_response_bus: session.buses.bot_response.clone(),
            bot_status_bus: session.buses.bot_status.clone(),
            capture_status_bus: session.buses.capture_status.clone(),
            notify_bus: session.buses.notify.clone(),
            analysis_bus: session.buses.analysis.clone(),
            history_bus: session.buses.history.clone(),
            bot_status: session.bot_status.clone(),
            capture_control: session.capture_control.clone(),
            game_tracker: session.game_tracker.clone(),
            analysis_cache: session.analysis_cache.clone(),
            history_store: session.history_store.clone(),
            history_platform: session.history_platform.clone(),
            runtime,
            syncs_in_flight: Arc::new(Mutex::new(HashSet::new())),
            bot_manager_started: session.bot_manager_started.clone(),
            autoplay_context: session.autoplay_context.clone(),
            autoplay_manager_started: session.autoplay_manager_started.clone(),
            pool_cancel: session.pool_cancel.clone(),
            pool_stop_after_game: session.pool_stop_after_game.clone(),
            auto_next_global,
            restore_pending_capture: session.restore_pending_capture.clone(),
            updater_lock: Arc::new(Mutex::new(())),
            account_locks: Arc::new(Mutex::new(HashMap::new())),
            pending_update: Arc::new(RwLock::new(None)),
            testing: Arc::new(crate::testing::TestController::new()),
            sessions,
            session,
        }
    }

    /// Clone the global handles and re-point every per-session field at
    /// `session`. Used to spawn tasks for sessions 2..N without a second
    /// Tauri-managed state object.
    pub fn with_session(&self, session: Arc<SessionRuntime>) -> Self {
        let mut out = self.clone();
        out.session = session.clone();
        out.config = session.config.clone();
        out.config_path = session.config_path.clone();
        out.log_session = session.log_session.clone();
        out.mjai_bus = session.buses.mjai.clone();
        out.post_tracker_bus = session.buses.post_tracker.clone();
        out.bot_response_bus = session.buses.bot_response.clone();
        out.bot_status_bus = session.buses.bot_status.clone();
        out.capture_status_bus = session.buses.capture_status.clone();
        out.notify_bus = session.buses.notify.clone();
        out.analysis_bus = session.buses.analysis.clone();
        out.history_bus = session.buses.history.clone();
        out.bot_status = session.bot_status.clone();
        out.capture_control = session.capture_control.clone();
        out.game_tracker = session.game_tracker.clone();
        out.analysis_cache = session.analysis_cache.clone();
        out.history_store = session.history_store.clone();
        out.history_platform = session.history_platform.clone();
        out.bot_manager_started = session.bot_manager_started.clone();
        out.autoplay_context = session.autoplay_context.clone();
        out.autoplay_manager_started = session.autoplay_manager_started.clone();
        out.pool_cancel = session.pool_cancel.clone();
        out.pool_stop_after_game = session.pool_stop_after_game.clone();
        out.restore_pending_capture = session.restore_pending_capture.clone();
        out
    }
}
