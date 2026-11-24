pub mod analysis;
pub mod autoplay;
pub mod bot;
pub mod bridge;
pub mod capture;
pub mod cli;
pub mod config;
pub mod event_bus;
pub mod game_state;
pub mod github;
pub mod history;
pub mod inspector;
pub mod ipc;
pub mod logger;
pub mod platform;
pub mod pool;
pub mod proxy;
pub mod testing;
pub mod schema;
pub mod session;
pub mod updater;
pub mod util;

use clap::Parser;
use std::sync::Arc;
use tauri::Manager;
use tracing::{error, info, warn};

pub fn run() {
    platform::setup();

    let args = cli::Cli::parse();
    let (cfg, config_path) = config::load_config(args.config.as_deref());

    let log_dir = util::resolve_dir(&cfg.logging.dir);
    let targets = [
        logger::LogTarget::new("proxy", "aogara::proxy"),
        logger::LogTarget::new("bot", "aogara::bot"),
    ];
    let session = match logger::init(
        &log_dir,
        &cfg.logging.level,
        &cfg.logging.all_level,
        &targets,
        &cfg.logging.inspector,
    ) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            // Read-only fs fallback: retry under user data dir.
            if let Some(fallback) =
                util::user_config_root().map(|r| r.join(util::strip_leading_dot(&cfg.logging.dir)))
            {
                if fallback != log_dir {
                    eprintln!(
                        "Logger init at {} failed: {e:?}. Retrying at {}",
                        log_dir.display(),
                        fallback.display()
                    );
                    match logger::init(
                        &fallback,
                        &cfg.logging.level,
                        &cfg.logging.all_level,
                        &targets,
                        &cfg.logging.inspector,
                    ) {
                        Ok(s) => Arc::new(s),
                        Err(e2) => {
                            eprintln!("Failed to initialise logger: {e2:?}");
                            return;
                        }
                    }
                } else {
                    eprintln!("Failed to initialise logger: {e:?}");
                    return;
                }
            } else {
                eprintln!("Failed to initialise logger: {e:?}");
                return;
            }
        }
    };

    info!("Config loaded: {cfg:?}");
    info!("Log session at {}", session.dir().display());

    // All buses constructed up front so AppState owns the canonical
    // sender clones — every subsystem (bot manager, proxy supervisor,
    // ipc forwarders, future HUD) shares these handles via state.
    let mjai_bus = event_bus::mjai_bus();
    let bot_response_bus = event_bus::bot_response_bus();
    let bot_status_bus = event_bus::bot_status_bus();
    let capture_status_bus = event_bus::capture_status_bus();
    let notify_bus = event_bus::notify_bus();
    let analysis_bus = event_bus::analysis_bus();
    let post_tracker_bus = event_bus::post_tracker_bus();
    let history_bus = event_bus::history_bus();

    // Persistent game-history store. Lives under `<config_root>/history/`
    // by default — independent of the volatile session log dir. Failure
    // to create the dir is fatal: the recorder task and IPC commands
    // both depend on it.
    let history_root = util::resolve_dir(&cfg.history.dir);
    let history_store = match history::HistoryStore::new(history_root.clone()) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            error!(
                "Failed to initialise history store at {}: {e:#}",
                history_root.display()
            );
            return;
        }
    };
    info!("History store at {}", history_root.display());

    let bot_enabled = cfg.bot.enabled;
    let proxy_enabled = cfg.proxy.enabled;
    // Off by default: Aogara starts without touching the game browser. The
    // switch lives on the pool page; a session's browser is opened on demand.
    let start_capture_on_launch = cfg.multi.start_capture_on_launch;
    let autoplay_enabled = cfg.autoplay.enabled;
    let overlay_cfg = cfg.overlay.clone();
    let multi_cfg = cfg.multi.clone();
    let test_cfg = cfg.test.clone();

    // Game-state tracker handle is built up front so AppState can carry
    // the Arc, but the consumer task is spawned inside `.setup()` once
    // the Tauri Tokio runtime is live (sync `lib::run` has no runtime).
    let game_tracker = game_state::tracker::new_handle();
    // Set when a fresh unfinished match is restored at startup. The launch
    // code reattaches this session's browser if set, even when the global
    // start_capture_on_launch switch is off.
    let restore_pending_capture: Arc<std::sync::atomic::AtomicBool> =
        Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Restore an in-progress match into the fresh tracker on startup. The
    // browser may still be running the same game from before this process
    // started (Aogara was restarted / crashed). Replaying the persisted mjai
    // log directly into the tracker (NOT via the bus, so autoplay can't fire
    // stale discards) rebuilds seat + hand state, letting autoplay take over
    // the live game as soon as the next CDP frame arrives — no page reload.
    // If the restored state has it sitting at a decision point for our seat,
    // stash the last event so setup can nudge the bot manager once it's live
    // (the bus has no subscribers at this point, so publishing now is lost).
    let restored_kick: Arc<tokio::sync::Mutex<Option<crate::schema::mjai::MjaiEvent>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    {
        // Freshly created tracker — nobody else holds the lock yet.
        if let Ok(mut t) = game_tracker.try_lock() {
            if let Some(info) = game_state::restore::try_restore(&log_dir, &mut t) {
                info!(
                    "Startup game restore: {} events from {}, seat {:?}, {}p (last {})",
                    info.events,
                    info.source.display(),
                    info.seat,
                    info.num_players,
                    info.last_event
                );
                // A live match was restored: reattach this session's browser
                // at launch even when start_capture_on_launch is off,
                // otherwise the bot has no CDP event stream to play on.
                restore_pending_capture.store(true, std::sync::atomic::Ordering::SeqCst);
                if t.our_seat_can_act() == Some(true) {
                    if let Some(last) = info.last_mjai {
                        info!("Startup game restore: restored at our turn — scheduling a bot nudge");
                        if let Ok(mut k) = restored_kick.try_lock() {
                            *k = Some(last);
                        }
                    }
                }
            }
        }
    }

    let analysis_cache = std::sync::Arc::new(tokio::sync::RwLock::new(None));

    let tracker_rx = mjai_bus.subscribe();
    let tracker_post = post_tracker_bus.clone();
    let analysis_rx = post_tracker_bus.subscribe();
    let analysis_tracker = game_tracker.clone();
    let analysis_bus_for_runner = analysis_bus.clone();
    let analysis_cache_for_runner = analysis_cache.clone();

    let history_rx = mjai_bus.subscribe();
    let history_store_for_recorder = history_store.clone();
    let history_bus_for_recorder = history_bus.clone();

    tauri::Builder::default()
        // Must be the first plugin: a second instance would run a full second
        // set of per-session tasks (bot managers, autoplay managers, pool
        // drivers) against the SAME game browsers — two automation stacks
        // clicking one page produce exactly the "clicked into the shop" kind
        // of chaos. Detect and bail; focus the existing window instead.
        .plugin(tauri_plugin_single_instance::init(|app_handle, _args, _cwd| {
            if let Some(w) = tauri::Manager::get_webview_window(app_handle, "main") {
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_window_state::Builder::default()
                // The overlay's geometry is still saved and restored, but not
                // by the plugin's automatic pass: that restores `StateFlags::all()`,
                // which would put decorations back on a deliberately frameless
                // window. `ipc::overlay::open` restores position+size itself.
                .skip_initial_state(ipc::overlay::LABEL)
                .build(),
        )
        .invoke_handler(crate::ipc_handlers!())
        .setup({
            // AppState constructed *inside* setup() so the python+uv
            // runtime can be located using `resource_dir` — bundled
            // binaries live under `<resource_dir>/runtime/...` and that
            // path is only resolvable once the AppHandle exists.
            move |app| {
                let resource_dir = app.path().resource_dir().ok();
                let runtime = bot::PythonRuntime::locate(resource_dir.as_deref()).ok();
                match &runtime {
                    Some(rt) => info!(
                        "bot runtime: python={} uv={} mode={:?}",
                        rt.python().display(),
                        rt.uv().display(),
                        rt.mode()
                    ),
                    None => warn!(
                        "no python3+uv runtime found (neither bundled nor on PATH); bot install/sync will be unavailable"
                    ),
                }

                let history_platform =
                    history::recorder::shared_platform(schema::Platform::from(cfg.platform.kind));

                let mut state = ipc::AppState::new(
                    cfg,
                    config_path,
                    session.clone(),
                    mjai_bus.clone(),
                    post_tracker_bus.clone(),
                    bot_response_bus.clone(),
                    bot_status_bus.clone(),
                    capture_status_bus.clone(),
                    notify_bus.clone(),
                    analysis_bus.clone(),
                    history_bus.clone(),
                    game_tracker,
                    analysis_cache,
                    history_store.clone(),
                    history_platform.clone(),
                    runtime.clone(),
                );
                // Point the state's restore flag at the Arc the startup
                // restore code set (slot1's SessionRuntime was built before
                // that detection, so its own flag is still false).
                state.restore_pending_capture = restore_pending_capture.clone();

                ipc::install(app.handle(), state.clone())?;

                // Extra sessions from `[multi].sessions`: each points at its
                // own full config file, so profile / port / logs / history /
                // pool selection are isolated without duplicating AppConfig.
                let disabled_slots = ipc::commands::load_disabled_slots(&state.config_path);
                // One slot = one runtime. The static list and the dynamic
                // registry can name the same slot (a slot the user re-added
                // from the UI); spawning both would give the slot two capture
                // handles and two pool drivers racing each other.
                let mut launched: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                launched.insert(crate::session::DEFAULT_SESSION_ID.to_string());
                for entry in multi_cfg
                    .sessions
                    .iter()
                    .filter(|s| s.enabled && !disabled_slots.contains(&s.slot))
                    .take(multi_cfg.max_sessions.saturating_sub(1))
                {
                    if !launched.insert(entry.slot.clone()) {
                        warn!("[session {}] duplicate entry in config, skipped", entry.slot);
                        continue;
                    }
                    spawn_extra_session(
                        app.handle().clone(),
                        state.clone(),
                        entry.slot.clone(),
                        std::path::PathBuf::from(&entry.config),
                    );
                }

                // Sessions the user added from the UI (not written back into
                // config.toml's `[[multi.sessions]]`).
                for dyn_entry in ipc::commands::load_dynamic_sessions(&state.config_path) {
                    if !launched.insert(dyn_entry.slot.clone()) {
                        continue;
                    }
                    spawn_extra_session(
                        app.handle().clone(),
                        state.clone(),
                        dyn_entry.slot.clone(),
                        std::path::PathBuf::from(&dyn_entry.config),
                    );
                }

                // Reopen the suggestion overlay if it was left enabled. Safe
                // before any game data exists — it renders its empty state and
                // fills in on the first `bot-response`.
                ipc::overlay::reconcile(app.handle(), &overlay_cfg);

                // The overlay is an accessory window, not a co-equal one. Tauri
                // exits when *all* windows close, and the overlay counts — so
                // without this, closing the main window leaves the overlay on
                // screen holding the process open. It is `skip_taskbar` and
                // undecorated, which is right while Aogara is running and hostile
                // the moment it isn't: no taskbar entry, no title bar, nothing
                // that leads back to the process still alive behind the card.
                //
                // Close it with the main window and let Tauri exit on its own
                // once no windows remain — no `exit(0)`, so the window-state save
                // and every other shutdown path still runs. This closes the
                // *window*; it deliberately does not touch `overlay.enabled`, so
                // the overlay comes back on the next launch. Only a deliberate ×
                // (or the toggle) turns the feature off.
                if let Some(main) = app.get_webview_window("main") {
                    let handle = app.handle().clone();
                    main.on_window_event(move |event| {
                        if matches!(event, tauri::WindowEvent::CloseRequested { .. }) {
                            if let Err(e) = ipc::overlay::close(&handle) {
                                warn!("overlay: could not close with the main window: {e}");
                            }
                        }
                    });
                }

                // Spawn tracker + analysis loops inside the Tauri Tokio
                // runtime — `lib::run` itself is sync.
                tauri::async_runtime::spawn(game_state::tracker::drive_loop(
                    state.game_tracker.clone(),
                    tracker_rx,
                    Some(tracker_post),
                ));
                tauri::async_runtime::spawn(analysis::runner::drive_loop(
                    analysis_rx,
                    analysis_tracker,
                    analysis_bus_for_runner,
                    analysis_cache_for_runner,
                ));

                // Pool/state signals for this session, independent of all.log
                // (sessions 2..N do not install a tracing subscriber).
                tauri::async_runtime::spawn(crate::session::run_signal_tracker(
                    state.mjai_bus.subscribe(),
                    state.session.signals.clone(),
                ));

                // History recorder. Subscribes to the shared MjaiBus and
                // finalises one GameRecord per `EndGame`. The platform tag
                // is taken from the active bridge selection at startup;
                // changing platform at runtime requires a relaunch for the
                // history records to pick up the new tag.
                tauri::async_runtime::spawn(history::recorder::drive_loop(
                    history_store_for_recorder,
                    history_bus_for_recorder,
                    history_platform.clone(),
                    history_rx,
                ));

                if bot_enabled {
                    let cfg_for_bot = state.config.clone();
                    let events_for_bot = post_tracker_bus.clone();
                    let resp = bot_response_bus.clone();
                    let bs = bot_status_bus.clone();
                    let nb = notify_bus.clone();
                    let inspector = state.log_session.inspector();
                    let rt = runtime.clone();
                    let syncs = state.syncs_in_flight.clone();
                    state
                        .bot_manager_started
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    tauri::async_runtime::spawn(async move {
                        if let Err(e) = bot::run_bot_manager(
                            cfg_for_bot,
                            events_for_bot,
                            resp,
                            bs,
                            nb,
                            inspector,
                            rt,
                            syncs,
                        )
                        .await
                        {
                            error!("Bot manager failed: {e:#}");
                        }
                    });
                }

                // An untouched delay.lua from an older version becomes the
                // current default now, not at the first paced decision of a
                // game played with autoplay on.
                if let Some(dir) = state.config_path.parent() {
                    autoplay::delay::refresh_outdated_default(&dir.join("delay.lua"));
                }

                if autoplay_enabled {
                    let cfg_for_ap = state.config.clone();
                    let ctx_for_ap = state.autoplay_context.clone();
                    let tracker_for_ap = state.game_tracker.clone();
                    let mjai_for_ap = mjai_bus.clone();
                    let resp_for_ap = bot_response_bus.clone();
                    let notify_for_ap = notify_bus.clone();
                    // Default delay-script location is resolved relative
                    // to the loaded config file's directory.
                    let config_dir_for_ap = state
                        .config_path
                        .parent()
                        .map(std::path::Path::to_path_buf)
                        .unwrap_or_default();
                    state
                        .autoplay_manager_started
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    tauri::async_runtime::spawn(async move {
                        if let Err(e) = autoplay::run_autoplay_manager(
                            cfg_for_ap,
                            ctx_for_ap,
                            tracker_for_ap,
                            mjai_for_ap,
                            resp_for_ap,
                            notify_for_ap,
                            config_dir_for_ap,
                        )
                        .await
                        {
                            error!("Autoplay manager failed: {e:#}");
                        }
                    });
                }

                // If startup restored a game that was sitting at OUR turn to
                // discard, the historical events never touched the buses (to
                // avoid stale bot actions). Now that bot manager + autoplay
                // are subscribed, deliver one synthetic TrackedEvent carrying
                // can_act=true so the bot re-evaluates the current snapshot
                // and plays. No tracker.handle() here — the restored state is
                // already current; we only nudge the decision path.
                {
                    let kick = restored_kick.clone();
                    let tracker = state.game_tracker.clone();
                    let post = state.post_tracker_bus.clone();
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                        let last = kick.lock().await.take();
                        let Some(last) = last else { return };
                        let can_act = tracker.lock().await.our_seat_can_act();
                        if can_act != Some(true) {
                            return; // a real event already arrived; don't double-fire
                        }
                        info!("Startup restore: nudging bot at restored decision point");
                        let _ = post.send(crate::event_bus::TrackedEvent {
                            event: last,
                            can_act: Some(true),
                        });
                    });
                }

                // Inspector mjai-bus subscriber: every event the bridges
                // emit is mirrored into the inspector timeline so the
                // user sees frames → mjai events → bot reactions in one
                // place. Spawned here (vs. inside Session::init) because
                // we need the Tauri Tokio runtime — Session is built sync.
                {
                    let mut rx = mjai_bus.subscribe();
                    let inspector = state.log_session.inspector();
                    tauri::async_runtime::spawn(async move {
                        loop {
                            match rx.recv().await {
                                Ok(event) => {
                                    inspector.record(crate::schema::InspectorEntry::MjaiEvent {
                                        ts_ms: chrono::Local::now().timestamp_millis(),
                                        event,
                                    });
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    // Lossy — the inspector file already
                                    // tells the durable story; the
                                    // real-time view tolerates gaps.
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                            }
                        }
                    });
                }

                // Auto-reattach the browser when either the user's launch
                // switch is on OR startup restored a live match — a restored
                // game with no CDP stream can't be played by the bot.
                if proxy_enabled
                    && (start_capture_on_launch
                        || state.restore_pending_capture.load(std::sync::atomic::Ordering::SeqCst))
                {
                    let state_for_capture = state.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Err(e) =
                            ipc::capture_supervisor::spawn_capture_supervisor(state_for_capture)
                                .await
                        {
                            error!("Capture supervisor failed: {e:#}");
                        }
                    });
                }

                // One global pool driver for all sessions: auto-next, timed
                // stop and idle browser release. Internal per-session state
                // is kept by slot; inert while the stress runner is active.
                ipc::commands::spawn_pool_driver(state.clone());

                // Test-mode stress harness. `TestController::start` itself
                // refuses unless `[test].enabled = true`; this block only
                // decides whether to start it automatically after launch.
                if test_cfg.enabled && test_cfg.auto_start {
                    let st = state.clone();
                    let app_handle = app.handle().clone();
                    tauri::async_runtime::spawn(async move {
                        // Let extra sessions register their runtime first.
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        if let Err(e) = st.testing.start(app_handle, st.clone()).await {
                            error!("[test] auto-start failed: {e:#}");
                        }
                    });
                }

                // Best-effort terminal ctrl_c → graceful capture shutdown.
                // GUI close goes through Tauri's window event path, not
                // here, so this only matters when the user runs Aogara
                // headless from a terminal.
                let stop_state = state.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    let stop = {
                        let mut ctl = stop_state.capture_control.lock().await;
                        ctl.stop.take()
                    };
                    if let Some(tx) = stop {
                        let _ = tx.send(());
                    }
                });

                Ok(())
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Build and start one extra session from its own full `config.toml`.
///
/// The main config remains session 1; entries in `[multi].sessions` point at
/// complete config files so `update_config` / pool persistence keep writing
/// to the session's own file instead of clobbering the others.
fn spawn_extra_session(
    _app: tauri::AppHandle,
    base: ipc::AppState,
    slot: String,
    config_path: std::path::PathBuf,
) {
    if !config_path.exists() {
        error!(
            "[session {slot}] config not found: {} — skipped",
            config_path.display()
        );
        return;
    }
    let (cfg, resolved_path) = config::load_config(Some(&config_path));
    info!(
        "[session {slot}] config {} profile={} port={:?}",
        resolved_path.display(),
        cfg.capture.chromium.user_data_dir,
        crate::util::parse_fixed_cdp_port(&cfg.capture.chromium.extra_args)
    );

    let log_dir = util::resolve_dir(&cfg.logging.dir);
    let log_session = match logger::Session::create(&log_dir, &cfg.logging.inspector) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            error!("[session {slot}] logger create failed at {}: {e:#}", log_dir.display());
            return;
        }
    };
    let history_root = util::resolve_dir(&cfg.history.dir);
    let history_store = match history::HistoryStore::new(history_root.clone()) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            error!("[session {slot}] history store failed at {}: {e:#}", history_root.display());
            return;
        }
    };

    let buses = crate::session::SessionBuses {
        mjai: event_bus::mjai_bus(),
        post_tracker: event_bus::post_tracker_bus(),
        bot_response: event_bus::bot_response_bus(),
        bot_status: event_bus::bot_status_bus(),
        capture_status: event_bus::capture_status_bus(),
        notify: event_bus::notify_bus(),
        analysis: event_bus::analysis_bus(),
        history: event_bus::history_bus(),
    };
    let tracker = game_state::tracker::new_handle();
    let analysis_cache = Arc::new(tokio::sync::RwLock::new(None));
    let history_platform =
        history::recorder::shared_platform(schema::Platform::from(cfg.platform.kind));
    let session = Arc::new(crate::session::SessionRuntime::new(
        slot.clone(),
        Arc::new(tokio::sync::RwLock::new(cfg)),
        Arc::new(resolved_path),
        log_session,
        history_store,
        buses,
        tracker,
        analysis_cache,
        history_platform,
    ));
    let state = base.with_session(session.clone());
    let manager = base.sessions.clone();

    tauri::async_runtime::spawn(async move {
        if manager.get(&session.id).await.is_some() {
            return; // static + dynamic entry for the same slot
        }
        manager.insert(session).await;
        spawn_session_tasks(state, _app).await;
    });
}

/// Spawn the same task set `setup()` builds for session 1, but against a
/// per-session `AppState` clone. No Tauri forwarders here: the UI polls
/// `list_sessions` / `get_session_status`, and `bot_status` is mirrored below.
pub(crate) async fn spawn_session_tasks(state: ipc::AppState, app: tauri::AppHandle) {
    let slot = state.session.id.clone();
    info!("[session {slot}] starting tasks");

    // Per-slot event forwarding: sessions 2..N emit `event:{slot}` so the
    // dashboard can show each account's match independently. slot1 keeps the
    // unsuffixed global events (wired in `ipc::install`).
    if slot != crate::session::DEFAULT_SESSION_ID {
        ipc::spawn_slot_forwarders(app, state.clone(), &slot);
    }

    let cfg = state.config.read().await.clone();
    let log_root = state.log_session.root().to_path_buf();

    // Restore an unfinished match from this session's own log tree.
    let restored_kick: Arc<tokio::sync::Mutex<Option<schema::MjaiEvent>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    if let Ok(mut t) = state.game_tracker.try_lock() {
            if let Some(info) = game_state::restore::try_restore(&log_root, &mut t) {
            info!(
                "[session {slot}] restored {} events from {} (seat {:?}, {}p)",
                info.events, info.source.display(), info.seat, info.num_players
            );
            // Reattach this session's browser at launch even if the global
            // start_capture_on_launch switch is off — the restored live match
            // needs its CDP stream back for autoplay to continue.
            state
                .restore_pending_capture
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if t.our_seat_can_act() == Some(true) {
                if let Some(last) = info.last_mjai {
                    *restored_kick.try_lock().expect("fresh kick lock") = Some(last);
                }
            }
        }
    }

    let tracker_rx = state.mjai_bus.subscribe();
    let tracker_post = state.post_tracker_bus.clone();
    tauri::async_runtime::spawn(game_state::tracker::drive_loop(
        state.game_tracker.clone(),
        tracker_rx,
        Some(tracker_post),
    ));
    tauri::async_runtime::spawn(analysis::runner::drive_loop(
        state.post_tracker_bus.subscribe(),
        state.game_tracker.clone(),
        state.analysis_bus.clone(),
        state.analysis_cache.clone(),
    ));
    tauri::async_runtime::spawn(history::recorder::drive_loop(
        state.history_store.clone(),
        state.history_bus.clone(),
        state.history_platform.clone(),
        state.mjai_bus.subscribe(),
    ));
    tauri::async_runtime::spawn(crate::session::run_signal_tracker(
        state.mjai_bus.subscribe(),
        state.session.signals.clone(),
    ));

    // Mirror bot status for the session panel (no frontend event emission).
    {
        let mut rx = state.bot_status_bus.subscribe();
        let slot_status = state.bot_status.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(s) => *slot_status.write().await = s,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    if cfg.bot.enabled {
        state
            .bot_manager_started
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let cfg_for_bot = state.config.clone();
        let events = state.post_tracker_bus.clone();
        let resp = state.bot_response_bus.clone();
        let status = state.bot_status_bus.clone();
        let notify = state.notify_bus.clone();
        let inspector = state.log_session.inspector();
        let runtime = state.runtime.clone();
        let syncs = state.syncs_in_flight.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = bot::run_bot_manager(
                cfg_for_bot, events, resp, status, notify, inspector, runtime, syncs,
            )
            .await
            {
                error!("[session] bot manager failed: {e:#}");
            }
        });
    }

    if cfg.autoplay.enabled {
        state
            .autoplay_manager_started
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let cfg_for_ap = state.config.clone();
        let ctx = state.autoplay_context.clone();
        let tracker = state.game_tracker.clone();
        let mjai = state.mjai_bus.clone();
        let resp = state.bot_response_bus.clone();
        let notify = state.notify_bus.clone();
        let config_dir = state
            .config_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = autoplay::run_autoplay_manager(
                cfg_for_ap, ctx, tracker, mjai, resp, notify, config_dir,
            )
            .await
            {
                error!("[session] autoplay manager failed: {e:#}");
            }
        });
    }

    // Inspector timeline for this session.
    {
        let mut rx = state.mjai_bus.subscribe();
        let inspector = state.log_session.inspector();
        tauri::async_runtime::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => inspector.record(schema::InspectorEntry::MjaiEvent {
                        ts_ms: chrono::Local::now().timestamp_millis(),
                        event,
                    }),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    // Nudge a restored decision point once bot + autoplay are subscribed.
    {
        let kick = restored_kick.clone();
        let tracker = state.game_tracker.clone();
        let post = state.post_tracker_bus.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            let last = kick.lock().await.take();
            let Some(last) = last else { return };
            if tracker.lock().await.our_seat_can_act() == Some(true) {
                let _ = post.send(crate::event_bus::TrackedEvent {
                    event: last,
                    can_act: Some(true),
                });
            }
        });
    }

    // Auto-reattach when the launch switch is on OR a live match was restored
    // for this session — otherwise the restored game has no event stream.
    if cfg.proxy.enabled
        && (cfg.multi.start_capture_on_launch
            || state
                .restore_pending_capture
                .load(std::sync::atomic::Ordering::SeqCst))
    {
        let state_for_capture = state.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) =
                ipc::capture_supervisor::spawn_capture_supervisor(state_for_capture).await
            {
                error!("[session] capture supervisor failed: {e:#}");
            }
        });
    }
}
