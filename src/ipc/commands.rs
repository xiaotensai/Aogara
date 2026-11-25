//! `#[tauri::command]` handlers exposed to the frontend.
//!
//! Errors are returned as `String` because Tauri serializes command
//! errors via `Display` and most call sites just want a human message in
//! a toast. Keep the JSON shape conservative — clients lock onto field
//! names quickly and renames break dashboards.

use crate::analysis::result::AnalysisResult;
use crate::bot::install::{self, GithubInstallSpec, LocalZipInstallSpec};
use crate::bot::manifest::{self, BotSource};
use crate::bot::runtime;
use crate::bot::sync_guard::SyncGuard;
use crate::bot::{BotEntry, BotRegistry};
use crate::config::{AppConfig, PoolAccount, PoolCoord};
use crate::game_state::mahgen_view::MahgenView;
use crate::game_state::snapshot::GameStateSnapshot;
use crate::ipc::capture_supervisor::{
    restart_capture as restart_capture_inner, spawn_capture_supervisor,
};
use crate::ipc::overlay;
use crate::ipc::state::AppState;
use crate::pool::{self as pool_mod, EnterResult, LoginState, SwitchResult};
use crate::schema::{
    BotInfo, BotSettings, CaptureStatus, GameRecord, HistoryEvent, HistoryEventLog, HistoryFilter,
    HoraScoreInfo, InspectorEntry, LogEntry, LogSessionInfo, Notification, ReadInspectorRequest,
    ReadInspectorResponse, ReadLogRequest, ReadLogResponse, Snapshot,
};
use crate::util::resolve_dir;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, State};

/// Returns `true` exactly once per process the first time `bot_enabled`
/// is observed as `true` here. Side-effect on success: flips `flag`
/// false→true via a CAS. Used by `update_config` to decide whether the
/// caller should spawn a fresh `BotManager`.
fn claim_bot_manager_spawn(bot_enabled: bool, flag: &std::sync::atomic::AtomicBool) -> bool {
    use std::sync::atomic::Ordering;
    bot_enabled
        && flag
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
}

/// Same once-per-process gating as the bot variant, but for the autoplay
/// manager. Toggling `autoplay.enabled` back to false leaves the manager
/// alive — it just becomes a no-op because every bot response is gated
/// on a fresh `cfg.autoplay.enabled` read.
fn claim_autoplay_manager_spawn(
    autoplay_enabled: bool,
    flag: &std::sync::atomic::AtomicBool,
) -> bool {
    use std::sync::atomic::Ordering;
    autoplay_enabled
        && flag
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
}

fn entry_to_info(e: &BotEntry) -> BotInfo {
    BotInfo {
        name: e.name.clone(),
        dir: e.dir.to_string_lossy().into_owned(),
        has_pyproject: e.pyproject.is_some(),
        env_ready: runtime::is_synced(&e.dir),
        manifest: e.manifest.clone(),
    }
}

type CmdResult<T> = Result<T, String>;

// ---------------------------------------------------------------------------
// Anti-top-out: account identity + global ownership lock.
// One account (identified by numeric UID, or email when UID is absent) must
// never be actively started by two sessions at once — that is exactly what
// produces 「您的帳號已在另一處登入」. These helpers back both the precheck
// command and the per-session start command.
// ---------------------------------------------------------------------------

/// Stable identity key for an account: `uid:<n>` when a numeric UID is set,
/// otherwise `mail:<email>`. Keeping the prefix avoids a UID colliding with
/// an email string.
pub(crate) fn account_identity_key(acc: &crate::config::PoolAccount) -> String {
    let uid = acc.uid.trim();
    if !uid.is_empty() {
        format!("uid:{}", uid.to_lowercase())
    } else {
        format!("mail:{}", acc.email.trim().to_lowercase())
    }
}

/// RAII guard for a session's account ownership. Dropped on any return path
/// (including `?` errors), so a failed start never leaves a stale lock behind.
struct AccountLockGuard {
    locks: std::sync::Arc<tokio::sync::Mutex<HashMap<String, String>>>,
    key: String,
    owner: String,
}

/// Remove `key` from the map only if it is still held by `owner`.
fn release_account_lock(m: &mut HashMap<String, String>, key: &str, owner: &str) {
    if m.get(key).map(|v| v.as_str()) == Some(owner) {
        m.remove(key);
    }
}

impl Drop for AccountLockGuard {
    fn drop(&mut self) {
        let locks = self.locks.clone();
        let key = self.key.clone();
        let owner = self.owner.clone();
        // Fast path: nobody holds the map, release synchronously.
        if let Ok(mut m) = locks.try_lock() {
            release_account_lock(&mut m, &key, &owner);
            return;
        }
        // Another task holds the map; defer cleanup to its own task with
        // fully-owned clones (no borrows of `self`).
        let _ = tauri::async_runtime::spawn(async move {
            let mut m = locks.lock().await;
            release_account_lock(&mut m, &key, &owner);
        });
    }
}

/// Try to acquire ownership of `key` for `owner`. Returns the guard on
/// success, or an error naming the current owner on conflict.
async fn acquire_account_lock(
    state: &AppState,
    key: String,
    owner: &str,
) -> Result<AccountLockGuard, String> {
    let mut locks = state.account_locks.lock().await;
    if let Some(held_by) = locks.get(&key) {
        if held_by != owner {
            return Err(format!(
                "账号正在被 {} 启动（同一账号不能同时登录两个会话，会顶号）",
                held_by
            ));
        }
    }
    locks.insert(key.clone(), owner.to_string());
    Ok(AccountLockGuard {
        locks: state.account_locks.clone(),
        key,
        owner: owner.to_string(),
    })
}

/// Result of the pre-launch integrity check for all enabled sessions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionPrecheck {
    pub ok: bool,
    /// identity key → list of slots that currently want it (duplicates only).
    pub conflicts: Vec<PrecheckConflict>,
    pub accounts: Vec<PrecheckAccount>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PrecheckConflict {
    pub identity: String,
    pub slots: Vec<String>,
    pub label: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PrecheckAccount {
    pub slot: String,
    pub account_id: String,
    pub label: String,
    pub identity: String,
}

/// Pre-launch check: build every enabled session's target account identity
/// and flag duplicates. Called by the frontend BEFORE a one-click batch start
/// (also by `session_pool_start` defensively). Pure config reads — no CDP, no
/// browser touch, so it is safe to call at any time.
#[tauri::command]
pub async fn precheck_sessions(state: State<'_, AppState>) -> CmdResult<SessionPrecheck> {
    let mut by_identity: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut accounts = Vec::new();

    // Every session reads its own config snapshot (slot1 = launch config).
    let all = state.sessions.all().await;
    for s in all {
        let cfg = s.config.read().await;
        let sel = cfg.pool.selected_id.trim();
        if sel.is_empty() {
            continue;
        }
        let acc = match cfg.pool.accounts.iter().find(|a| a.id == sel) {
            Some(a) => a.clone(),
            None => continue,
        };
        let identity = account_identity_key(&acc);
        by_identity
            .entry(identity.clone())
            .or_default()
            .push((s.id.clone(), acc.label.clone()));
        accounts.push(PrecheckAccount {
            slot: s.id.clone(),
            account_id: acc.id.clone(),
            label: acc.label.clone(),
            identity,
        });
    }

    let mut conflicts = Vec::new();
    for (identity, slots) in by_identity {
        if slots.len() > 1 {
            conflicts.push(PrecheckConflict {
                identity,
                slots: slots.iter().map(|(s, _)| s.clone()).collect(),
                label: slots
                    .first()
                    .map(|(_, l)| l.clone())
                    .unwrap_or_default(),
            });
        }
    }
    let ok = conflicts.is_empty();
    Ok(SessionPrecheck {
        ok,
        conflicts,
        accounts,
    })
}

/// 号池命令统一提示：号池依赖固定游戏调试端口，端口来自
/// `[capture.chromium].extra_args` 里的 `--remote-debugging-port`。
const POOL_CDP_PORT_REQUIRED: &str = "号池需要固定游戏调试端口：请在 config.toml 的 \
[capture.chromium].extra_args 加 --remote-debugging-port=63503";

/// 从配置取号池游戏 CDP 端口；未配置时返回统一错误。
fn pool_cdp_port(cfg: &AppConfig) -> CmdResult<u16> {
    pool_mod::game_cdp_port(&cfg.capture.chromium.extra_args)
        .ok_or_else(|| POOL_CDP_PORT_REQUIRED.to_string())
}

/// True once the user asked the pool to stop. Long-running commands check
/// this at step boundaries; `pool_start` clears it before a new run.
fn pool_cancelled(state: &AppState) -> bool {
    state
        .pool_cancel
        .load(std::sync::atomic::Ordering::SeqCst)
}

fn ensure_pool_active(state: &AppState) -> CmdResult<()> {
    if pool_cancelled(state) {
        Err("任务已取消".into())
    } else {
        Ok(())
    }
}

#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> CmdResult<AppConfig> {
    Ok(state.config.read().await.clone())
}

/// Replace the entire config and persist it to the same file the app
/// loaded from. Capture-related changes (mode, chromium settings, proxy
/// settings) trigger an automatic supervisor restart so the user doesn't
/// have to relaunch the app to switch capture modes. A `bot.enabled`
/// false→true flip (typically the first-run wizard finishing) hot-starts
/// the `BotManager` so the user doesn't have to relaunch either; once
/// started the manager runs for the lifetime of the process (toggling
/// `bot.enabled` back to false still requires a relaunch to actually
/// stop it).
#[tauri::command]
pub async fn update_config(
    new_config: AppConfig,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    update_config_inner(new_config, app, state.inner().clone()).await
}

async fn update_config_inner(
    new_config: AppConfig,
    app: AppHandle,
    state: AppState,
) -> CmdResult<()> {
    persist_config(&new_config, &state.config_path).map_err(|e| e.to_string())?;

    // Snapshot the *previous* capture-relevant fields before we overwrite,
    // so we can decide whether the supervisor needs a swap.
    let (prev_capture, prev_proxy, prev_platform) = {
        let cfg = state.config.read().await;
        (cfg.capture.clone(), cfg.proxy.clone(), cfg.platform.kind)
    };
    let capture_changed = prev_capture != new_config.capture;
    let proxy_changed = prev_proxy != new_config.proxy;
    let platform_changed = prev_platform != new_config.platform.kind;
    let new_platform = new_config.platform.kind;
    let bot_now_enabled = new_config.bot.enabled;
    let autoplay_now_enabled = new_config.autoplay.enabled;
    let new_overlay = new_config.overlay.clone();
    *state.config.write().await = new_config;

    // Open / close / retune the overlay window to match what was just saved.
    overlay::reconcile(&app, &new_overlay);

    // Sync the history recorder's platform tag immediately. Subsequent
    // finalised games are stamped with the new tag; the in-flight buffer
    // (if any) keeps the tag it had at start_game — acceptable, since
    // mid-game platform switches are not a real workflow.
    if platform_changed {
        *state
            .history_platform
            .write()
            .expect("history platform lock poisoned") = crate::schema::Platform::from(new_platform);
    }

    // Only restart a backend that is already running. Aogara no longer opens
    // the game browser on its own: if capture is idle, a config write (the
    // frontend persists defaults on first load) must not launch one.
    let capture_running = { state.capture_control.lock().await.stop.is_some() };

    if (capture_changed || proxy_changed || platform_changed) && capture_running {
        // Run the restart in the background — `update_config` returns
        // promptly so the UI doesn't hang on slow shutdowns.
        let st = state.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = restart_capture_inner(st).await {
                let _ = ();
                tracing::error!("auto-restart capture failed: {e:#}");
            }
        });
        let body = if platform_changed {
            "Applied platform / capture / proxy config changes."
        } else {
            "Applied capture / proxy config changes."
        };
        let _ = state
            .notify_bus
            .send(Notification::info("Capture restarted").body(body));
    } else {
        let _ = state.notify_bus.send(
            Notification::success("Config saved")
                .body("Restart affected subsystems for changes to take effect."),
        );
    }

    // Hot-start the bot manager when bot.enabled flips false→true.
    // `bot_manager_started` is process-wide, so repeat false→true→false→true
    // toggles only spawn once. The manager runs forever; flipping back to
    // false still requires an Aogara relaunch to actually stop it.
    if claim_bot_manager_spawn(bot_now_enabled, &state.bot_manager_started) {
        let cfg_for_bot = state.config.clone();
        let events = state.post_tracker_bus.clone();
        let resp = state.bot_response_bus.clone();
        let bs = state.bot_status_bus.clone();
        let nb = state.notify_bus.clone();
        let inspector = state.log_session.inspector();
        let rt = state.runtime.clone();
        let syncs = state.syncs_in_flight.clone();
        let started_flag = state.bot_manager_started.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) =
                crate::bot::run_bot_manager(cfg_for_bot, events, resp, bs, nb, inspector, rt, syncs)
                    .await
            {
                tracing::error!("Bot manager failed: {e:#}");
                // Setup failure: clear the flag so a follow-up
                // update_config (e.g. the user fixing the bot dir and
                // saving again) gets another shot at spawning.
                started_flag.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        });
    }

    if claim_autoplay_manager_spawn(autoplay_now_enabled, &state.autoplay_manager_started) {
        let cfg_for_ap = state.config.clone();
        let ctx_for_ap = state.autoplay_context.clone();
        let tracker_for_ap = state.game_tracker.clone();
        let mjai_for_ap = state.mjai_bus.clone();
        let resp_for_ap = state.bot_response_bus.clone();
        let notify_for_ap = state.notify_bus.clone();
        let config_dir_for_ap = state
            .config_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        let started_flag = state.autoplay_manager_started.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = crate::autoplay::run_autoplay_manager(
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
                tracing::error!("Autoplay manager failed: {e:#}");
                started_flag.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        });
    }
    Ok(())
}

/// Flip `overlay.enabled` and apply it, without going through the Settings
/// page's whole-config save.
///
/// The overlay's own close button is the reason this exists: closing the
/// window has to *stay* closed across restarts, and the overlay webview has no
/// business round-tripping (and re-persisting) an entire `AppConfig` it never
/// loaded.
#[tauri::command]
pub async fn set_overlay_enabled(
    enabled: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let cfg = {
        let mut cfg = state.config.write().await;
        cfg.overlay.enabled = enabled;
        cfg.clone()
    };
    persist_config(&cfg, &state.config_path).map_err(|e| e.to_string())?;
    overlay::reconcile(&app, &cfg.overlay);
    Ok(())
}

/// Synthetic `BotInfo` entries for the built-in native bots. They have no
/// directory, no `pyproject.toml`, and are always "ready" (weights are embedded
/// in the binary — nothing to install).
fn native_bot_infos() -> Vec<BotInfo> {
    [crate::bot::native::NATIVE_4P, crate::bot::native::NATIVE_3P]
        .into_iter()
        .map(|name| BotInfo {
            name: name.to_string(),
            dir: String::new(),
            has_pyproject: false,
            env_ready: true,
            manifest: None,
        })
        .collect()
}

#[tauri::command]
pub async fn list_bots(state: State<'_, AppState>) -> CmdResult<Vec<BotInfo>> {
    let dir = state.config.read().await.bot.dir.clone();
    let resolved = resolve_dir(Path::new(&dir));
    let registry = BotRegistry::scan(&resolved).map_err(|e| format!("scan bots: {e:#}"))?;
    // Built-in native bots first, then discovered `mjai_bot/*` bots.
    let mut bots = native_bot_infos();
    bots.extend(registry.entries().iter().map(entry_to_info));
    Ok(bots)
}

/// Read the merged settings (manifest + on-disk values) for one bot.
/// Returns an error when the bot does not exist or has no manifest —
/// frontend should hide the settings panel for manifest-less bots and
/// avoid calling this command for them.
#[tauri::command]
pub async fn get_bot_settings(name: String, state: State<'_, AppState>) -> CmdResult<BotSettings> {
    let dir = state.config.read().await.bot.dir.clone();
    let resolved = resolve_dir(Path::new(&dir));
    let registry = BotRegistry::scan(&resolved).map_err(|e| format!("scan bots: {e:#}"))?;
    let entry = registry
        .find(&name)
        .ok_or_else(|| format!("bot {name:?} not found"))?;
    let manifest = entry
        .manifest
        .clone()
        .ok_or_else(|| format!("bot {name:?} has no manifest.toml"))?;
    let values = manifest::load_values(&entry.dir, &manifest)
        .map_err(|e| format!("load settings: {e:#}"))?;
    Ok(BotSettings { manifest, values })
}

/// Persist user-edited settings for one bot. Validates the values against
/// the manifest before writing — wrong type, out-of-range numeric, and
/// unknown enum choice all surface as command errors.
///
/// New values take effect on the next bot spawn (i.e. the next
/// `start_game` event). The currently-running subprocess keeps its old
/// values; document this caveat in the UI.
#[tauri::command]
pub async fn update_bot_settings(
    name: String,
    values: BTreeMap<String, serde_json::Value>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let dir = state.config.read().await.bot.dir.clone();
    let resolved = resolve_dir(Path::new(&dir));
    let registry = BotRegistry::scan(&resolved).map_err(|e| format!("scan bots: {e:#}"))?;
    let entry = registry
        .find(&name)
        .ok_or_else(|| format!("bot {name:?} not found"))?;
    let manifest = entry
        .manifest
        .as_ref()
        .ok_or_else(|| format!("bot {name:?} has no manifest.toml"))?;
    manifest::save_values(&entry.dir, manifest, &values)
        .map_err(|e| format!("save settings: {e:#}"))?;
    let _ = state
        .notify_bus
        .send(Notification::success(format!("{name} settings saved")));
    Ok(())
}

/// Update the active bot for a given mode (`"4p"` or `"3p"`) in config +
/// persist. Doesn't restart the running `BotManager` — it reads the active
/// bot fresh from the shared config at each `start_game`, so the next game
/// picks up this change with no relaunch (an in-progress game keeps its bot).
///
/// Empty `name` clears that mode's active bot (analysis-only in that mode).
///
/// Refuses to activate a bot whose Python environment isn't installed yet:
/// otherwise the bot's first in-game spawn would run `uv sync`, which can
/// exceed the react time limit and error. Clearing (empty `name`) is always
/// allowed.
#[tauri::command]
pub async fn set_active_bot(
    mode: String,
    name: String,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    // Built-in native bots are always available (no venv); skip the registry
    // + environment checks that only apply to Python `mjai_bot/*` bots.
    if !name.is_empty() && !crate::bot::native::is_native(&name) {
        let dir = state.config.read().await.bot.dir.clone();
        let resolved = resolve_dir(Path::new(&dir));
        let registry = BotRegistry::scan(&resolved).map_err(|e| format!("scan bots: {e:#}"))?;
        let entry = registry
            .find(&name)
            .ok_or_else(|| format!("bot {name:?} not found"))?;
        if !runtime::is_synced(&entry.dir) {
            return Err(format!(
                "Bot {name:?}'s Python environment isn't installed yet — install or sync it before setting it active."
            ));
        }
    }
    {
        let mut cfg = state.config.write().await;
        match mode.as_str() {
            "4p" => cfg.bot.active_4p = name.clone(),
            "3p" => cfg.bot.active_3p = name.clone(),
            other => return Err(format!("unknown mode {other:?}; expected \"4p\" or \"3p\"")),
        }
        persist_config(&cfg, &state.config_path).map_err(|e| e.to_string())?;
    }
    let label = if name.is_empty() {
        format!("{mode} bot cleared")
    } else {
        format!("Active {mode} bot set to {name}")
    };
    let _ = state.notify_bus.send(Notification::success(label));
    Ok(())
}

/// Install a bot by downloading the latest release zip from a GitHub
/// repository. Refuses to overwrite an existing `mjai_bot/<name>/` —
/// the user must remove it first via the file browser. The installer
/// reports progress through `NotifyBus` with sticky id
/// `bot-install-<name>`.
#[tauri::command]
pub async fn install_bot_from_github(
    repo: String,
    asset_glob: Option<String>,
    name: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<BotInfo> {
    let (dir, net) = {
        let cfg = state.config.read().await;
        (cfg.bot.dir.clone(), cfg.network.clone())
    };
    let resolved = resolve_dir(Path::new(&dir));
    std::fs::create_dir_all(&resolved)
        .map_err(|e| format!("create bot dir {}: {e}", resolved.display()))?;

    let spec = GithubInstallSpec {
        repo,
        asset_glob,
        name,
    };
    let entry = install::install_from_github_release(
        spec,
        &resolved,
        &state.notify_bus,
        state.runtime.as_ref(),
        &net,
    )
    .await
    .map_err(|e| format!("install: {e:#}"))?;
    Ok(entry_to_info(&entry))
}

/// Install a bot from a local `.zip` file the user picked (typically via the
/// native file dialog). Runs the same pipeline as
/// [`install_bot_from_github`] minus the release download. Refuses to
/// overwrite an existing `mjai_bot/<name>/`; the source zip is never deleted.
/// Reports progress through `NotifyBus` with sticky id `bot-install-<name>`.
#[tauri::command]
pub async fn install_bot_from_zip(
    zip_path: String,
    name: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<BotInfo> {
    let p = Path::new(&zip_path);
    if !p.is_file() {
        return Err(format!("{zip_path} is not a file"));
    }
    if !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) {
        return Err(format!("{zip_path} is not a .zip file"));
    }

    let dir = state.config.read().await.bot.dir.clone();
    let resolved = resolve_dir(Path::new(&dir));
    std::fs::create_dir_all(&resolved)
        .map_err(|e| format!("create bot dir {}: {e}", resolved.display()))?;

    let spec = LocalZipInstallSpec {
        zip_path: p.to_path_buf(),
        name,
    };
    let entry =
        install::install_from_zip(spec, &resolved, &state.notify_bus, state.runtime.as_ref())
            .await
            .map_err(|e| format!("install: {e:#}"))?;
    Ok(entry_to_info(&entry))
}

/// Reinstall a bot from the GitHub source declared in its existing
/// `manifest.toml`. Removes the current install first.
#[tauri::command]
pub async fn update_bot_from_manifest(
    name: String,
    state: State<'_, AppState>,
) -> CmdResult<BotInfo> {
    let (dir, net) = {
        let cfg = state.config.read().await;
        (cfg.bot.dir.clone(), cfg.network.clone())
    };
    let resolved = resolve_dir(Path::new(&dir));
    let registry = BotRegistry::scan(&resolved).map_err(|e| format!("scan bots: {e:#}"))?;
    let entry = registry
        .find(&name)
        .ok_or_else(|| format!("bot {name:?} not found"))?;
    let manifest = entry
        .manifest
        .as_ref()
        .ok_or_else(|| format!("bot {name:?} has no manifest.toml"))?;
    let source = manifest
        .source
        .as_ref()
        .ok_or_else(|| format!("bot {name:?} manifest has no [bot.source] block"))?;

    let (repo, asset_glob) = match source {
        BotSource::GithubRelease { repo, asset_glob } => (repo.clone(), asset_glob.clone()),
    };

    std::fs::remove_dir_all(&entry.dir)
        .map_err(|e| format!("remove old install {}: {e}", entry.dir.display()))?;

    let spec = GithubInstallSpec {
        repo,
        asset_glob,
        name: Some(name.clone()),
    };
    let new_entry = install::install_from_github_release(
        spec,
        &resolved,
        &state.notify_bus,
        state.runtime.as_ref(),
        &net,
    )
    .await
    .map_err(|e| format!("install: {e:#}"))?;
    Ok(entry_to_info(&new_entry))
}

/// Re-run `uv sync` for an installed bot. Frontend wires this to the
/// "Reinstall environment" button under Configure. `force=true` wipes
/// `.aogara/synced.stamp` and `.aogara/venv` first so a corrupted venv is
/// rebuilt from scratch (incremental sync can otherwise mask the breakage).
/// Reports progress + outcome through `NotifyBus` with sticky id
/// `bot-sync-<name>`. Refuses to start a second concurrent sync for the
/// same bot.
#[tauri::command]
pub async fn sync_bot_deps(name: String, force: bool, state: State<'_, AppState>) -> CmdResult<()> {
    let dir = state.config.read().await.bot.dir.clone();
    let resolved = resolve_dir(Path::new(&dir));
    let registry = BotRegistry::scan(&resolved).map_err(|e| format!("scan bots: {e:#}"))?;
    let entry = registry
        .find(&name)
        .ok_or_else(|| format!("bot {name:?} not found"))?
        .clone();
    let runtime = state.runtime.as_ref().ok_or_else(|| {
        "Python runtime not available — install python3 and uv on PATH".to_string()
    })?;

    let _guard = SyncGuard::acquire(&state.syncs_in_flight, &name)
        .await
        .ok_or_else(|| format!("sync already in progress for {name}"))?;

    let notify_id = format!("bot-sync-{name}");
    let _ = state.notify_bus.send(
        Notification::info(format!("Syncing {name}"))
            .body("Rebuilding Python environment (uv sync)…")
            .sticky()
            .id(notify_id.clone()),
    );

    if force {
        runtime::reset_sync_state(&entry.dir).await;
    }

    match runtime.ensure_synced(&entry.dir).await {
        Ok(()) => {
            let _ = state
                .notify_bus
                .send(Notification::success(format!("{name} environment ready")).id(notify_id));
            Ok(())
        }
        Err(e) => {
            let msg = format!("uv sync failed: {e:#}");
            let _ = state.notify_bus.send(
                Notification::error(format!("Sync failed for {name}"))
                    .body(msg.clone())
                    .id(notify_id),
            );
            Err(msg)
        }
    }
}

/// Start the capture backend selected by `cfg.capture.mode`. No-op
/// (returns Err) when one is already running — call `restart_capture`
/// instead if you want to swap.
#[tauri::command]
pub async fn start_capture(state: State<'_, AppState>) -> CmdResult<()> {
    let already_running = {
        let ctl = state.capture_control.lock().await;
        ctl.stop.is_some()
    };
    if already_running {
        return Err("capture backend already running".into());
    }
    spawn_capture_supervisor((*state).clone())
        .await
        .map_err(|e| format!("start capture: {e:#}"))
}

/// Tear down the running backend and start a fresh one. Used by the
/// Settings "Restart capture" button and by `update_config` whenever a
/// capture-affecting field changed. Safe to call when nothing is
/// running (becomes a plain start).
#[tauri::command]
pub async fn restart_capture(state: State<'_, AppState>) -> CmdResult<()> {
    restart_capture_inner((*state).clone())
        .await
        .map_err(|e| format!("restart capture: {e:#}"))
}

/// Probe the system for installed Chromium-family browsers. Surface in the
/// Settings UI so the user can pick which executable to launch.
#[tauri::command]
pub async fn detect_system_chrome(
) -> CmdResult<Vec<crate::capture::chromium::detect::DetectedBrowser>> {
    Ok(crate::capture::chromium::detect::detect_system_browsers())
}

/// Probe the account currently logged into the game browser (native pool).
///
/// Connects to the game Chromium on its fixed CDP port (from
/// `capture.chromium.extra_args`), finds the Majsoul page, and reads
/// `localStorage.account`. No external process; no dependency on autoplay's
/// page handle (which is only bound once a game WS appears).
#[tauri::command]
pub async fn pool_probe_login(state: State<'_, AppState>) -> CmdResult<LoginState> {
    let cfg = state.config.read().await;
    let port = pool_cdp_port(&cfg)?;
    drop(cfg);
    let cdp = pool_mod::GameCdp::connect(port)
        .await
        .map_err(|e| format!("连接游戏浏览器失败: {e:#}"))?;
    pool_mod::probe_login(&cdp.page)
        .await
        .map_err(|e| format!("读取登录态失败: {e:#}"))
}

/// Switch the game browser to a pool account (logout → login if needed).
/// Refuses mid-game (checked from Aogara's own game tracker).
#[tauri::command]
pub async fn pool_switch_account(
    account_id: String,
    state: State<'_, AppState>,
) -> CmdResult<SwitchResult> {
    let cfg = state.config.read().await;
    let port = pool_cdp_port(&cfg)?;
    let acc = cfg
        .pool
        .accounts
        .iter()
        .find(|a| a.id == account_id)
        .cloned()
        .ok_or_else(|| format!("号池里没有账号 {account_id}"))?;
    // Accounts without their own coords inherit main's (same Unity UI layout).
    let acc = pool_mod::session::resolve_account(&cfg.pool, &acc);
    drop(cfg);
    let in_game = {
        let t = state.game_tracker.lock().await;
        t.state().is_some() || t.state_3p().is_some()
    };
    let cdp = pool_mod::GameCdp::connect(port)
        .await
        .map_err(|e| format!("连接游戏浏览器失败: {e:#}"))?;
    pool_mod::switch_account(&cdp.page, &acc, in_game, &state.pool_cancel)
        .await
        .map_err(|e| format!("换号失败: {e:#}"))
}

/// Log-derived game/pool status (session, in-game, finish count) plus the
/// live autoplay flag. Replaces the bridge's `/state` for the native page.
#[tauri::command]
pub async fn pool_status(state: State<'_, AppState>) -> CmdResult<serde_json::Value> {
    let (autoplay, selected_id) = {
        let cfg = state.config.read().await;
        (cfg.autoplay.enabled, cfg.pool.selected_id.clone())
    };
    let auto_next = state
        .auto_next_global
        .load(std::sync::atomic::Ordering::SeqCst);
    let s = state.session.signals.snapshot();
    let stop_after_game = state
        .pool_stop_after_game
        .load(std::sync::atomic::Ordering::SeqCst);
    Ok(serde_json::json!({
        "session": state.session.id.clone(),
        "in_game": s.in_game,
        "last_enter": s.last_enter,
        "last_finish": s.last_finish,
        "finish_count": s.finish_count,
        "autoplay": autoplay,
        "auto_next": auto_next,
        "selected_id": selected_id,
        "stop_after_game": stop_after_game,
    }))
}

/// One session's status row for the multi-session panel.
async fn session_status_json(
    s: &std::sync::Arc<crate::session::SessionRuntime>,
    auto_next: bool,
) -> serde_json::Value {
    let (port, profile, selected_id, autoplay, target_room, target_mode, paused) = {
        let cfg = s.config.read().await;
        (
            pool_mod::game_cdp_port(&cfg.capture.chromium.extra_args),
            cfg.capture.chromium.user_data_dir.clone(),
            cfg.pool.selected_id.clone(),
            cfg.autoplay.enabled,
            cfg.pool.target.room.clone(),
            cfg.pool.target.mode.clone(),
            s.paused.load(std::sync::atomic::Ordering::SeqCst),
        )
    };
    let sig = s.signals.snapshot();
    let bot = s.bot_status.read().await.clone();
    let capture = s.capture_control.lock().await.status.clone();
    serde_json::json!({
        "slot": s.id,
        "selected_id": selected_id,
        "profile": profile,
        "cdp_port": port,
        "log_dir": s.log_session.dir().display().to_string(),
        "in_game": sig.in_game,
        "finish_count": sig.finish_count,
        "last_enter": sig.last_enter,
        "last_finish": sig.last_finish,
        "autoplay": autoplay,
        "auto_next": auto_next,
        "paused": paused,
        "target_room": target_room,
        "target_mode": target_mode,
        "bot_status": bot,
        "capture_status": capture,
    })
}

/// All live sessions, ordered by slot id.
#[tauri::command]
pub async fn list_sessions(state: State<'_, AppState>) -> CmdResult<Vec<serde_json::Value>> {
    let mut rows = Vec::new();
    let auto_next = state
        .auto_next_global
        .load(std::sync::atomic::Ordering::SeqCst);
    for s in state.sessions.all().await {
        rows.push(session_status_json(&s, auto_next).await);
    }
    rows.sort_by(|a, b| {
        a.get("slot")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .cmp(b.get("slot").and_then(|v| v.as_str()).unwrap_or(""))
    });
    Ok(rows)
}

#[tauri::command]
pub async fn get_session_status(
    slot: String,
    state: State<'_, AppState>,
) -> CmdResult<serde_json::Value> {
    let s = state
        .sessions
        .get(&slot)
        .await
        .ok_or_else(|| format!("没有会话 {slot}"))?;
    let auto_next = state
        .auto_next_global
        .load(std::sync::atomic::Ordering::SeqCst);
    Ok(session_status_json(&s, auto_next).await)
}

#[tauri::command]
pub async fn get_session_snapshot(
    slot: String,
    state: State<'_, AppState>,
) -> CmdResult<Option<GameStateSnapshot>> {
    let s = state
        .sessions
        .get(&slot)
        .await
        .ok_or_else(|| format!("没有会话 {slot}"))?;
    let snap = s.game_tracker.lock().await.snapshot();
    Ok(snap)
}

/// Start a match in a specific session without touching the active one.
#[tauri::command]
pub async fn session_pool_start(
    slot: String,
    account_id: String,
    room: String,
    mode: String,
    dry_run: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<EnterResult> {
    let s = state
        .sessions
        .get(&slot)
        .await
        .ok_or_else(|| format!("没有会话 {slot}"))?;
    let sstate = state.with_session(s);
    pool_start_inner(account_id, room, mode, dry_run, app, sstate).await
}

#[tauri::command]
pub async fn session_pool_stop(
    slot: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<serde_json::Value> {
    let s = state
        .sessions
        .get(&slot)
        .await
        .ok_or_else(|| format!("没有会话 {slot}"))?;
    let sstate = state.with_session(s);
    pool_stop_inner(app, sstate).await
}

// ---------------------------------------------------------------------------
// Dynamic sessions: add a pool account as a session (max 8) / remove one.
// The generated session config lives next to the main config
// (`slot-<account>.toml`); the registry `sessions.dynamic.json` is what lets
// them come back on the next launch without touching `config.toml`.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DynamicSession {
    pub slot: String,
    pub account_id: String,
    pub config: String,
}

fn dynamic_sessions_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("sessions.dynamic.json")
}

pub fn load_dynamic_sessions(config_path: &Path) -> Vec<DynamicSession> {
    std::fs::read_to_string(dynamic_sessions_path(config_path))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn disabled_slots_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("sessions.disabled.json")
}

/// Slots the user removed from the UI. Static `[[multi].sessions]]` entries
/// are skipped on the next launch so a removal sticks.
pub fn load_disabled_slots(config_path: &Path) -> Vec<String> {
    std::fs::read_to_string(disabled_slots_path(config_path))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_disabled_slots(config_path: &Path, list: &[String]) -> std::io::Result<()> {
    let p = disabled_slots_path(config_path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(list).unwrap_or_else(|_| b"[]".to_vec());
    std::fs::write(p, body)
}

fn save_dynamic_sessions(config_path: &Path, list: &[DynamicSession]) -> std::io::Result<()> {
    let p = dynamic_sessions_path(config_path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(list).unwrap_or_else(|_| b"[]".to_vec());
    std::fs::write(p, body)
}

/// The main session's config path (registry anchor). slot1 is the launch
/// config; fall back to the active session if it somehow isn't there.
async fn base_config_path(state: &AppState) -> PathBuf {
    match state.sessions.get("slot1").await {
        Some(s) => (*s.config_path).clone(),
        None => (*state.config_path).clone(),
    }
}

/// Start a session's capture backend if it isn't running and wait for its
/// CDP port to accept connections. Used by `pool_start` so a session opened
/// on demand works with no manual "open browser" step.
async fn ensure_capture_running(app: &AppHandle, state: AppState) -> CmdResult<()> {
    let port = {
        let cfg = state.config.read().await;
        pool_mod::game_cdp_port(&cfg.capture.chromium.extra_args)
    };
    let already = { state.capture_control.lock().await.stop.is_some() };
    if !already {
        spawn_capture_supervisor(state.clone())
            .await
            .map_err(|e| format!("启动浏览器失败: {e:#}"))?;
    }
    let Some(port) = port else { return Ok(()) };
    for _ in 0..180 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Err(format!("浏览器调试端口 {port} 未就绪"))
}

/// Build the default Majsoul canvas coordinates (normalised 0..1 relative to
/// the Unity canvas, so they work at any window size). These are UI geometry,
/// identical for every account — nothing account-specific here.
fn default_pool_coords() -> BTreeMap<String, PoolCoord> {
    let pt = |nx: f64, ny: f64| PoolCoord::Point([nx, ny]);
    let mut c = BTreeMap::new();
    for (k, x, y) in [
        ("again", 0.7671, 0.9245),
        ("back", 0.615, 0.25),
        ("cancel", 0.2528, 0.877),
        ("confirm", 0.9115, 0.9245),
        ("loginEmail", 0.7398, 0.2904),
        ("loginPassword", 0.7398, 0.4059),
        ("loginSubmit", 0.7407, 0.5891),
        ("logout", 0.252, 0.845),
        ("logoutConfirm", 0.41, 0.635),
        ("ranked", 0.7022, 0.311),
        ("settings", 0.964, 0.067),
    ] {
        c.insert(k.to_string(), pt(x, y));
    }
    let mut modes = BTreeMap::new();
    for (k, x, y) in [
        ("mode3e", 0.7264, 0.6949),
        ("mode3s", 0.7264, 0.82),
        ("mode4e", 0.7264, 0.3874),
        ("mode4s", 0.7264, 0.5415),
    ] {
        modes.insert(k.to_string(), [x, y]);
    }
    c.insert("modes".to_string(), PoolCoord::Table(modes));
    let mut rooms = BTreeMap::new();
    for (k, x, y) in [
        ("玉之间", 0.7235, 0.8358),
        ("王座之间", 0.7235, 0.989),
        ("金之间", 0.7235, 0.6823),
        ("铜之间", 0.7235, 0.3754),
        ("银之间", 0.7235, 0.5288),
    ] {
        rooms.insert(k.to_string(), [x, y]);
    }
    c.insert("rooms".to_string(), PoolCoord::Table(rooms));
    c
}

/// First-run wizard: add an account the user just typed in.
///
/// Unlike `create_session` (which promotes an *existing* pool account into a
/// new session), this command creates the pool account itself with the default
/// canvas coordinates, persists it into the main config, and — for the very
/// first account — selects it for slot1. Additional accounts are turned into
/// their own sessions by the frontend calling `create_session` afterwards.
///
/// The profile path is intentionally left empty on slot1 so `resolve_profile_dir`
/// falls back to the portable `./chrome-profile`; no machine-specific absolute
/// path is ever written into the shipped config.
/// Pure helper: append a wizard-entered account to a config, returning the
/// mutated config plus (id, first, count). Kept free of AppState/Tauri so the
/// account-creation rules can be unit-tested in isolation.
fn wizard_add_account(
    mut cfg: AppConfig,
    label: &str,
    email: &str,
    password: &str,
    uid: Option<String>,
    login_name: Option<String>,
) -> Result<(AppConfig, String, bool, usize), String> {
    let email = email.trim().to_lowercase();
    let label = label.trim();
    if email.is_empty() {
        return Err("邮箱不能为空".into());
    }
    if password.is_empty() {
        return Err("密码不能为空".into());
    }
    if cfg.pool.accounts.iter().any(|a| a.email.eq_ignore_ascii_case(&email)) {
        return Err(format!("账号 {email} 已存在，未重复添加"));
    }

    let mut n = 1;
    let id = loop {
        let candidate = format!("account-{n}");
        if !cfg.pool.accounts.iter().any(|a| a.id == candidate) {
            break candidate;
        }
        n += 1;
    };

    let display = if label.is_empty() {
        format!("账号{}", cfg.pool.accounts.len() + 1)
    } else {
        label.to_string()
    };
    let account = PoolAccount {
        id: id.clone(),
        label: display,
        uid: uid.unwrap_or_default(),
        email: email.clone(),
        password: password.to_string(),
        login_name,
        coords: default_pool_coords(),
    };

    let is_first = cfg.pool.accounts.is_empty();
    cfg.pool.accounts.push(account);
    if is_first || cfg.pool.selected_id.is_empty() {
        cfg.pool.selected_id = id.clone();
    }
    let count = cfg.pool.accounts.len();
    Ok((cfg, id, is_first, count))
}

#[tauri::command]
pub async fn setup_add_account(
    label: String,
    email: String,
    password: String,
    uid: Option<String>,
    login_name: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<serde_json::Value> {
    let base = match state.sessions.get("slot1").await {
        Some(s) => s,
        None => state.session.clone(),
    };
    let cfg = { base.config.read().await.clone() };
    let (cfg, id, is_first, count) =
        wizard_add_account(cfg, &label, &email, &password, uid, login_name)?;

    // Persist to the main config and refresh slot1's in-memory copy. Use the
    // session's own config path (slot1 == main config) so a re-run of setup
    // writes to the same file the loader will read next launch.
    persist_config(&cfg, &base.config_path).map_err(|e| format!("保存账号失败: {e}"))?;
    // state.config and slot1's config are the same Arc (AppState::new aliases
    // session.config), so one write refreshes both the session and Settings.
    *base.config.write().await = cfg;

    tracing::info!("[setup] added pool account {id} ({email}); first={is_first}");
    Ok(serde_json::json!({
        "id": id,
        "email": email.trim().to_lowercase(),
        "first": is_first,
        "count": count,
    }))
}

/// Add a pool account as a new session. Slots are `slot1`…`slot8`; `slot1`
/// is the launch session and cannot be created here.
#[tauri::command]
pub async fn create_session(
    account_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<serde_json::Value> {
    let base = match state.sessions.get("slot1").await {
        Some(s) => s,
        None => state.session.clone(),
    };
    let base_state = state.with_session(base.clone());
    let base_cfg = base_state.config.read().await.clone();
    let limit = base_cfg.multi.max_sessions.clamp(1, 8);

    let account = base_cfg
        .pool
        .accounts
        .iter()
        .find(|a| a.id == account_id)
        .cloned()
        .ok_or_else(|| format!("号池里没有账号 {account_id}"))?;

    let mut used_slots = std::collections::HashSet::new();
    let mut used_accounts = std::collections::HashSet::new();
    for s in state.sessions.all().await {
        used_slots.insert(s.id.clone());
        let sel = s.config.read().await.pool.selected_id.clone();
        if !sel.is_empty() {
            used_accounts.insert(sel);
        }
    }
    if used_accounts.contains(&account_id) {
        return Err(format!(
            "账号 {} 已被某个会话占用；先在那个会话里换号，再加入多开",
            account.label
        ));
    }
    let mut picked: Option<(String, usize)> = None;
    for i in 2..=limit {
        let name = format!("slot{i}");
        if !used_slots.contains(&name) {
            picked = Some((name, i));
            break;
        }
    }
    let (slot, idx) = picked.ok_or_else(|| format!("多开已满（上限 {limit} 个会话）"))?;

    let mut cfg = base_cfg.clone();
    cfg.pool.selected_id = account_id.clone();
    cfg.pool.auto_next = base_cfg.pool.auto_next;
    cfg.multi.sessions.clear();
    cfg.logging.dir = PathBuf::from(format!("./logs/{slot}"));
    cfg.history.dir = PathBuf::from(format!("./history/{slot}"));

    let profiles_root = Path::new(&base_cfg.capture.chromium.user_data_dir)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| resolve_dir(Path::new("./profiles")));
    cfg.capture.chromium.user_data_dir = profiles_root
        .join(format!("p-{account_id}"))
        .to_string_lossy()
        .replace('\\', "/");

    let port: u16 = 63502 + idx as u16;
    cfg.capture.chromium.extra_args = cfg
        .capture
        .chromium
        .extra_args
        .iter()
        .map(|a| {
            if a.starts_with("--remote-debugging-port=") {
                format!("--remote-debugging-port={port}")
            } else {
                a.clone()
            }
        })
        .collect();

    let cfg_dir = base_state
        .config_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let cfg_path = cfg_dir.join(format!("slot-{account_id}.toml"));
    persist_config(&cfg, &cfg_path).map_err(|e| format!("写会话配置失败: {e}"))?;

    let log_dir = resolve_dir(&cfg.logging.dir);
    let log_session = std::sync::Arc::new(
        crate::logger::Session::create(&log_dir, &cfg.logging.inspector)
            .map_err(|e| format!("建日志目录失败: {e:#}"))?,
    );
    let history_root = resolve_dir(&cfg.history.dir);
    let history_store = std::sync::Arc::new(
        crate::history::HistoryStore::new(history_root)
            .map_err(|e| format!("建历史库失败: {e:#}"))?,
    );
    // Fresh buses, never clones of `state`'s: `state` is session slot1, so
    // sharing its bus would route this session's CDP frames into slot1's
    // tracker — slot1 would then see "in_game" / "finished" events it never
    // played, and its auto-next driver would open a match of its own.
    let buses = crate::session::SessionBuses {
        mjai: crate::event_bus::mjai_bus(),
        post_tracker: crate::event_bus::post_tracker_bus(),
        bot_response: crate::event_bus::bot_response_bus(),
        bot_status: crate::event_bus::bot_status_bus(),
        capture_status: crate::event_bus::capture_status_bus(),
        notify: crate::event_bus::notify_bus(),
        analysis: crate::event_bus::analysis_bus(),
        history: crate::event_bus::history_bus(),
    };
    let tracker = crate::game_state::tracker::new_handle();
    let analysis_cache = std::sync::Arc::new(tokio::sync::RwLock::new(None));
    let platform = crate::history::recorder::shared_platform(crate::schema::Platform::from(
        cfg.platform.kind,
    ));
    let runtime = std::sync::Arc::new(crate::session::SessionRuntime::new(
        slot.clone(),
        std::sync::Arc::new(tokio::sync::RwLock::new(cfg)),
        std::sync::Arc::new(cfg_path.clone()),
        log_session,
        history_store,
        buses,
        tracker,
        analysis_cache,
        platform,
    ));
    state.sessions.insert(runtime.clone()).await;
    let sstate = state.with_session(runtime);
    crate::spawn_session_tasks(sstate.clone(), app).await;
    // Deliberately no capture start: adding a session only provisions it.
    // Opening the browser is a separate, explicit choice per session.

    let anchor = base_config_path(&state).await;
    {
        let mut disabled = load_disabled_slots(&anchor);
        disabled.retain(|s| s != &slot);
        let _ = save_disabled_slots(&anchor, &disabled);
    }
    let mut list = load_dynamic_sessions(&anchor);
    list.retain(|d| d.slot != slot);
    list.push(DynamicSession {
        slot: slot.clone(),
        account_id: account_id.clone(),
        config: cfg_path.to_string_lossy().to_string(),
    });
    let _ = save_dynamic_sessions(&anchor, &list);

    Ok(serde_json::json!({
        "slot": slot,
        "account_id": account_id,
        "profile": profiles_root.join(format!("p-{account_id}")).to_string_lossy().to_string(),
        "cdp_port": port,
    }))
}

#[tauri::command]
pub async fn remove_session(
    slot: String,
    state: State<'_, AppState>,
) -> CmdResult<serde_json::Value> {
    if slot == "slot1" {
        return Err("slot1 是主会话，不能移除".into());
    }
    let s = state
        .sessions
        .get(&slot)
        .await
        .ok_or_else(|| format!("没有会话 {slot}"))?;
    if s.signals.snapshot().in_game {
        return Err("该会话正在对局，先软停再移除".into());
    }
    {
        let mut ctl = s.capture_control.lock().await;
        if let Some(tx) = ctl.stop.take() {
            let _ = tx.send(());
        }
        ctl.force_close.notify_waiters();
    }
    state.sessions.remove(&slot).await;
    let anchor = base_config_path(&state).await;
    let mut list = load_dynamic_sessions(&anchor);
    list.retain(|d| d.slot != slot);
    let _ = save_dynamic_sessions(&anchor, &list);
    // Tombstone: a static `[[multi].sessions]]` entry would otherwise come
    // back on the next launch. `create_session` clears it again.
    let mut disabled = load_disabled_slots(&anchor);
    if !disabled.iter().any(|s| s == &slot) {
        disabled.push(slot.clone());
    }
    let _ = save_disabled_slots(&anchor, &disabled);
    Ok(serde_json::json!({ "removed": slot }))
}

/// Per-session settings from the 多开会话 page: which pool account this slot
/// plays, and which room/mode it targets.
///
/// The pool page only owns the account list; everything that drives a running
/// slot is edited here so one page is the single source for "who plays what".
/// The account table is copied from the launch config into the slot config so
/// accounts added to the pool after the slot was created still resolve.
#[tauri::command]
pub async fn update_session_settings(
    slot: String,
    account_id: Option<String>,
    room: Option<String>,
    mode: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<serde_json::Value> {
    let s = state
        .sessions
        .get(&slot)
        .await
        .ok_or_else(|| format!("没有会话 {slot}"))?;
    if s.signals.snapshot().in_game {
        return Err("该会话正在对局，先软停再改设置".into());
    }

    let mut cfg = s.config.read().await.clone();
    // Authoritative account table lives in the launch config.
    if let Some(main) = state.sessions.get("slot1").await {
        let accounts = main.config.read().await.pool.accounts.clone();
        if !accounts.is_empty() {
            cfg.pool.accounts = accounts;
        }
    }

    if let Some(aid) = account_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if !cfg.pool.accounts.iter().any(|a| a.id == aid) {
            return Err(format!("号池里没有账号 {aid}"));
        }
        for other in state.sessions.all().await {
            if other.id == slot {
                continue;
            }
            let sel = other.config.read().await.pool.selected_id.clone();
            if sel == aid {
                return Err(format!("账号 {aid} 已被会话 {} 占用", other.id));
            }
        }
        cfg.pool.selected_id = aid.to_string();
    }
    if let Some(r) = room.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        cfg.pool.target.room = r.to_string();
    }
    if let Some(m) = mode.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        cfg.pool.target.mode = m.to_string();
    }

    persist_config(&cfg, &s.config_path)
        .map_err(|e| format!("写 {} 失败: {e}", s.config_path.display()))?;
    *s.config.write().await = cfg;
    Ok(serde_json::json!({ "slot": slot, "ok": true }))
}

/// Open (start) the game browser for one session, then log it in.
///
/// A fresh session profile has no login state: opening the browser used to
/// leave the user stuck on Majsoul's login screen with nothing happening
/// until they pressed 开始对局. Now the session's own account is logged in
/// automatically (no-op if the profile is already on the right account).
#[tauri::command]
pub async fn session_start_capture(
    slot: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let s = state
        .sessions
        .get(&slot)
        .await
        .ok_or_else(|| format!("没有会话 {slot}"))?;
    ensure_capture_running(&app, state.with_session(s.clone())).await?;

    // Idle only: a session playing a match keeps its browser untouched.
    if s.signals.snapshot().in_game {
        return Ok(());
    }
    let launch_accounts = match state.sessions.get("slot1").await {
        Some(main) => main.config.read().await.pool.accounts.clone(),
        None => Vec::new(),
    };
    let (port, acc) = {
        let cfg = s.config.read().await;
        let sel = cfg.pool.selected_id.trim().to_string();
        if sel.is_empty() {
            return Ok(()); // no account chosen: just the browser
        }
        let port = pool_mod::game_cdp_port(&cfg.capture.chromium.extra_args)
            .ok_or_else(|| "该会话没有固定调试端口".to_string())?;
        let found = cfg
            .pool
            .accounts
            .iter()
            .find(|a| a.id == sel)
            .or_else(|| launch_accounts.iter().find(|a| a.id == sel))
            .cloned()
            .ok_or_else(|| format!("号池里没有账号 {sel}"))?;
        let acc = pool_mod::session::resolve_account(&cfg.pool, &found);
        (port, acc)
    };

    let cdp = pool_mod::GameCdp::connect(port)
        .await
        .map_err(|e| format!("连接游戏浏览器失败: {e:#}"))?;
    // A stale 客服 tab has no canvas — fix the URL before waiting for ready.
    pool_mod::ensure_game_url(&cdp.page, &s.pool_cancel)
        .await
        .map_err(|e| format!("返回游戏页失败: {e:#}"))?;
    pool_mod::wait_page_ready(&cdp.page, 60_000, &s.pool_cancel)
        .await
        .map_err(|e| format!("等待游戏页面加载失败: {e:#}"))?;
    tracing::info!("[session {slot}] auto-login as {} starting", acc.id);
    pool_mod::switch_account_retry(&cdp.page, &acc, false, &s.pool_cancel, 2)
        .await
        .map_err(|e| format!("自动登录失败: {e:#}"))?;
    tracing::info!("[session {slot}] auto-login done");
    Ok(())
}

/// Close (stop) the game browser for one session.
#[tauri::command]
/// Close (stop) the game browser for one session — hard exit.
///
/// Three stages: graceful capture stop (which asks the browser to close
/// itself via CDP `Browser.close`), wait for the supervisor to wind down,
/// then kill any browser process still holding this session's profile by
/// its command line. The command-line match is per `--user-data-dir`, so
/// other sessions' browsers are never touched.
pub async fn session_stop_capture(
    slot: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let s = state
        .sessions
        .get(&slot)
        .await
        .ok_or_else(|| format!("没有会话 {slot}"))?;
    if s.signals.snapshot().in_game {
        return Err("该会话正在对局，先软停再关浏览器".into());
    }
    // ① Graceful: ask the capture supervisor (and via it, CDP Browser.close)
    //    to shut down.
    {
        let mut ctl = s.capture_control.lock().await;
        if let Some(tx) = ctl.stop.take() {
            let _ = tx.send(());
        }
        ctl.force_close.notify_waiters();
    }
    // ② Wait for the supervisor to finish (it sets stop=None on exit and
    //    settles the status) so a later forced kill can't race it into
    //    reporting a spurious "browser exited unexpectedly" error.
    for _ in 0..10 {
        let running = { s.capture_control.lock().await.stop.is_some() };
        if !running {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    // ③ Hard exit: kill whatever browser process still holds this profile.
    //    Launcher handoff can leave the window alive without us owning a
    //    child handle; a profile-scoped command-line kill is exact and
    //    cannot touch other sessions.
    //
    //    aogara: capture the outcome instead of `?`/early-return. A failed
    //    reclaim must NOT skip the state write-back (④) or the bot/autoplay
    //    release below — a closed-or-dying browser can no longer receive
    //    decisions, so the bot slot must always be freed. Batch close
    //    (closeAll) issues several of these concurrently, which is exactly
    //    when reclaim_singleton is most likely to fail; that was the root
    //    cause of "batch close leaves autoplay/bot reserved" (P-04).
    let reclaim_result: Result<(), String> = {
        let profile = s.config.read().await.capture.chromium.user_data_dir.clone();
        let pd = resolve_dir(Path::new(&profile));
        match tokio::task::spawn_blocking(move || {
            crate::capture::chromium::profile::reclaim_singleton(&pd)
        })
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("强退浏览器失败: {e:#}")),
            Err(e) => Err(format!("强退浏览器任务失败: {e}")),
        }
    };
    // ④ Reflect the closed state immediately — the supervisor task may
    //    already be gone, so nothing else will write Stopped for us.
    //    Always runs, even if reclaim failed above.
    {
        let mut ctl = s.capture_control.lock().await;
        ctl.stop = None;
        ctl.status = crate::schema::CaptureStatus::Stopped;
    }
    let _ = s
        .buses
        .capture_status
        .send(crate::schema::CaptureStatus::Stopped);

    // ---- Release the bot/autoplay when the browser is closed ---------------
    // A closed browser can no longer receive decisions, so leaving
    // `autoplay.enabled=true` keeps the "停止对局" state and the bot slot
    // reserved (screenshot 2026-11-24: browser 未打开 but Bot still shown).
    // `session_stop_capture` already refuses in_game sessions above, so this
    // is always a safe non-match moment to switch autoplay off. This goes
    // through update_config_inner so the live flag, persisted config and the
    // autoplay manager all settle together.
    // NOTE: use the TARGET session's config/state, not the Tauri global state
    // (which is slot1). Otherwise we would read slot1's autoplay flag and
    // silently skip releasing slot3/4/5 — the exact bug seen in verification.
    let sstate = state.with_session(s.clone());
    let autoplay_on = s.config.read().await.autoplay.enabled;
    if autoplay_on {
        let new_cfg = {
            let cfg = s.config.read().await;
            let mut c = cfg.clone();
            c.autoplay.enabled = false;
            c
        };
        tracing::debug!(
            "[capture] releasing autoplay for {} via {:?}",
            s.id,
            sstate.config_path
        );
        if let Err(e) = update_config_inner(new_cfg, app, sstate).await {
            tracing::warn!("[capture] close: failed to release autoplay for {}: {e}", s.id);
        } else {
            tracing::info!(
                "[capture] browser closed for {}; autoplay and bot released",
                s.id
            );
        }
    }
    // Report a reclaim failure only AFTER the bot/autoplay has been freed,
    // so a hard-kill hiccup never leaves the bot slot reserved (P-04).
    reclaim_result?;
    Ok(())
}

/// Single process-wide switch: open the browser(s) when Aogara launches.
/// Persisted in the launch config's `[multi]` section only.
#[tauri::command]
pub async fn set_start_capture_on_launch(
    enabled: bool,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let anchor = base_config_path(&state).await;
    let Some(main) = state.sessions.get("slot1").await else {
        return Err("主会话不存在".into());
    };
    let mut cfg = main.config.read().await.clone();
    if cfg.multi.start_capture_on_launch == enabled {
        return Ok(());
    }
    cfg.multi.start_capture_on_launch = enabled;
    persist_config(&cfg, &anchor).map_err(|e| format!("写 {} 失败: {e}", anchor.display()))?;
    *main.config.write().await = cfg;
    Ok(())
}

/// Turn 自动连打 on/off for **every** live session.
///
/// The pool page used to write only the active session's config, so a slot
/// with `auto_next = true` in its own file kept opening new games after the
/// switch was turned off. This writes the flag to each session's config file
/// and updates the in-memory copy the pool driver reads.
#[tauri::command]
pub async fn set_pool_auto_next(
    enabled: bool,
    state: State<'_, AppState>,
) -> CmdResult<usize> {
    use std::sync::atomic::Ordering;
    state.auto_next_global.store(enabled, Ordering::SeqCst);

    // Exactly one file write, no matter how many accounts exist: the switch
    // lives in the launch config's `[pool]` section. Per-session configs keep
    // whatever they had; the drivers read the shared flag instead.
    let anchor = base_config_path(&state).await;
    let Some(main) = state.sessions.get("slot1").await else {
        return Ok(0);
    };
    let mut cfg = main.config.read().await.clone();
    if cfg.pool.auto_next != enabled {
        cfg.pool.auto_next = enabled;
        persist_config(&cfg, &anchor).map_err(|e| format!("写 {} 失败: {e}", anchor.display()))?;
        *main.config.write().await = cfg;
    }
    Ok(1)
}

/// Stop the pool.
///
/// Two-stage semantics:
/// - Always set the cancellation flag, so an in-flight `pool_start`
///   (switching / walking into a room) aborts at its next checkpoint.
/// - Disable auto-next immediately. If a match is running, keep autoplay ON
///   and ask the driver to turn it off when that match ends — stopping
///   autoplay mid-hand would AFK the player and lose the game.
#[tauri::command]
pub async fn pool_stop(
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<serde_json::Value> {
    pool_stop_inner(app, state.inner().clone()).await
}

async fn pool_stop_inner(
    app: AppHandle,
    state: AppState,
) -> CmdResult<serde_json::Value> {
    state
        .pool_cancel
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let (in_game, autoplay) = {
        let cfg = state.config.read().await;
        (state.session.signals.snapshot().in_game, cfg.autoplay.enabled)
    };

    // Stop means "this session stops opening games" — a runtime pause, not a
    // config edit. The global 自动连打 switch is a separate, single setting.
    state
        .session
        .paused
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let new_cfg = {
        let cfg = state.config.read().await;
        let mut c = cfg.clone();
        if !in_game {
            c.autoplay.enabled = false;
        }
        c
    };

    let stop_after_game = in_game && autoplay;
    state
        .pool_stop_after_game
        .store(stop_after_game, std::sync::atomic::Ordering::SeqCst);
    update_config_inner(new_cfg, app, state.clone()).await?;

    Ok(serde_json::json!({
        "stopped": true,
        "in_game": in_game,
        "stop_after_game": stop_after_game,
    }))
}

/// Turn autoplay on/off through the SAME path the Settings page uses, so the
/// autoplay manager is spawned hot (no Aogara restart). This is the native
/// replacement for the bridge's `setAutoplayLive` HTTP round-trip.
#[tauri::command]
pub async fn pool_set_autoplay(
    enabled: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let new_cfg = {
        let cfg = state.config.read().await;
        if cfg.autoplay.enabled == enabled {
            return Ok(()); // already in the requested state
        }
        let mut c = cfg.clone();
        c.autoplay.enabled = enabled;
        c
    };
    // update_config persists, swaps the live config and spawns the manager
    // when enabled flips false→true (claim_autoplay_manager_spawn).
    update_config(new_cfg, app, state).await
}

/// One-shot "开始对局": ensure autoplay is on, switch to the selected account
/// if needed, then walk ranked → room → mode and wait for the game to start.
/// This is the native replacement for the bridge's `/run` — no Node process.
#[tauri::command]
pub async fn pool_start(
    account_id: String,
    room: String,
    mode: String,
    dry_run: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<EnterResult> {
    pool_start_inner(account_id, room, mode, dry_run, app, state.inner().clone()).await
}

/// Enter the room with staged recovery:
///  ① plain `enter_room`;
///  ② on failure: `dismiss_popups` — clear the 活動 overlay WITHOUT reloading
///     (reload just shows the same overlay again) and retry;
///  ③ still failing: `ensure_lobby` (Escape + reload), dismiss the overlay
///     AGAIN, verify the login account, and retry one last time.
async fn enter_room_with_recovery(
    page: &chromiumoxide::Page,
    pool_cfg: &crate::config::PoolConfig,
    acc: &crate::config::PoolAccount,
    room: &str,
    mode: &str,
    signals: &crate::session::SessionSignals,
    dry_run: bool,
    cancel: &std::sync::atomic::AtomicBool,
) -> CmdResult<EnterResult> {
    // Bring the tab back to a clean lobby BEFORE the first entry attempt.
    // Without this, a tab sitting on the 客服 webchat / a "获取版本资讯失败"
    // dialog gets the fixed 段位场 clicks landed on the wrong UI (we saw
    // browsers end up on 客服 after 一键开始对局 on 2026-11-23).
    pool_mod::prepare_lobby(page, acc, cancel)
        .await
        .map_err(|e| format!("准备大厅失败: {e:#}"))?;

    let mut result = pool_mod::enter_room(page, pool_cfg, acc, room, mode, signals, dry_run, cancel)
        .await
        .map_err(|e| format!("进房失败: {e:#}"))?;
    if result.entered || dry_run || signals.snapshot().in_game {
        return Ok(result);
    }

    tracing::info!("[pool] enter failed; clearing overlays without reload");
    pool_mod::dismiss_popups(page, acc, cancel)
        .await
        .map_err(|e| format!("清除弹窗失败: {e:#}"))?;
    result = pool_mod::enter_room(page, pool_cfg, acc, room, mode, signals, dry_run, cancel)
        .await
        .map_err(|e| format!("进房失败（清弹窗后重试）: {e:#}"))?;
    if result.entered || signals.snapshot().in_game {
        return Ok(result);
    }

    tracing::info!("[pool] still not in game; lobby recovery (Escape + reload, then back-arrow via dismiss_popups)");
    pool_mod::ensure_lobby(page, cancel)
        .await
        .map_err(|e| format!("恢复大厅失败: {e:#}"))?;
    pool_mod::dismiss_popups(page, acc, cancel)
        .await
        .map_err(|e| format!("重载后清除弹窗失败: {e:#}"))?;
    let after = pool_mod::probe_login(page)
        .await
        .map_err(|e| format!("重载后读取登录态失败: {e:#}"))?;
    let want = acc.email.trim().to_lowercase();
    let account_ok = after
        .account
        .as_deref()
        .map(|cur| {
            cur == want
                || acc
                    .login_name
                    .as_deref()
                    .map(|n| cur == n.trim().to_lowercase())
                    .unwrap_or(false)
        })
        .unwrap_or(false);
    if !account_ok {
        return Err("页面重载后登录态丢失，请检查账号".into());
    }
    pool_mod::enter_room(page, pool_cfg, acc, room, mode, signals, dry_run, cancel)
        .await
        .map_err(|e| format!("进房失败（恢复后重试）: {e:#}"))
}

pub(crate) async fn pool_start_inner(
    account_id: String,
    room: String,
    mode: String,
    dry_run: bool,
    app: AppHandle,
    state: AppState,
) -> CmdResult<EnterResult> {
    // A new run supersedes any previous stop request.
    state
        .session
        .paused
        .store(false, std::sync::atomic::Ordering::SeqCst);
    state
        .pool_cancel
        .store(false, std::sync::atomic::Ordering::SeqCst);
    state
        .pool_stop_after_game
        .store(false, std::sync::atomic::Ordering::SeqCst);

    // Account table fallback: a slot config is a snapshot taken when the slot
    // was created, so accounts added to the pool afterwards are missing there.
    let launch_accounts = match state.sessions.get("slot1").await {
        Some(main) => main.config.read().await.pool.accounts.clone(),
        None => Vec::new(),
    };
    let (port, acc) = {
        let cfg = state.config.read().await;
        let port = pool_cdp_port(&cfg)?;
        let found = cfg
            .pool
            .accounts
            .iter()
            .find(|a| a.id == account_id)
            .or_else(|| launch_accounts.iter().find(|a| a.id == account_id))
            .cloned()
            .ok_or_else(|| format!("号池里没有账号 {account_id}"))?;
        let acc = pool_mod::session::resolve_account(&cfg.pool, &found);
        (port, acc)
    };

    // ---- Anti-top-out: claim this account before any CDP/login work --------
    // Held for the whole start sequence (RAII). A concurrent start of the same
    // identity from another slot fails fast instead of logging in twice.
    let identity = account_identity_key(&acc);
    let _account_guard = acquire_account_lock(&state, identity, &state.session.id).await?;

    // ① Autoplay on (hot). Do this FIRST so a bot manager is ready before the
    //    game starts; otherwise the first decisions would be missed.
    {
        let need_on = !state.config.read().await.autoplay.enabled;
        if need_on {
            let new_cfg = {
                let cfg = state.config.read().await;
                let mut c = cfg.clone();
                c.autoplay.enabled = true;
                c
            };
            update_config_inner(new_cfg, app.clone(), state.clone()).await?;
        }
    }
    ensure_pool_active(&state)?;

    let in_game = state.session.signals.snapshot().in_game;
    if in_game && !dry_run {
        return Err("当前在对局中，不能开始新对局（会让这一局掉线托管）".into());
    }

    // The browser is not launched at startup any more: open it on demand.
    ensure_capture_running(&app, state.clone()).await?;

    let cdp = pool_mod::GameCdp::connect(port)
        .await
        .map_err(|e| format!("连接游戏浏览器失败: {e:#}"))?;
    ensure_pool_active(&state)?;

    // Bring a tab that wandered to the 客服 webchat back to the game FIRST —
    // a non-game page never renders a canvas, so wait_page_ready below would
    // time out before prepare_lobby ever got a chance to fix it.
    pool_mod::ensure_game_url(&cdp.page, &state.pool_cancel)
        .await
        .map_err(|e| format!("返回游戏页失败: {e:#}"))?;

    // Page readiness: the browser may have been opened seconds ago — Majsoul
    // keeps loading long after the CDP port accepts connections. Clicking the
    // login form before the page renders goes nowhere ("卡在登录界面").
    pool_mod::wait_page_ready(&cdp.page, 60_000, &state.pool_cancel)
        .await
        .map_err(|e| format!("等待游戏页面加载失败: {e:#}"))?;
    ensure_pool_active(&state)?;

    // ② Ensure the right account is logged in (no-op if already).
    if !in_game {
        let sw = pool_mod::switch_account_retry(&cdp.page, &acc, in_game, &state.pool_cancel, 2)
            .await
            .map_err(|e| format!("换号失败: {e:#}"))?;
        let _ = sw; // status is reported by pool_status
        // localStorage flips to the new email before the Unity client has
        // finished the lobby transition. Clicking the ranked banner during
        // that window lands on a half-drawn scene. Give it a beat (bridge's
        // waitLobby does the same, but via screenshot judgement).
        for _ in 0..45 {
            ensure_pool_active(&state)?;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
    ensure_pool_active(&state)?;

    // ③ Enter the room, with staged recovery (see enter_room_with_recovery):
    //    plain → clear 活動 overlay without reload → reload + clear → verify
    //    login. Majsoul re-shows the full-screen 活動 page after EVERY reload,
    //    so a reload-only retry (the old code) could never recover from it —
    //    that is exactly what trapped slot4 on 2026-11-23.
    let cfg_snapshot = state.config.read().await.clone();
    let mut result = enter_room_with_recovery(
        &cdp.page,
        &cfg_snapshot.pool,
        &acc,
        &room,
        &mode,
        &state.session.signals,
        dry_run,
        &state.pool_cancel,
    )
    .await?;

    // The mode card can be miscalibrated: clicking "mode3e" once landed on
    // 四人东, and the old success judge (in_game) happily accepted it. Give the
    // tracker a moment to apply StartGame, then compare the real player count
    // with the requested mode. We still report entered=true (the match IS
    // running, stopping autoplay now would AFK the player) but surface a
    // clear warning instead of silently playing the wrong mode.
    if result.entered {
        let want = if result.mode.starts_with("mode3") { 3 } else { 4 };
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let mut got: Option<u8> = None;
        for _ in 0..20 {
            let (has4, has3) = {
                let t = state.game_tracker.lock().await;
                (t.state().is_some(), t.state_3p().is_some())
            };
            if has3 {
                got = Some(3);
                break;
            }
            if has4 {
                got = Some(4);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        if let Some(n) = got {
            if n != want {
                tracing::warn!(
                    "[pool] mode mismatch: requested {}p, tracker reports {}p",
                    want,
                    n
                );
                result.message = format!(
                    "已进入对局，但人数不符：请求 {} 人，实际 {} 人（模式坐标需校准）",
                    want, n
                );
            }
        }
    }
    Ok(result)
}

/// Spawn the persistent pool driver (auto-next + timed stop). It replaces
/// the bridge's `autoNextTick` / `timerTick` loops. Runs entirely inside
/// Aogara — no Node process, no HTTP. It only acts on config flags, so it is
/// cheap when the pool page is unused.
#[derive(Default)]
struct PoolDriverState {
    last_finish_count: Option<usize>,
    pending_finish: bool,
    timer_started: Option<std::time::Instant>,
    timer_fired: bool,
    last_job: Option<std::time::Instant>,
    idle_since: Option<std::time::Instant>,
}

/// One global driver for every session. The old design spawned one 5s loop
/// per session; with five accounts that meant five redundant config clones
/// and five wakeups every tick.
pub fn spawn_pool_driver(state: AppState) {
    tauri::async_runtime::spawn(async move {
        let mut drivers: HashMap<String, PoolDriverState> = HashMap::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            // The stress runner owns sequencing while a test run is active.
            if state.testing.is_running() {
                continue;
            }
            for session in state.sessions.all().await {
                let driver = drivers.entry(session.id.clone()).or_default();
                let session_state = state.with_session(session);
                run_pool_driver_tick(&session_state, driver).await;
            }
        }
    });
}

async fn run_pool_driver_tick(state: &AppState, driver: &mut PoolDriverState) {
    let (pool_cfg, autoplay_on, capture_args, idle_secs) = {
        let cfg = state.config.read().await;
        (
            cfg.pool.clone(),
            cfg.autoplay.enabled,
            cfg.capture.chromium.extra_args.clone(),
            cfg.multi.close_idle_browser_secs,
        )
    };

    let sess = state.session.signals.snapshot();
    let finish_now = sess.finish_count;
    let stop_after_game = state
        .pool_stop_after_game
        .load(std::sync::atomic::Ordering::SeqCst);

    // ---- Soft stop: wait until the current match is over, then release bot. ----
    if stop_after_game && !sess.in_game {
        let new_cfg = {
            let cfg = state.config.read().await;
            let mut c = cfg.clone();
            c.autoplay.enabled = false;
            c
        };
        if let Ok(()) = persist_config(&new_cfg, &state.config_path) {
            *state.config.write().await = new_cfg;
        }
        state
            .pool_stop_after_game
            .store(false, std::sync::atomic::Ordering::SeqCst);
        tracing::info!("[pool] soft stop complete: autoplay off after the match");
    }

    // Baseline on first tick so enabling auto-next cannot re-open the last
    // already-finished match.
    if driver.last_finish_count.is_none() {
        driver.last_finish_count = Some(finish_now);
    }
    let new_finish = driver
        .last_finish_count
        .map(|c| finish_now > c)
        .unwrap_or(false);
    if new_finish {
        driver.last_finish_count = Some(finish_now);
        driver.pending_finish = true;
    }

    let auto_next = state
        .auto_next_global
        .load(std::sync::atomic::Ordering::SeqCst);
    if !auto_next {
        driver.pending_finish = false;
        if new_finish && !sess.in_game && state.config.read().await.autoplay.enabled {
            let new_cfg = {
                let cfg = state.config.read().await;
                let mut c = cfg.clone();
                c.autoplay.enabled = false;
                c
            };
            if let Ok(()) = persist_config(&new_cfg, &state.config_path) {
                *state.config.write().await = new_cfg;
            }
            tracing::info!("[pool] auto-next off: autoplay disabled after match, bot released");
        }
    }

    // ---- Timed stop ----
    if pool_cfg.timer_enabled && !driver.timer_fired {
        let start = driver
            .timer_started
            .get_or_insert_with(std::time::Instant::now);
        let limit = std::time::Duration::from_secs((pool_cfg.timer_hours * 3600.0) as u64);
        if start.elapsed() >= limit && !sess.in_game {
            let new_cfg = {
                let cfg = state.config.read().await;
                let mut c = cfg.clone();
                c.autoplay.enabled = false;
                c
            };
            if let Ok(()) = persist_config(&new_cfg, &state.config_path) {
                *state.config.write().await = new_cfg;
            }
            driver.timer_fired = true;
            tracing::info!("[pool] timer fired: autoplay off after the match");
        }
    } else if !pool_cfg.timer_enabled {
        driver.timer_started = None;
        driver.timer_fired = false;
    }

    // ---- Idle browser release ----
    let capture_running = {
        let ctl = state.capture_control.lock().await;
        matches!(&ctl.status, CaptureStatus::Running { .. })
    };
    // Only release browsers that have actually finished a match. A freshly
    // opened browser is still logging in / navigating; closing it here would
    // abort auto-login and leave the user with a dead session.
    if idle_secs > 0
        && capture_running
        && finish_now > 0
        && !sess.in_game
        && !autoplay_on
        && !auto_next
    {
        let since = driver.idle_since.get_or_insert_with(std::time::Instant::now);
        if since.elapsed() >= std::time::Duration::from_secs(idle_secs) {
            match crate::testing::close_slot_browser(state.session.clone()).await {
                Ok(()) => tracing::info!(
                    "[pool {}] idle browser closed after {}s",
                    state.session.id,
                    idle_secs
                ),
                Err(e) => tracing::warn!("[pool {}] idle close failed: {e:#}", state.session.id),
            }
            driver.idle_since = None;
        }
    } else {
        driver.idle_since = None;
    }

    // ---- Auto-next ----
    if pool_cancelled(state)
        || state
            .session
            .paused
            .load(std::sync::atomic::Ordering::SeqCst)
        || !auto_next
        || sess.in_game
        || !driver.pending_finish
    {
        return;
    }
    let account_id = if pool_cfg.selected_id.is_empty() {
        return;
    } else {
        pool_cfg.selected_id.clone()
    };
    if let Some(t) = driver.last_job {
        if t.elapsed() < std::time::Duration::from_secs(60) {
            return;
        }
    }
    let launch_accounts = match state.sessions.get("slot1").await {
        Some(main) => main.config.read().await.pool.accounts.clone(),
        None => Vec::new(),
    };
    let acc = {
        let found = pool_cfg
            .accounts
            .iter()
            .find(|a| a.id == account_id)
            .or_else(|| launch_accounts.iter().find(|a| a.id == account_id))
            .cloned();
        match found {
            Some(a) => pool_mod::session::resolve_account(&pool_cfg, &a),
            None => return,
        }
    };
    let port = match pool_mod::game_cdp_port(&capture_args) {
        Some(p) => p,
        None => return,
    };
    driver.pending_finish = false;
    driver.last_job = Some(std::time::Instant::now());
    let cfg_snapshot_ref = state.config.read().await.clone();

    let result: Result<pool_mod::EnterResult, String> = async {
        let cdp = pool_mod::GameCdp::connect(port)
            .await
            .map_err(|e| format!("CDP: {e:#}"))?;
        let _ = pool_mod::settle(
            &cdp.page,
            &cfg_snapshot_ref.pool,
            &acc,
            &state.session.signals,
            &state.pool_cancel,
        )
        .await;
        let _ = pool_mod::switch_account_retry(
            &cdp.page,
            &acc,
            false,
            &state.pool_cancel,
            2,
        )
        .await
        .map_err(|e| format!("switch: {e:#}"))?;
        enter_room_with_recovery(
            &cdp.page,
            &cfg_snapshot_ref.pool,
            &acc,
            &pool_cfg.target.room,
            &pool_cfg.target.mode,
            &state.session.signals,
            false,
            &state.pool_cancel,
        )
        .await
    }
    .await;

    match result {
        Ok(r) => tracing::info!("[pool] auto-next: {}", r.message),
        Err(e) => tracing::warn!("[pool] auto-next failed: {e}"),
    }
    if !autoplay_on {
        let new_cfg = {
            let cfg = state.config.read().await;
            let mut c = cfg.clone();
            c.autoplay.enabled = true;
            c
        };
        let _ = persist_config(&new_cfg, &state.config_path);
        *state.config.write().await = new_cfg;
    }
}

/// List Chrome-for-Testing versions currently installed under
/// `<user_config_root>/chrome-for-testing/`. Newest first. Empty when
/// nothing is installed or the platform isn't supported by CfT.
#[tauri::command]
pub async fn list_cft_installed() -> CmdResult<Vec<String>> {
    Ok(crate::capture::chromium::cft::list_installed())
}

/// Download + extract Chrome-for-Testing for the current platform.
/// `channel` is interpreted as: `"stable"` / `"beta"` / `"dev"` /
/// `"canary"` (channel pin) or any literal version string (e.g.
/// `"131.0.6778.85"`). Empty string ≡ `"stable"`. Returns the
/// installed version.
///
/// Progress is reported through `NotifyBus` with sticky id
/// `capture-cft-download` so the frontend can show a single live toast.
#[tauri::command]
pub async fn download_chrome_for_testing(
    channel: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let raw = channel.unwrap_or_default();
    let parsed = crate::capture::chromium::cft::Channel::parse(&raw);
    crate::capture::chromium::cft::install(&parsed, &state.notify_bus)
        .await
        .map_err(|e| format!("install Chrome for Testing: {e:#}"))
}

/// Remove an installed Chrome-for-Testing version. No-op when the
/// version isn't installed.
#[tauri::command]
pub async fn remove_chrome_for_testing(
    version: String,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    if version.is_empty()
        || version.contains('/')
        || version.contains('\\')
        || version.contains("..")
    {
        return Err(format!("invalid CfT version {version:?}"));
    }
    crate::capture::chromium::cft::remove(&version)
        .map_err(|e| format!("remove Chrome for Testing: {e:#}"))?;
    let _ = state.notify_bus.send(Notification::success(format!(
        "Removed Chrome for Testing {version}"
    )));
    Ok(())
}

/// Stop the running capture backend. Kicks in-flight WebSocket flows
/// (MITM mode) and signals the supervisor to tear down. Returns Err if
/// nothing is running.
#[tauri::command]
pub async fn stop_capture(state: State<'_, AppState>) -> CmdResult<()> {
    let (stop, force_close) = {
        let mut ctl = state.capture_control.lock().await;
        (ctl.stop.take(), ctl.force_close.clone())
    };
    // Kick in-flight WS flows first so the game client actually
    // disconnects. Without this, hudsucker's graceful shutdown only
    // blocks new connections; existing ones drain naturally and the
    // user sees comm "still working" even after stop. (Chromium backend
    // ignores this — its shutdown closes the browser process directly.)
    force_close.notify_waiters();
    match stop {
        Some(tx) => {
            // Receiver dropped means the task already exited — that's fine,
            // we still cleared `stop` and the status forwarder will catch
            // up via the next CaptureStatus emission.
            let _ = tx.send(());
            Ok(())
        }
        None => Err("capture backend is not running".into()),
    }
}

#[tauri::command]
pub async fn get_status(state: State<'_, AppState>) -> CmdResult<Snapshot> {
    let config = state.config.read().await.clone();
    let bot_status = state.bot_status.read().await.clone();
    let capture_status = state.capture_control.lock().await.status.clone();
    let log_dir = state.log_session.dir().to_path_buf();
    Ok(Snapshot {
        config,
        bot_status,
        capture_status,
        log_dir,
    })
}

/// One-shot read of the latest [`crate::schema::CaptureStatus`]. Cheaper
/// than `get_status` when the caller only needs the capture lifecycle.
#[tauri::command]
pub async fn get_capture_status(
    state: State<'_, AppState>,
) -> CmdResult<crate::schema::CaptureStatus> {
    Ok(state.capture_control.lock().await.status.clone())
}

#[tauri::command]
pub async fn get_log_dir(state: State<'_, AppState>) -> CmdResult<PathBuf> {
    Ok(state.log_session.dir().to_path_buf())
}

/// Best-effort "open this folder in the OS file manager". Spawns the
/// platform-native opener and returns immediately — we don't wait on the
/// child, so a missing tool surfaces only if `spawn` itself fails. The
/// frontend already wraps this in try/catch.
#[tauri::command]
pub async fn open_log_folder(session: Option<String>, state: State<'_, AppState>) -> CmdResult<()> {
    let target = match session {
        Some(name) if !name.is_empty() => {
            // Defense-in-depth: only allow the canonical session name
            // shape — no `..`, no separators — so a malicious frontend
            // can't redirect this to anywhere else on disk.
            if !is_session_name(&name) {
                return Err(format!("invalid session name {name:?}"));
            }
            state.log_session.root().join(name)
        }
        _ => state.log_session.dir().to_path_buf(),
    };
    open_path(&target)
}

fn open_path(path: &Path) -> CmdResult<()> {
    #[cfg(target_os = "linux")]
    let cmd = "xdg-open";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "windows")]
    let cmd = "explorer";

    std::process::Command::new(cmd)
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("open path {}: {e}", path.display()))
}

/// The `delay.lua` next to the config file, next to the bundled default:
/// what the frontend needs to offer an edited script the new default once
/// per default (see `autoplay::delay::script_state`).
#[derive(Debug, serde::Serialize)]
pub struct DelayScriptStatus {
    pub state: crate::autoplay::delay::ScriptState,
    /// Names the bundled default; changes when an update ships a new one.
    pub version: String,
}

fn delay_script_path(state: &AppState) -> PathBuf {
    state
        .config_path
        .parent()
        .map(|dir| dir.join("delay.lua"))
        .unwrap_or_else(|| PathBuf::from("delay.lua"))
}

#[tauri::command]
pub async fn delay_script_status(state: State<'_, AppState>) -> CmdResult<DelayScriptStatus> {
    Ok(DelayScriptStatus {
        state: crate::autoplay::delay::script_state(&delay_script_path(&state)),
        version: crate::autoplay::delay::default_version(),
    })
}

/// Replace `delay.lua` with the bundled default, keeping the old file as a
/// timestamped `.bak` beside it. Returns the backup's path, if any. The
/// autoplay manager hot-reloads the new script.
#[tauri::command]
pub async fn replace_delay_script(state: State<'_, AppState>) -> CmdResult<Option<String>> {
    crate::autoplay::delay::replace_with_default(&delay_script_path(&state))
        .map(|backup| backup.map(|p| p.display().to_string()))
        .map_err(|e| format!("replace delay.lua: {e}"))
}

/// Opens an `http(s)://` URL in the user's default browser. Used by the
/// first-run wizard's GitHub / Discord links and the purchase flow's PayPal
/// approve page — Tauri 2's webview won't reliably honour `target="_blank"`
/// without the opener plugin, so we route the click through the OS's native
/// handler ourselves. Validates the scheme to keep this from being abused as
/// a generic process spawn.
///
/// Goes through the `opener` crate (ShellExecuteW on Windows, `open` on
/// macOS, xdg-open on Linux) rather than spawning `explorer <url>`:
/// explorer.exe silently opens the Documents folder instead of the browser
/// when the URL carries a query string (e.g. PayPal's `?token=...`).
#[tauri::command]
pub async fn open_external_url(url: String) -> CmdResult<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("refused non-http(s) url: {url}"));
    }
    // `opener::open` can block briefly (it may wait on the launcher), so keep
    // it off the async runtime.
    tauri::async_runtime::spawn_blocking(move || {
        opener::open(&url).map_err(|e| format!("open url {url}: {e}"))
    })
    .await
    .map_err(|e| format!("open url task: {e}"))?
}

/// Strict matcher for session directory names — `YYYYMMDD-HHMMSS`. Used
/// both to filter `list_log_sessions` output and to validate user-
/// supplied session names before they touch the filesystem.
fn is_session_name(name: &str) -> bool {
    if name.len() != 15 {
        return false;
    }
    let bytes = name.as_bytes();
    bytes.iter().enumerate().all(|(i, &b)| {
        if i == 8 {
            b == b'-'
        } else {
            b.is_ascii_digit()
        }
    })
}

/// List every session directory under the active log root. Newest first.
/// `is_active` marks the session this process is currently writing — the
/// UI uses it to enable live tail.
#[tauri::command]
pub async fn list_log_sessions(state: State<'_, AppState>) -> CmdResult<Vec<LogSessionInfo>> {
    let active_name = state
        .log_session
        .dir()
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let root = state.log_session.root().to_path_buf();

    tokio::task::spawn_blocking(move || -> Result<Vec<LogSessionInfo>, String> {
        let mut out = Vec::new();
        let rd = match std::fs::read_dir(&root) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(format!("read_dir {}: {e}", root.display())),
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_session_name(&name) {
                continue;
            }
            let path = entry.path();
            // Cheap directory size: sum file sizes in the immediate dir.
            // Sub-directories (per-platform flow logs) are NOT walked —
            // for the UI it's enough to be a rough indicator, and a
            // recursive walk on a session with thousands of game files
            // would block the picker.
            let sensitive = path.join(crate::logger::SENSITIVE_DIR);
            let mut size_bytes: u64 = 0;
            for dir in [&path, &sensitive] {
                if let Ok(rd2) = std::fs::read_dir(dir) {
                    for f in rd2.flatten() {
                        if let Ok(m) = f.metadata() {
                            if m.is_file() {
                                size_bytes += m.len();
                            }
                        }
                    }
                }
            }
            let mtime_ms = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let is_active = name == active_name;
            out.push(LogSessionInfo {
                name,
                path,
                size_bytes,
                mtime_ms,
                is_active,
                legacy_layout: !sensitive.is_dir(),
            });
        }
        out.sort_by(|a, b| b.name.cmp(&a.name));
        Ok(out)
    })
    .await
    .map_err(|e| format!("list_log_sessions join: {e}"))?
}

/// Read filtered, paginated entries from one session's `all.jsonl`.
///
/// Filtering is server-side so a frontend opening a 100k-line session can
/// still ask "just the errors" without dragging the whole file across the
/// wire. The reader is line-oriented — one bad line is counted under
/// `skipped_malformed` and skipped, so a partial trailing line in the
/// active session (still being written) doesn't poison the response.
/// `limit` is capped at 2000 to bound payload size.
#[tauri::command]
pub async fn read_log_session(
    req: ReadLogRequest,
    state: State<'_, AppState>,
) -> CmdResult<ReadLogResponse> {
    if !is_session_name(&req.session) {
        return Err(format!("invalid session name {:?}", req.session));
    }
    let root = state.log_session.root().to_path_buf();

    tokio::task::spawn_blocking(move || -> Result<ReadLogResponse, String> {
        let path = root.join(&req.session).join("all.jsonl");
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ReadLogResponse {
                    entries: Vec::new(),
                    has_more: false,
                    skipped_malformed: 0,
                });
            }
            Err(e) => return Err(format!("open {}: {e}", path.display())),
        };
        let reader = std::io::BufReader::new(file);
        use std::io::BufRead;

        let level_set: Option<std::collections::HashSet<String>> =
            req.levels.as_ref().map(|v| v.iter().cloned().collect());
        let target_filters: Vec<String> = req.targets.unwrap_or_default();
        let search_lc = req.search.as_deref().map(|s| s.to_lowercase());
        // 0 → use a sane default (1000); otherwise hard-cap at 2000.
        let limit = if req.limit == 0 {
            1000
        } else {
            req.limit.min(2000)
        };

        let mut skipped_malformed: u32 = 0;
        let mut total_match: usize = 0;
        let mut entries: Vec<LogEntry> = Vec::with_capacity(limit);

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let entry: LogEntry = match serde_json::from_str(trimmed) {
                Ok(e) => e,
                Err(_) => {
                    skipped_malformed += 1;
                    continue;
                }
            };
            if let Some(ref ls) = level_set {
                if !ls.contains(&entry.level) {
                    continue;
                }
            }
            if !target_filters.is_empty()
                && !target_filters.iter().any(|t| entry.target.starts_with(t))
            {
                continue;
            }
            if let Some(ref s) = search_lc {
                if !entry.message.to_lowercase().contains(s) {
                    continue;
                }
            }
            total_match += 1;
            if total_match > req.offset && entries.len() < limit {
                entries.push(entry);
            }
        }

        let has_more = total_match > req.offset + entries.len();
        Ok(ReadLogResponse {
            entries,
            has_more,
            skipped_malformed,
        })
    })
    .await
    .map_err(|e| format!("read_log_session join: {e}"))?
}

/// The full inspector file under `sensitive/`, or the session's only one if
/// it was recorded before the split. The Inspector tab is local, so it shows
/// everything.
fn inspector_path(session_dir: &Path) -> PathBuf {
    let full = session_dir
        .join(crate::logger::SENSITIVE_DIR)
        .join("inspector.jsonl");
    if full.is_file() {
        full
    } else {
        session_dir.join("inspector.jsonl")
    }
}

/// Bundle log sessions into a zip for a bug report. See `logger::pack`.
#[tauri::command]
pub async fn pack_logs(
    req: crate::schema::PackLogsRequest,
    state: State<'_, AppState>,
) -> CmdResult<crate::logger::pack::PackSummary> {
    if let Some(bad) = req.sessions.iter().find(|s| !is_session_name(s)) {
        return Err(format!("invalid session name {bad:?}"));
    }
    let root = state.log_session.root().to_path_buf();
    let opts = crate::logger::pack::PackOptions {
        include_sensitive: req.include_sensitive,
        password: req.password,
    };
    tokio::task::spawn_blocking(move || {
        crate::logger::pack::pack(&root, &req.sessions, &opts, &req.dest)
    })
    .await
    .map_err(|e| format!("pack_logs join: {e}"))?
    .map_err(|e| format!("{e:#}"))
}

/// Read filtered, paginated entries from a session's `inspector.jsonl`.
/// Same line-oriented, fault-tolerant reader as `read_log_session` —
/// malformed lines bumped under `skipped_malformed`, `limit` capped at
/// 2000. Filtering is server-side so a session with hundreds of
/// thousands of events doesn't have to cross the wire to be narrowed.
#[tauri::command]
pub async fn read_inspector(
    req: ReadInspectorRequest,
    state: State<'_, AppState>,
) -> CmdResult<ReadInspectorResponse> {
    if !is_session_name(&req.session) {
        return Err(format!("invalid session name {:?}", req.session));
    }
    let root = state.log_session.root().to_path_buf();

    tokio::task::spawn_blocking(move || -> Result<ReadInspectorResponse, String> {
        let path = inspector_path(&root.join(&req.session));
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ReadInspectorResponse {
                    entries: Vec::new(),
                    has_more: false,
                    skipped_malformed: 0,
                });
            }
            Err(e) => return Err(format!("open {}: {e}", path.display())),
        };
        let reader = std::io::BufReader::new(file);
        use std::io::BufRead;

        let kind_set: Option<std::collections::HashSet<String>> =
            req.kinds.as_ref().map(|v| v.iter().cloned().collect());
        let actor = req.actor;
        let search_lc = req.search.as_deref().map(|s| s.to_lowercase());
        let limit = if req.limit == 0 {
            1000
        } else {
            req.limit.min(2000)
        };

        let mut skipped_malformed: u32 = 0;
        let mut total_match: usize = 0;
        let mut entries: Vec<InspectorEntry> = Vec::with_capacity(limit);

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let entry: InspectorEntry = match serde_json::from_str(trimmed) {
                Ok(e) => e,
                Err(_) => {
                    skipped_malformed += 1;
                    continue;
                }
            };
            if !inspector_matches(&entry, kind_set.as_ref(), actor, search_lc.as_deref()) {
                continue;
            }
            total_match += 1;
            if total_match > req.offset && entries.len() < limit {
                entries.push(entry);
            }
        }

        let has_more = total_match > req.offset + entries.len();
        Ok(ReadInspectorResponse {
            entries,
            has_more,
            skipped_malformed,
        })
    })
    .await
    .map_err(|e| format!("read_inspector join: {e}"))?
}

/// Predicate factored out of `read_inspector` so the filtering rules are
/// in one place (and easy to test, when we add tests for the inspector
/// pipeline). `kind_set` matches against the `kind` discriminant; `actor`
/// matches mjai events with an `actor` field and bot reactions by
/// `actor_id`; ws frames are unaffected by the actor filter (they're
/// pre-bridge, no actor concept yet) — they only drop out via `kinds`.
fn inspector_matches(
    entry: &InspectorEntry,
    kind_set: Option<&std::collections::HashSet<String>>,
    actor: Option<u8>,
    search_lc: Option<&str>,
) -> bool {
    let kind = match entry {
        InspectorEntry::WsFrame { .. } => "ws_frame",
        InspectorEntry::MjaiEvent { .. } => "mjai_event",
        InspectorEntry::BotReaction { .. } => "bot_reaction",
        InspectorEntry::Http { .. } => "http",
    };
    if let Some(ks) = kind_set {
        if !ks.contains(kind) {
            return false;
        }
    }
    if let Some(a) = actor {
        match entry {
            InspectorEntry::WsFrame { .. } => {} // pre-bridge, no actor
            // Describes the client, not any seat — same treatment as a
            // ws frame: only the `kinds` filter can drop it.
            InspectorEntry::Http { .. } => {}
            InspectorEntry::MjaiEvent { event, .. } => {
                if let Some(ev_actor) = mjai_event_actor(event) {
                    if ev_actor != a {
                        return false;
                    }
                } else {
                    return false;
                }
            }
            InspectorEntry::BotReaction { reaction, .. } => {
                if reaction.actor_id != a {
                    return false;
                }
            }
        }
    }
    if let Some(s) = search_lc {
        let hay = match entry {
            InspectorEntry::WsFrame { raw, parsed, .. } => {
                let mut buf = match raw {
                    crate::schema::FrameRaw::Text(t) => t.to_lowercase(),
                    crate::schema::FrameRaw::Binary(b) => b.to_lowercase(),
                    crate::schema::FrameRaw::Withheld => String::new(),
                };
                if let Some(p) = parsed {
                    buf.push(' ');
                    buf.push_str(&p.method.to_lowercase());
                    buf.push(' ');
                    buf.push_str(&p.args.to_string().to_lowercase());
                }
                buf
            }
            InspectorEntry::MjaiEvent { event, .. } => serde_json::to_string(event)
                .unwrap_or_default()
                .to_lowercase(),
            InspectorEntry::BotReaction { reaction, .. } => serde_json::to_string(reaction)
                .unwrap_or_default()
                .to_lowercase(),
            // The whole exchange, so a search for a URL, a header, a
            // status, or anything a recognizer decoded all hit.
            InspectorEntry::Http { exchange, .. } => serde_json::to_string(exchange)
                .unwrap_or_default()
                .to_lowercase(),
        };
        if !hay.contains(s) {
            return false;
        }
    }
    true
}

fn mjai_event_actor(event: &crate::schema::MjaiEvent) -> Option<u8> {
    use crate::schema::MjaiEvent as E;
    match event {
        E::Tsumo { actor, .. }
        | E::Dahai { actor, .. }
        | E::Chi { actor, .. }
        | E::Pon { actor, .. }
        | E::Daiminkan { actor, .. }
        | E::Kakan { actor, .. }
        | E::Ankan { actor, .. }
        | E::Reach { actor, .. }
        | E::ReachAccepted { actor }
        | E::Hora { actor, .. }
        | E::Kita { actor, .. } => Some(*actor),
        _ => None,
    }
}

/// Subscribe to live inspector events. Same shape and lossy semantics as
/// `subscribe_log_events`. The Logs → Inspector tab uses this for the
/// active session's live tail.
#[tauri::command]
pub async fn subscribe_inspector(
    state: State<'_, AppState>,
    on_event: tauri::ipc::Channel<InspectorEntry>,
) -> CmdResult<()> {
    let mut rx = state.log_session.subscribe_inspector();
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(entry) => {
                    let _ = on_event.send(entry);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    // Inject a synthetic mjai-event-shaped warn marker so
                    // the UI sees the gap. We piggyback on MjaiEvent::None
                    // — the inspector tab knows to render dropped-event
                    // markers when it sees `none` interleaved.
                    let warn = InspectorEntry::MjaiEvent {
                        ts_ms: chrono::Local::now().timestamp_millis(),
                        event: crate::schema::MjaiEvent::None,
                    };
                    let _ = on_event.send(warn);
                    tracing::warn!(
                        target: "aogara.inspector",
                        "inspector forwarder dropped {n} entries"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    Ok(())
}

/// Subscribe to live log events. Each event the active session emits is
/// forwarded over the supplied `tauri::ipc::Channel`. Slow consumers
/// surface a synthetic `WARN aogara.logger "dropped N events…"` so the
/// UI can show the gap explicitly. The forwarder task lives until the
/// broadcast is closed (process shutdown).
#[tauri::command]
pub async fn subscribe_log_events(
    state: State<'_, AppState>,
    on_event: tauri::ipc::Channel<LogEntry>,
) -> CmdResult<()> {
    let mut rx = state.log_session.subscribe();
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(entry) => {
                    let _ = on_event.send(entry);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    let warn = LogEntry {
                        ts_ms: chrono::Local::now().timestamp_millis(),
                        level: "WARN".into(),
                        target: "aogara.logger".into(),
                        file: None,
                        line: None,
                        message: format!("dropped {n} log events (consumer too slow)"),
                        fields: std::collections::HashMap::new(),
                    };
                    let _ = on_event.send(warn);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    Ok(())
}

/// Latest analysis output. `None` until the analysis runner has produced
/// at least one result for the current game.
#[tauri::command]
pub async fn get_analysis(
    slot: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<Option<AnalysisResult>> {
    let state = session_state(state, slot).await?;
    let v = state.analysis_cache.read().await.clone();
    Ok(v)
}

/// Live game-state snapshot from the tracker. `None` before any
/// `start_game` event has been observed.
#[tauri::command]
pub async fn get_game_snapshot(
    slot: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<Option<GameStateSnapshot>> {
    let state = session_state(state, slot).await?;
    let snap = state.game_tracker.lock().await.snapshot();
    Ok(snap)
}

/// Score a hypothetical bot hora (ron or tsumo) against the live engine
/// state. The bot's own `hora` mjai response carries no score data
/// (`deltas` is only populated on the inbound mjai event from the platform
/// after the win is confirmed). The frontend's `BotActionTile` calls this
/// to display "+8000點" beside the action label.
///
/// Returns `None` when no game is in progress, when the actor's hand isn't
/// a valid agari shape, or when the winning tile can't be inferred from
/// the live state (no recent discard for ron / no recent draw for tsumo).
#[tauri::command]
pub async fn compute_bot_hora_score(
    actor: u8,
    is_tsumo: bool,
    state: State<'_, AppState>,
) -> CmdResult<Option<HoraScoreInfo>> {
    Ok(state
        .game_tracker
        .lock()
        .await
        .evaluate_hora(actor, is_tsumo))
}

/// Pre-encoded mahgen DSL strings ready for the frontend `<mah-gen>`
/// element. Built from the same snapshot as `get_game_snapshot` — call
/// whichever the UI surface prefers; both are O(34 tiles) to generate.
#[tauri::command]
pub async fn get_mahgen_view(
    slot: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<Option<MahgenView>> {
    let state = session_state(state, slot).await?;
    let view = state
        .game_tracker
        .lock()
        .await
        .snapshot()
        .map(|s| MahgenView::from_snapshot(&s));
    Ok(view)
}

/// Resolve an optional `slot` argument to that session's AppState clone.
/// `None` means the launch session (slot1) — keeps every existing caller
/// working unchanged.
async fn session_state(
    state: State<'_, AppState>,
    slot: Option<String>,
) -> CmdResult<AppState> {
    match slot.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(state.inner().clone()),
        Some(s) => {
            let rt = state
                .sessions
                .get(s)
                .await
                .ok_or_else(|| format!("没有会话 {s}"))?;
            Ok(state.with_session(rt))
        }
    }
}

/// Remove a bot's directory under `bot.dir/<name>/`. Refuses to delete
/// the currently-active bot — user must `set_active_bot` to a different
/// one first. Refuses target paths that escape `bot.dir` (defense in
/// depth even though `name` came from the bot list, not raw user input).
#[tauri::command]
pub async fn delete_bot(name: String, state: State<'_, AppState>) -> CmdResult<()> {
    let (active_4p, active_3p, dir) = {
        let cfg = state.config.read().await;
        (
            cfg.bot.active_4p.clone(),
            cfg.bot.active_3p.clone(),
            cfg.bot.dir.clone(),
        )
    };
    if active_4p == name || active_3p == name {
        return Err(format!(
            "{name:?} is an active bot (4p or 3p) — switch to a different bot first"
        ));
    }
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(format!("invalid bot name {name:?}"));
    }
    let resolved_root = resolve_dir(Path::new(&dir));
    let target = resolved_root.join(&name);
    if !target.is_dir() {
        return Err(format!("bot {name:?} not found at {}", target.display()));
    }
    let canon_root = std::fs::canonicalize(&resolved_root)
        .map_err(|e| format!("canonicalize {}: {e}", resolved_root.display()))?;
    let canon_target = std::fs::canonicalize(&target)
        .map_err(|e| format!("canonicalize {}: {e}", target.display()))?;
    if !canon_target.starts_with(&canon_root) {
        return Err(format!("bot {name:?} resolves outside the bot directory"));
    }
    std::fs::remove_dir_all(&canon_target)
        .map_err(|e| format!("remove {}: {e}", canon_target.display()))?;
    let _ = state
        .notify_bus
        .send(Notification::success(format!("Deleted bot {name}")));
    Ok(())
}

// ---------- Game history ----------
//
// Reads/writes are delegated to `state.history_store`. All errors bubble
// up as user-readable strings (the store error chain via anyhow has the
// detail we need; `:#` prints the full chain).

/// Filtered, paginated listing of finalised games. Newest-first by
/// `started_at`. `limit == 0` means use the store's default cap.
#[tauri::command]
pub async fn list_game_history(
    filter: Option<HistoryFilter>,
    limit: Option<u32>,
    offset: Option<u32>,
    state: State<'_, AppState>,
) -> CmdResult<Vec<GameRecord>> {
    let store = state.history_store.clone();
    let filter = filter.unwrap_or_default();
    let limit = limit.unwrap_or(0);
    let offset = offset.unwrap_or(0);
    tokio::task::spawn_blocking(move || store.list(&filter, limit, offset))
        .await
        .map_err(|e| format!("history list join error: {e}"))?
        .map_err(|e| format!("{e:#}"))
}

/// Single record by id.
#[tauri::command]
pub async fn get_game_history_record(
    id: String,
    state: State<'_, AppState>,
) -> CmdResult<Option<GameRecord>> {
    let store = state.history_store.clone();
    tokio::task::spawn_blocking(move || store.get(&id))
        .await
        .map_err(|e| format!("history get join error: {e}"))?
        .map_err(|e| format!("{e:#}"))
}

/// Full mjai event stream for a recorded game. `None` if the id is unknown.
#[tauri::command]
pub async fn get_game_history_events(
    id: String,
    state: State<'_, AppState>,
) -> CmdResult<Option<HistoryEventLog>> {
    let store = state.history_store.clone();
    tokio::task::spawn_blocking(move || store.get_events(&id))
        .await
        .map_err(|e| format!("history get_events join error: {e}"))?
        .map_err(|e| format!("{e:#}"))
}

/// Delete a recorded game (its index entry + games/<id>.mjai.jsonl).
/// Returns true if a record was actually removed. Emits a
/// `HistoryEvent::Deleted` on the history bus so the frontend can drop
/// the row from its cache without a refetch.
#[tauri::command]
pub async fn delete_game_history_entry(id: String, state: State<'_, AppState>) -> CmdResult<bool> {
    let store = state.history_store.clone();
    let id_for_blocking = id.clone();
    let removed = tokio::task::spawn_blocking(move || store.delete(&id_for_blocking))
        .await
        .map_err(|e| format!("history delete join error: {e}"))?
        .map_err(|e| format!("{e:#}"))?;
    if removed {
        let _ = state.history_bus.send(HistoryEvent::Deleted { id });
    }
    Ok(removed)
}

/// Upstream repo slug — kept as a const so the auto-updater can be wired
/// back in without touching call sites.
///
/// TODO(aogara): Aogara is a personal fork (v1.1.0) with no published
/// release channel yet. The old value `shinkuan/Aogara` does not exist
/// (404), and even the real upstream (`shinkuan/Akagi`, v3.8.0) would be
/// a false "new version" for a 1.x fork and could overwrite the fork with
/// upstream. The user asked to DISABLE update checking for now and leave
/// a seed for their own repo. When you publish, set this to your own
/// `owner/repo` and delete the early-return in `check_for_update` below.
/// 引子：当前关闭更新检查；后续接个人仓库时把这里改成自己的 owner/repo，
/// 并删掉 check_for_update 里的提前返回即可，其余链路不用动。
#[allow(dead_code)]
const UPSTREAM_REPO: &str = "shinkuan/Aogara";

/// One-shot "is there a newer release?" — frontend calls this on app
/// launch (with a 6h cache) and from the Settings "Check for updates"
/// button. Returns `Ok(None)` for "already up to date" or unsupported
/// platforms; `Err(String)` for network / parse failures (the toast in
/// the frontend surfaces the message verbatim).
#[tauri::command]
pub async fn check_for_update(
    _state: State<'_, AppState>,
) -> CmdResult<Option<crate::updater::UpdateInfo>> {
    // Update checking is intentionally disabled for the personal fork.
    // Returning None means "up to date": no sidebar red-dot, no launch
    // toast, and no network hit to a non-existent repo. To re-enable,
    // see the TODO on UPSTREAM_REPO above.
    Ok(None)
}

/// Download the release zip found by the last `check_for_update`
/// (mirror fallback per `[network]` config), verify digest + minisign
/// signature, swap the binary via `self_replace::self_replace`, then
/// relaunch. Takes no payload — the pending update is read from
/// `AppState`, so the webview cannot substitute its own URLs or trust
/// markers. On success the process exits inside `app.restart()` and
/// this never returns. The typed error variant lets the frontend
/// distinguish "fall back to release page" (`read_only_install`,
/// `unsupported_platform`, `no_matching_asset`, `signature_missing`)
/// from a real network / integrity error.
#[tauri::command]
pub async fn apply_update(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), crate::updater::UpdateError> {
    let Ok(_guard) = state.updater_lock.try_lock() else {
        return Err(crate::updater::UpdateError::Other {
            message: "another update operation is in progress".into(),
        });
    };
    let info = state.pending_update.read().await.clone();
    let Some(info) = info else {
        return Err(crate::updater::UpdateError::Other {
            message: "no pending update — run a check first".into(),
        });
    };
    let net = state.config.read().await.network.clone();
    crate::updater::apply::download_and_apply(&app, &info, &net).await
}

// ---------- Built-in bot cloud inference (native API) ----------
//
// Thin passthroughs to `crate::bot::api`. They take the server URL / key as
// explicit args (rather than reading them from config) so the frontend can
// verify a key before saving it, and redeem a code before any key exists.

/// Redeem a prepaid code (`POST /v3/redeem`, no auth). By default mints a new
/// key; pass `renew_key` to stack time onto a key you already hold.
#[tauri::command]
pub async fn native_api_redeem(
    base_url: String,
    proxy: Option<String>,
    code: String,
    email: Option<String>,
    renew_key: Option<String>,
) -> CmdResult<crate::bot::api::RedeemResponse> {
    crate::bot::api::redeem(
        &base_url,
        proxy.as_deref().unwrap_or(""),
        &code,
        email.as_deref(),
        renew_key.as_deref(),
    )
    .await
    .map_err(|e| format!("{e:#}"))
}

/// Fetch a key's plan / expiry / live limits (`GET /v3/key`).
#[tauri::command]
pub async fn native_api_key_status(
    base_url: String,
    proxy: Option<String>,
    key: String,
) -> CmdResult<crate::bot::api::KeyStatus> {
    crate::bot::api::ApiClient::new(&base_url, &key, proxy.as_deref().unwrap_or(""))
        .map_err(|e| format!("{e:#}"))?
        .key_status()
        .await
        .map_err(|e| format!("{e:#}"))
}

/// List the models a key's plan may use (`GET /v3/models`).
#[tauri::command]
pub async fn native_api_models(
    base_url: String,
    proxy: Option<String>,
    key: String,
) -> CmdResult<Vec<crate::bot::api::ModelInfo>> {
    crate::bot::api::ApiClient::new(&base_url, &key, proxy.as_deref().unwrap_or(""))
        .map_err(|e| format!("{e:#}"))?
        .models()
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Liveness + aggregate load (`GET /healthz`, no auth).
#[tauri::command]
pub async fn native_api_health(
    base_url: String,
    proxy: Option<String>,
) -> CmdResult<crate::bot::api::Health> {
    crate::bot::api::health(&base_url, proxy.as_deref().unwrap_or(""))
        .await
        .map_err(|e| format!("{e:#}"))
}

// ---------- Whole-game review (native API, `/v3/review*`) ----------
//
// Same family as the commands above: server URL / key travel as explicit
// args from the frontend's config store. The submit is the one exception to
// "thin passthrough" — it loads the recorded mjai log server-side (Rust) so
// a whole game never round-trips through the webview, and reuses the native
// bot's censor/shaping helper so `/v3/review` sees the exact same
// perspective stream `/v3/react` does.

/// Hard cap `POST /v3/review` places on a submitted stream. Checked here so
/// an oversized log fails with a clear local message instead of a generic
/// server `400`.
const REVIEW_MAX_EVENTS: usize = 4096;

/// Submit a recorded history game for whole-game review. `id` is the
/// history record's ULID; `model` optionally pins the model (empty ⇒ the
/// server default for the game's player count).
#[tauri::command]
pub async fn native_api_review_history_game(
    base_url: String,
    proxy: Option<String>,
    key: String,
    id: String,
    model: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<crate::bot::api::ReviewSubmitted> {
    let store = state.history_store.clone();
    let record_id = id.clone();
    let loaded = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        Ok((store.get(&record_id)?, store.get_events(&record_id)?))
    })
    .await
    .map_err(|e| format!("history review join error: {e}"))?
    .map_err(|e| format!("{e:#}"))?;
    let (record, events) = loaded;
    let Some(record) = record else {
        return Err(format!("history record {id:?} not found"));
    };
    // Index entry present but the games/<id>.mjai.jsonl copy is gone —
    // distinct message so the user isn't told the record doesn't exist.
    let Some(events) = events else {
        return Err(format!("event log for history record {id:?} is missing"));
    };
    // Observer / replay recordings carry no own-seat; there is no perspective
    // to review from (and no hand visible to build one).
    let Some(seat) = record.our_seat else {
        return Err("record has no player seat; cannot review an observed game".into());
    };

    let events = crate::bot::native::build_api_events(&events, seat, record.num_players);
    if events.len() > REVIEW_MAX_EVENTS {
        return Err(format!(
            "game log has {} events, over the review limit of {REVIEW_MAX_EVENTS}",
            events.len()
        ));
    }

    // An omitted model resolves to the server's default **4p** model, which
    // rejects a sanma stream — so a 3p game with no configured model falls
    // back to the id `"3p"`, which the server documents as resolving to its
    // default 3p model.
    let configured = model.as_deref().map(str::trim).filter(|m| !m.is_empty());
    let model = match configured {
        Some(m) => Some(m.to_string()),
        None if record.num_players == 3 => Some("3p".to_string()),
        None => None,
    };

    crate::bot::api::ApiClient::new(&base_url, &key, proxy.as_deref().unwrap_or(""))
        .map_err(|e| format!("{e:#}"))?
        .submit_review(model.as_deref(), Some(seat), events)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Poll a review job (`GET /v3/review/{review_id}`). Meta-only — a `done`
/// job answers with its share URL, which is where the result body lives.
#[tauri::command]
pub async fn native_api_review_status(
    base_url: String,
    proxy: Option<String>,
    key: String,
    review_id: String,
) -> CmdResult<crate::bot::api::ReviewJobStatus> {
    crate::bot::api::ApiClient::new(&base_url, &key, proxy.as_deref().unwrap_or(""))
        .map_err(|e| format!("{e:#}"))?
        .review_status(&review_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// (Re-)issue a review's public share link (`POST /v3/review/{id}/share`).
#[tauri::command]
pub async fn native_api_review_share(
    base_url: String,
    proxy: Option<String>,
    key: String,
    review_id: String,
) -> CmdResult<crate::bot::api::ShareIssued> {
    crate::bot::api::ApiClient::new(&base_url, &key, proxy.as_deref().unwrap_or(""))
        .map_err(|e| format!("{e:#}"))?
        .review_share(&review_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// List this key's live share links (`GET /v3/shares`), newest first.
#[tauri::command]
pub async fn native_api_list_shares(
    base_url: String,
    proxy: Option<String>,
    key: String,
) -> CmdResult<Vec<crate::bot::api::ShareEntry>> {
    crate::bot::api::ApiClient::new(&base_url, &key, proxy.as_deref().unwrap_or(""))
        .map_err(|e| format!("{e:#}"))?
        .shares()
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Revoke a share link (`DELETE /v3/shared/{share_id}`). The review stays
/// stored; `native_api_review_share` can re-issue a fresh link later.
#[tauri::command]
pub async fn native_api_revoke_share(
    base_url: String,
    proxy: Option<String>,
    key: String,
    share_id: String,
) -> CmdResult<()> {
    crate::bot::api::ApiClient::new(&base_url, &key, proxy.as_deref().unwrap_or(""))
        .map_err(|e| format!("{e:#}"))?
        .revoke_share(&share_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

// ---------- Self-serve key purchase (PayPal, `/paypal/*`) ----------
//
// Thin passthroughs to `crate::bot::purchase`. The purchase state machine
// (create → open approve_url → poll → redeem/store key) lives in the
// frontend's purchase store; these commands are stateless like the rest of
// the native-API family and all run without auth — the buyer is acquiring
// a key, so they don't hold one yet.

/// Start a one-time purchase (`POST /paypal/create-order`, no auth). Not
/// idempotent — each call opens a fresh PayPal order.
///
/// `redeem: true` has the server turn the prepaid code into an API key
/// itself, so the poll and the buyer's email both carry the key. Pass `false`
/// only to renew an existing key, which needs the raw code for
/// `/v3/redeem`'s `renew_key`.
#[tauri::command]
pub async fn native_api_create_order(
    base_url: String,
    proxy: Option<String>,
    product: String,
    redeem: bool,
) -> CmdResult<crate::bot::purchase::CreatedOrder> {
    crate::bot::purchase::create_order(&base_url, proxy.as_deref().unwrap_or(""), &product, redeem)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Poll a one-time purchase (`POST /paypal/order-result`, no auth).
/// Idempotent; returns `pending` until paid, then `ready` with the API key
/// (`redeem: true` order) or the redeem code (`redeem: false`).
#[tauri::command]
pub async fn native_api_order_result(
    base_url: String,
    proxy: Option<String>,
    order_id: String,
    claim: String,
) -> CmdResult<crate::bot::purchase::OrderResult> {
    crate::bot::purchase::order_result(&base_url, proxy.as_deref().unwrap_or(""), &order_id, &claim)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Start a subscription (`POST /paypal/create-subscription`, no auth). Not
/// idempotent — each call opens a fresh PayPal subscription.
#[tauri::command]
pub async fn native_api_create_subscription(
    base_url: String,
    proxy: Option<String>,
    product: String,
) -> CmdResult<crate::bot::purchase::CreatedSubscription> {
    crate::bot::purchase::create_subscription(&base_url, proxy.as_deref().unwrap_or(""), &product)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Poll a subscription (`POST /paypal/subscription-result`, no auth). On
/// `ready` the response carries the API key directly (no redeem step).
#[tauri::command]
pub async fn native_api_subscription_result(
    base_url: String,
    proxy: Option<String>,
    subscription_id: String,
    claim: String,
) -> CmdResult<crate::bot::purchase::SubscriptionResult> {
    crate::bot::purchase::subscription_result(
        &base_url,
        proxy.as_deref().unwrap_or(""),
        &subscription_id,
        &claim,
    )
    .await
    .map_err(|e| format!("{e:#}"))
}

/// Start a Creem checkout (`POST /creem/create-checkout`, no auth). One
/// endpoint for both one-time and subscription products. Not idempotent —
/// each call opens a fresh checkout.
///
/// `redeem` mirrors `native_api_create_order`: one-time products only,
/// `true` has the poll return the API key directly instead of a redeem code.
#[tauri::command]
pub async fn native_api_create_checkout(
    base_url: String,
    proxy: Option<String>,
    product: String,
    redeem: bool,
) -> CmdResult<crate::bot::purchase::CreatedCheckout> {
    crate::bot::purchase::create_checkout(
        &base_url,
        proxy.as_deref().unwrap_or(""),
        &product,
        redeem,
    )
    .await
    .map_err(|e| format!("{e:#}"))
}

/// Poll a Creem checkout (`POST /creem/result`, no auth). Idempotent; same
/// payload shape as the PayPal order poll — on `ready` a one-time purchase
/// carries the code or key per its `redeem` flag, a subscription carries the
/// key with `days: 0`.
#[tauri::command]
pub async fn native_api_checkout_result(
    base_url: String,
    proxy: Option<String>,
    checkout_id: String,
    claim: String,
) -> CmdResult<crate::bot::purchase::OrderResult> {
    crate::bot::purchase::checkout_result(
        &base_url,
        proxy.as_deref().unwrap_or(""),
        &checkout_id,
        &claim,
    )
    .await
    .map_err(|e| format!("{e:#}"))
}

// ---------------------------------------------------------------------------
// Test mode (inert unless `[test].enabled = true`)
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn test_status(
    state: State<'_, AppState>,
) -> CmdResult<crate::testing::TestStatus> {
    Ok(state.testing.status(state.inner()).await)
}

#[tauri::command]
pub async fn test_start(
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<crate::testing::TestStatus> {
    state
        .testing
        .start(app, state.inner().clone())
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn test_stop(
    state: State<'_, AppState>,
) -> CmdResult<crate::testing::TestStatus> {
    state
        .testing
        .stop(state.inner())
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn test_force_exit(
    slot: Option<String>,
    reason: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<crate::testing::TestStatus> {
    state
        .testing
        .force_exit(slot, reason, state.inner())
        .await
        .map_err(|e| format!("{e:#}"))
}

fn persist_config(config: &AppConfig, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // Merge into whatever is already there rather than rewriting the file:
    // `config.toml` can hold keys this build doesn't declare — another
    // Aogara's, or the user's own notes — and a wholesale rewrite deletes them
    // along with every comment in the file.
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let body = match crate::config::merge_into(config, &existing) {
        Ok(merged) => merged,
        Err(e) => {
            // Not valid TOML, so there is nothing to merge into — and nothing
            // was being read from it either (`load_config` falls back to
            // defaults on a parse error). Rewriting it is what every earlier
            // build did, and it leaves the user with a file that works.
            tracing::warn!(
                "config at {} is not valid TOML ({e}); rewriting it",
                path.display()
            );
            toml::to_string_pretty(config).map_err(std::io::Error::other)?
        }
    };
    std::fs::write(path, body)
}

/// Pre-builds the handler list for `tauri::generate_handler!`. Keep in
/// sync when adding commands.
#[macro_export]
macro_rules! ipc_handlers {
    () => {
        ::tauri::generate_handler![
            $crate::ipc::commands::get_config,
            $crate::ipc::commands::update_config,
            $crate::ipc::commands::set_overlay_enabled,
            $crate::ipc::commands::list_bots,
            $crate::ipc::commands::set_active_bot,
            $crate::ipc::commands::get_bot_settings,
            $crate::ipc::commands::update_bot_settings,
            $crate::ipc::commands::install_bot_from_github,
            $crate::ipc::commands::install_bot_from_zip,
            $crate::ipc::commands::update_bot_from_manifest,
            $crate::ipc::commands::sync_bot_deps,
            $crate::ipc::commands::delete_bot,
            $crate::ipc::commands::start_capture,
            $crate::ipc::commands::stop_capture,
            $crate::ipc::commands::restart_capture,
            $crate::ipc::commands::get_capture_status,
            $crate::ipc::commands::detect_system_chrome,
            $crate::ipc::commands::pool_probe_login,
            $crate::ipc::commands::pool_switch_account,
            $crate::ipc::commands::pool_status,
            $crate::ipc::commands::pool_set_autoplay,
            $crate::ipc::commands::pool_start,
            $crate::ipc::commands::pool_stop,
            $crate::ipc::commands::list_sessions,
            $crate::ipc::commands::get_session_status,
            $crate::ipc::commands::get_session_snapshot,
            $crate::ipc::commands::session_pool_start,
            $crate::ipc::commands::precheck_sessions,
            $crate::ipc::commands::session_pool_stop,
            $crate::ipc::commands::update_session_settings,
            $crate::ipc::commands::setup_add_account,
            $crate::ipc::commands::create_session,
            $crate::ipc::commands::remove_session,
            $crate::ipc::commands::set_pool_auto_next,
            $crate::ipc::commands::session_start_capture,
            $crate::ipc::commands::session_stop_capture,
            $crate::ipc::commands::set_start_capture_on_launch,
            $crate::ipc::commands::list_cft_installed,
            $crate::ipc::commands::download_chrome_for_testing,
            $crate::ipc::commands::remove_chrome_for_testing,
            $crate::ipc::commands::get_status,
            $crate::ipc::commands::get_log_dir,
            $crate::ipc::commands::open_log_folder,
            $crate::ipc::commands::open_external_url,
            $crate::ipc::commands::delay_script_status,
            $crate::ipc::commands::replace_delay_script,
            $crate::ipc::commands::list_log_sessions,
            $crate::ipc::commands::read_log_session,
            $crate::ipc::commands::subscribe_log_events,
            $crate::ipc::commands::read_inspector,
            $crate::ipc::commands::pack_logs,
            $crate::ipc::commands::subscribe_inspector,
            $crate::ipc::commands::get_analysis,
            $crate::ipc::commands::get_game_snapshot,
            $crate::ipc::commands::get_mahgen_view,
            $crate::ipc::commands::compute_bot_hora_score,
            $crate::ipc::commands::list_game_history,
            $crate::ipc::commands::get_game_history_record,
            $crate::ipc::commands::get_game_history_events,
            $crate::ipc::commands::delete_game_history_entry,
            $crate::ipc::commands::check_for_update,
            $crate::ipc::commands::apply_update,
            $crate::ipc::commands::native_api_redeem,
            $crate::ipc::commands::native_api_key_status,
            $crate::ipc::commands::native_api_models,
            $crate::ipc::commands::native_api_health,
            $crate::ipc::commands::native_api_review_history_game,
            $crate::ipc::commands::native_api_review_status,
            $crate::ipc::commands::native_api_review_share,
            $crate::ipc::commands::native_api_list_shares,
            $crate::ipc::commands::native_api_revoke_share,
            $crate::ipc::commands::native_api_create_order,
            $crate::ipc::commands::native_api_order_result,
            $crate::ipc::commands::native_api_create_subscription,
            $crate::ipc::commands::native_api_subscription_result,
            $crate::ipc::commands::native_api_create_checkout,
            $crate::ipc::commands::native_api_checkout_result,
            $crate::ipc::commands::test_status,
            $crate::ipc::commands::test_start,
            $crate::ipc::commands::test_stop,
            $crate::ipc::commands::test_force_exit,
        ]
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Only `http(s)://` may reach the OS opener — anything else could be
    /// abused as a generic process/file launcher. (That a query-string URL
    /// reaches the *browser* — not explorer.exe's Documents fallback — is
    /// the manual half of this regression: opener uses ShellExecuteW.)
    #[tokio::test]
    async fn open_external_url_refuses_non_http_schemes() {
        for url in [
            "file:///C:/Windows",
            "ftp://host/x",
            "javascript:alert(1)",
            "C:\\Users",
            "httpss://not-http",
        ] {
            assert!(
                open_external_url(url.to_string()).await.is_err(),
                "{url} must be refused"
            );
        }
    }

    #[test]
    fn persist_config_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("config.toml");
        let mut cfg = AppConfig::default();
        cfg.bot.active_4p = "mortal".into();
        cfg.bot.active_3p = "mortal_3p".into();
        cfg.proxy.addr = "127.0.0.1:9999".into();

        persist_config(&cfg, &path).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        let back: AppConfig = toml::from_str(&body).unwrap();
        assert_eq!(back.bot.active_4p, "mortal");
        assert_eq!(back.bot.active_3p, "mortal_3p");
        assert_eq!(back.proxy.addr, "127.0.0.1:9999");
    }

    /// Regression: a save used to serialise `AppConfig` over the whole file,
    /// which deleted every section, key and comment this build doesn't
    /// declare — `config.toml` is shared with other builds and hand-edited.
    /// One toggle in Settings was enough to lose them.
    #[test]
    fn persist_config_keeps_keys_this_build_does_not_know() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let existing = "\
# my own notes

[bot]
enabled = true
active_4p = \"mortal\"
some_future_knob = 42

[experimental_v4]
feature = \"on\"
level = 3
";
        std::fs::write(&path, existing).unwrap();

        let mut cfg: AppConfig = toml::from_str(existing).unwrap();
        cfg.bot.active_4p = "mortal_3p_test".into();
        persist_config(&cfg, &path).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("[experimental_v4]\nfeature = \"on\"\nlevel = 3\n"),
            "unknown section was dropped:\n{body}"
        );
        assert!(
            body.contains("some_future_knob = 42"),
            "unknown key inside a known section was dropped:\n{body}"
        );
        assert!(
            body.contains("# my own notes"),
            "comment was eaten:\n{body}"
        );

        let back: AppConfig = toml::from_str(&body).unwrap();
        assert_eq!(back.bot.active_4p, "mortal_3p_test");

        // ...and saving again changes nothing.
        persist_config(&cfg, &path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
    }

    /// A `config.toml` that isn't valid TOML has nothing to preserve — the
    /// save must still go through instead of failing the user's edit.
    #[test]
    fn persist_config_rewrites_an_unparseable_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "this is not = = toml").unwrap();

        let cfg = AppConfig::default();
        persist_config(&cfg, &path).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        toml::from_str::<AppConfig>(&body).expect("rewritten file must parse");
    }

    /// Regression: when a user has `bot.enabled = false` and then flips it to
    /// `true` (e.g. via the wizard or Settings), `update_config` must spawn the
    /// bot manager in-process. Before the fix the manager was never spawned on
    /// that flip — the user had to relaunch the app.
    /// `claim_bot_manager_spawn` is the gate that makes
    /// `update_config` spawn the manager exactly once on that flip.
    #[test]
    fn claim_bot_manager_spawn_fires_once_on_false_to_true_flip() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let flag = AtomicBool::new(false);

        // bot.enabled still false (e.g. user saved an unrelated setting
        // before completing the wizard) — must not claim.
        assert!(!claim_bot_manager_spawn(false, &flag));
        assert!(!flag.load(Ordering::SeqCst));

        // Wizard finishes with bot.enabled = true — claim succeeds.
        assert!(claim_bot_manager_spawn(true, &flag));
        assert!(flag.load(Ordering::SeqCst));

        // Subsequent saves with bot.enabled still true — manager is
        // already running, must not double-spawn.
        assert!(!claim_bot_manager_spawn(true, &flag));
        assert!(flag.load(Ordering::SeqCst));

        // Toggling bot.enabled back to false then forward to true: the
        // running manager survives (no off-switch yet), so we still must
        // not claim a second time.
        assert!(!claim_bot_manager_spawn(false, &flag));
        assert!(!claim_bot_manager_spawn(true, &flag));
        assert!(flag.load(Ordering::SeqCst));
    }

    /// Regression: when startup spawns the manager (because `bot.enabled`
    /// was already true on first config load), the flag is set true
    /// up front. A subsequent `update_config` must observe that and skip
    /// spawning a duplicate manager.
    #[test]
    fn claim_bot_manager_spawn_respects_preset_flag() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let flag = AtomicBool::new(true);
        assert!(!claim_bot_manager_spawn(true, &flag));
        assert!(flag.load(Ordering::SeqCst));
    }

    /// Regression: the first-run wizard's account-creation rules. A missing
    /// coordinate or a wrong selected_id would make a freshly-added account
    /// unable to log in or leave slot1 pointing at nothing.
    #[test]
    fn setup_wizard_add_account_logic() {
        use crate::config::AppConfig;

        // Default coords must be complete.
        let c = super::default_pool_coords();
        for key in [
            "again", "back", "cancel", "confirm", "loginEmail", "loginPassword",
            "loginSubmit", "logout", "logoutConfirm", "ranked", "settings",
        ] {
            assert!(c.get(key).and_then(|v| v.point()).is_some(), "missing point {key}");
        }
        assert!(c.get("modes").and_then(|v| v.table_get("mode4e")).is_some());
        assert!(c.get("rooms").and_then(|v| v.table_get("铜之间")).is_some());

        // First account: becomes account-1 and is auto-selected for slot1.
        let cfg = AppConfig::default();
        let (cfg, id, first, count) =
            super::wizard_add_account(cfg, "Me", " Me@Example.com ", "pw", None, None).unwrap();
        assert_eq!(id, "account-1");
        assert!(first);
        assert_eq!(count, 1);
        assert_eq!(cfg.pool.selected_id, "account-1");
        assert_eq!(cfg.pool.accounts[0].email, "me@example.com"); // lowercased+trimmed
        assert_eq!(cfg.pool.accounts[0].label, "Me");

        // Second account: next id, selected_id unchanged, coords present.
        let (cfg, id2, first2, count2) =
            super::wizard_add_account(cfg, "", "second@example.com", "pw2", None, None).unwrap();
        assert_eq!(id2, "account-2");
        assert!(!first2);
        assert_eq!(count2, 2);
        assert_eq!(cfg.pool.selected_id, "account-1"); // stays on the first
        assert_eq!(cfg.pool.accounts[1].label, "账号2"); // fallback label
        assert!(cfg.pool.accounts[1].coords.contains_key("loginEmail"));

        // Duplicate email (different case) is rejected.
        let err = super::wizard_add_account(cfg, "", "ME@EXAMPLE.COM", "x", None, None);
        assert!(err.is_err());

        // Empty email / password are rejected.
        assert!(super::wizard_add_account(AppConfig::default(), "", "", "pw", None, None).is_err());
        assert!(super::wizard_add_account(AppConfig::default(), "", "a@b.com", "", None, None).is_err());
    }
}
