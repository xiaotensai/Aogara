//! Chromium capture backend.
//!
//! Launches a Chromium-family browser with `--user-data-dir` (so it
//! doesn't collide with the user's existing Chrome) and a remote debugging port,
//! then connects to it via the Chrome DevTools Protocol and intercepts
//! `Network.webSocketFrameReceived/Sent` for binary frames. Frames are
//! routed into the platform [`crate::bridge::Bridge`] just as the
//! hudsucker backend does.
//!
//! No CA cert. No system proxy. The user just plays the game in the
//! Aogara-spawned browser window.

pub mod cdp;
pub mod cft;
pub mod detect;
pub mod launch;
pub mod profile;

use super::{CaptureBackend, CaptureCtx, CaptureDescriptor, CaptureKind, ShutdownToken};
use crate::capture::flow::FlowBridges;
use crate::config::ChromiumConfig;
use crate::game_state::restore::find_latest_unfinished;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};

pub struct ChromiumBackend {
    cfg: ChromiumConfig,
}

impl ChromiumBackend {
    pub fn new(cfg: ChromiumConfig) -> Self {
        Self { cfg }
    }

    /// Resolve the chrome executable. Resolution order:
    /// 1. Explicit `cfg.executable` if set (must exist).
    /// 2. If `force_cft = false`: first auto-detected system browser.
    /// 3. Installed Chrome-for-Testing (latest version, or pinned via
    ///    `cfg.cft_channel` if a literal version is installed).
    /// 4. Error pointing the user at the Settings UI to install CfT.
    fn resolve_executable(&self) -> Result<PathBuf> {
        if !self.cfg.executable.is_empty() {
            let p = PathBuf::from(&self.cfg.executable);
            if !p.exists() {
                anyhow::bail!(
                    "configured chromium executable does not exist: {}",
                    p.display()
                );
            }
            return Ok(p);
        }

        if !self.cfg.force_cft {
            if let Some(b) = detect::detect_system_browsers().into_iter().next() {
                return Ok(b.path);
            }
        }

        let pinned = cft::Channel::parse(&self.cfg.cft_channel);
        if let Some(exe) = cft::installed_executable(&pinned) {
            return Ok(exe);
        }

        anyhow::bail!(
            "no Chromium-family browser detected and no Chrome-for-Testing installed. \
             Open Settings → Capture and click Download to install Chrome for Testing, \
             or set capture.chromium.executable explicitly."
        )
    }
}

#[async_trait]
impl CaptureBackend for ChromiumBackend {
    async fn run(self: Box<Self>, ctx: CaptureCtx, shutdown: ShutdownToken) -> Result<()> {
        let exe = self
            .resolve_executable()
            .context("resolving chromium executable")?;
        let profile_dir = profile::resolve_profile_dir(&self.cfg.user_data_dir)?;
        std::fs::create_dir_all(&profile_dir)
            .with_context(|| format!("creating chromium profile dir {}", profile_dir.display()))?;
        info!(
            "chromium backend starting: exe={} profile={}",
            exe.display(),
            profile_dir.display()
        );

        // Reuse a surviving browser instead of always killing + relaunching.
        // When Aogara is restarted mid-match the game browser (and the live
        // match) is often still running. Killing it here used to drop the
        // player from their game and force a fresh login — so first check for
        // a live DevTools endpoint on this profile and attach to it. When we
        // reattach, the client's authGame/syncGame handshake happened before
        // our subscription, so cdp::run reloads the game tab once to make the
        // client reconnect and re-emit that handshake (see reload_on_attach).
        let existing = launch::probe_existing_endpoint(&profile_dir, &self.cfg.extra_args).await;

        // Only reload when we actually need to take over a live game. A fresh
        // unfinished mjai log (written within the last few minutes) means a
        // match is in progress; reloading then re-emits the authGame/syncGame
        // handshake so autoplay can take over. If the browser is idle in the
        // lobby, reuse it quietly — no pointless page flash.
        let reused = existing.is_some();
        // The log root is per session. Using the old hard-coded `./logs`
        // made a second session see the first session's fresh unfinished
        // log and reload its own idle browser.
        let reload_on_attach =
            reused && find_latest_unfinished(ctx.session.root()).is_some();
        if reused && !reload_on_attach {
            info!("chromium backend: reusing idle browser (no live game) — skipping reload");
        }
        let (cdp_endpoint, mut child): (String, Option<tokio::process::Child>) = if let Some(ep) = existing {
            info!("chromium backend: reusing running browser at {ep} (no relaunch)");
            (ep, None)
        } else {
            // No live browser: reclaim the profile (kill a stale owner on the
            // singleton lock) and launch fresh, exactly as before.
            {
                let pd = profile_dir.clone();
                tokio::task::spawn_blocking(move || profile::reclaim_singleton(&pd))
                    .await
                    .context("reclaim-singleton task panicked")?
                    .with_context(|| {
                        format!("reclaiming chromium profile {}", profile_dir.display())
                    })?;
            }
            let launched =
                launch::spawn(&exe, &profile_dir, &self.cfg).context("launching chromium")?;
            let cdp_endpoint =
                launch::wait_for_devtools_endpoint(&profile_dir, launched.remote_debugging_port)
                    .await
                    .context("reading chromium CDP endpoint (chromium failed to start?)")?;
            (cdp_endpoint, Some(launched.child))
        };
        info!("chromium CDP endpoint: {cdp_endpoint}");

        let mut hooks = ctx
            .autoplay
            .as_ref()
            .map(|a| crate::bridge::BridgeHooks {
                time_budget: Some(a.time_budget.clone()),
                input_watch: Some(a.input_watch.clone()),
                tenhou_state: Some(a.tenhou_state.clone()),
                // Chromium has no injection relay; Riichi City autoplay
                // only runs on the MITM path.
                riichi_inject: None,
                notify: None,
                tenhou_journal: None,
            })
            .unwrap_or_default();
        hooks.notify = Some(ctx.notify_bus.clone());
        hooks.tenhou_journal = Some(crate::bridge::tenhou::journal::new_shared());
        let bridges = Arc::new(FlowBridges::<cdp::FlowKey>::new(
            ctx.session.clone(),
            ctx.platform,
            hooks,
        ));

        let cdp_run = cdp::run(
            &cdp_endpoint,
            bridges.clone(),
            ctx.mjai_bus.clone(),
            ctx.session.inspector(),
            ctx.autoplay.clone(),
            ctx.http.clone(),
            ctx.notify_bus.clone(),
            self.cfg.block_telemetry,
            reload_on_attach,
        );
        let mut cdp_fut = Box::pin(cdp_run);
        let shutdown_fut = shutdown.wait();
        tokio::pin!(shutdown_fut);

        // Race shutdown, the CDP loop, and the spawned process's exit. The
        // shutdown and CDP arms are terminal. The child-exit arm may not be:
        // after a launcher handoff (see below) we merely disable that arm via
        // its `if !child_exited` precondition and loop, so shutdown and CDP
        // keep racing — one set of arms instead of a duplicated inner select.
        let mut child_exited = child.is_none(); // reused browser: no child to watch
        let result = loop {
            tokio::select! {
                biased;
                _ = &mut shutdown_fut => {
                    info!("chromium backend: shutdown requested");
                    // A reused browser (no child at all) or a launcher-handoff
                    // browser (our child already exited and Edge relaunched
                    // itself) leaves `terminate` with nothing to kill — the
                    // window would survive "关浏览器". Ask the live browser to
                    // close itself via CDP instead.
                    if child.is_none() || child_exited {
                        if let Some(port) = launch::endpoint_port(&cdp_endpoint) {
                            if let Err(e) = launch::close_browser_via_cdp(port).await {
                                warn!("chromium backend: CDP close failed: {e:#}");
                            }
                        }
                    }
                    break Ok(());
                }
                r = &mut cdp_fut => {
                    match &r {
                        Ok(()) => info!("chromium backend: CDP loop exited cleanly"),
                        Err(e) => warn!("chromium backend: CDP loop error: {e:#}"),
                    }
                    break r;
                }
                // NOTE: `tokio::select!` evaluates the future expression even
                // when the `if` guard is false. With a reused browser `child`
                // is None, so `child.as_mut().unwrap()` would panic here before
                // the guard is checked, killing the whole capture task (CDP
                // never connects). Use a pending future instead — the guard
                // keeps it from ever being polled.
                status = async {
                    match child.as_mut() {
                        Some(c) => c.wait().await,
                        None => std::future::pending().await,
                    }
                }, if !child_exited => {
                    child_exited = true;
                    // A clean exit of the process we spawned is not necessarily
                    // the browser dying: Edge (and Chrome in some setups) may
                    // relaunch itself with the same arguments and exit the
                    // original process with code 0 — a launcher handoff. The
                    // relaunched browser keeps serving our CDP endpoint, so probe
                    // it before declaring the browser dead.
                    let handoff = matches!(&status, Ok(s) if s.success())
                        && match launch::endpoint_port(&cdp_endpoint) {
                            Some(port) => launch::devtools_http_alive(port).await,
                            None => false,
                        };
                    if handoff {
                        info!(
                            "chromium backend: spawned process exited but the CDP \
                             endpoint is still alive (launcher handoff); continuing"
                        );
                        // Loop again with the child arm disabled.
                    } else {
                        break match status {
                            Ok(s) => {
                                warn!("chromium backend: browser exited (status {s})");
                                Err(anyhow::anyhow!("browser exited unexpectedly: {s}"))
                            }
                            Err(e) => Err(anyhow::anyhow!("child wait error: {e}")),
                        };
                    }
                }
            }
        };

        // Best-effort shutdown of the process WE spawned. When we reused an
        // existing browser there is no child, and we deliberately leave that
        // browser running: the user may be mid-match and it keeps the game
        // connected. After a launcher handoff this only reaps the (already
        // dead) launcher for the same reason.
        if let Some(ref mut c) = child {
            launch::terminate(c).await;
        }
        result
    }

    fn descriptor(&self) -> CaptureDescriptor {
        let label = if self.cfg.executable.is_empty() {
            "auto-detect".to_string()
        } else {
            self.cfg.executable.clone()
        };
        CaptureDescriptor {
            kind: CaptureKind::Chromium,
            label,
        }
    }
}
