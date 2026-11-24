//! Continuous per-browser page-state monitor for test mode.
//!
//! The monitor deliberately keeps a long-lived CDP connection (unlike the
//! command paths, which connect per action). It records URL / readyState /
//! canvas / UI screen / in-game transitions and a heartbeat, and exposes the
//! latest snapshot to the stress runner for phase-based forced exits.

use crate::pool::{probe_ui_screen, GameCdp, UiScreen};
use crate::session::SessionSignals;
use crate::testing::logger::{now, CurrentMatch, TestEvent, TestLogger};
use anyhow::{Context, Result};
use chromiumoxide::Page;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

pub type PageStates = Arc<RwLock<HashMap<String, PageSnapshot>>>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PageSnapshot {
    pub slot: String,
    pub account: String,
    pub url: String,
    pub ready_state: String,
    pub screen: String,
    pub phase: String,
    pub in_game: bool,
    pub finish_count: usize,
    pub updated_at: String,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawProbe {
    href: String,
    ready: String,
    #[serde(rename = "hasCanvas")]
    has_canvas: bool,
}

async fn probe_page(page: &Page) -> Result<RawProbe> {
    let value = page
        .evaluate(
            "(()=>{const c=document.getElementById('unity-canvas')||document.getElementsByTagName('canvas')[0];\
             return {href:location.href,ready:document.readyState,hasCanvas:!!c};})()",
        )
        .await
        .context("probe page url/readyState")?
        .value()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("page probe returned no value"))?;
    serde_json::from_value(value).context("deserialize page probe")
}

fn is_game_url(url: &str) -> bool {
    url.contains("maj-soul.com") || url.contains("mahjongsoul.com")
}

fn phase_for(probe: &RawProbe, screen: UiScreen, in_game: bool) -> &'static str {
    if !is_game_url(&probe.href) || !probe.has_canvas || probe.ready != "complete" {
        return "page_load";
    }
    match screen {
        UiScreen::Login => "login",
        UiScreen::Lobby => "lobby",
        UiScreen::Lecture | UiScreen::Activity => "overlay",
        UiScreen::Game if in_game => "in_game",
        UiScreen::Game => "entering",
        UiScreen::AccountConflict => "account_conflict",
        UiScreen::Unknown if in_game => "in_game",
        UiScreen::Unknown => "unknown",
    }
}

fn current_parts(current: &Option<CurrentMatch>, slot: &str) -> (Option<u32>, Option<String>, Option<String>) {
    let Some(cur) = current.as_ref().filter(|c| c.slot == slot) else {
        return (None, None, None);
    };
    (
        Some(cur.round),
        Some(cur.account.clone()),
        Some(cur.match_id.clone()),
    )
}

fn event_for(
    logger: &TestLogger,
    kind: &str,
    slot: &str,
    account: &str,
    snapshot: &PageSnapshot,
    current: &Option<CurrentMatch>,
) -> Result<()> {
    let (round, current_account, match_id) = current_parts(current, slot);
    let mut ev = TestEvent::new(kind)
        .slot(slot)
        .account(current_account.unwrap_or_else(|| account.to_string()))
        .phase(snapshot.phase.clone())
        .screen(snapshot.screen.clone())
        .url(snapshot.url.clone())
        .ready_state(snapshot.ready_state.clone())
        .in_game(snapshot.in_game)
        .finish_count(snapshot.finish_count);
    if let Some(r) = round {
        ev = ev.round(r);
    }
    if let Some(m) = match_id {
        ev = ev.match_id(m);
    }
    if let Some(e) = &snapshot.error {
        ev = ev.error(e.clone());
    }
    logger.log(ev)
}

#[allow(clippy::too_many_arguments)]
pub async fn run_page_monitor(
    slot: String,
    account: String,
    port: u16,
    interval: Duration,
    heartbeat: Duration,
    logger: Arc<TestLogger>,
    current: Arc<RwLock<Option<CurrentMatch>>>,
    pages: PageStates,
    signals: Arc<SessionSignals>,
    stop: Arc<AtomicBool>,
) {
    let mut cdp: Option<GameCdp> = None;
    let mut last: Option<PageSnapshot> = None;
    let mut last_heartbeat = Instant::now();
    let mut last_connect_error: Option<Instant> = None;
    let mut connected_logged = false;
    let mut closed_logged = false;

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }

        if cdp.is_none() {
            match GameCdp::connect(port).await {
                Ok(c) => {
                    cdp = Some(c);
                    connected_logged = true;
                    closed_logged = false;
                    let _ = logger.log(
                        TestEvent::new("browser_connected")
                            .slot(slot.clone())
                            .account(account.clone())
                            .detail(format!("CDP port {port}")),
                    );
                }
                Err(e) => {
                    if last_connect_error
                        .map(|t| t.elapsed() > Duration::from_secs(10))
                        .unwrap_or(true)
                    {
                        last_connect_error = Some(Instant::now());
                        let _ = logger.log(
                            TestEvent::new("browser_unavailable")
                                .slot(slot.clone())
                                .account(account.clone())
                                .error(format!("{e:#}")),
                        );
                        let mut snap = PageSnapshot {
                            slot: slot.clone(),
                            account: account.clone(),
                            phase: "closed".into(),
                            screen: "closed".into(),
                            updated_at: now(),
                            error: Some(format!("{e:#}")),
                            ..PageSnapshot::default()
                        };
                        snap.updated_at = now();
                        pages.write().await.insert(slot.clone(), snap);
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            }
        }

        let Some(link) = cdp.as_ref() else { continue };
        let probed = probe_page(&link.page).await;
        let snap = match probed {
            Ok(probe) => {
                let sig = signals.snapshot();
                let screen = if probe.has_canvas && probe.ready == "complete" && is_game_url(&probe.href)
                {
                    probe_ui_screen(&link.page).await
                } else {
                    UiScreen::Unknown
                };
                let mut snap = PageSnapshot {
                    slot: slot.clone(),
                    account: account.clone(),
                    url: probe.href.clone(),
                    ready_state: probe.ready.clone(),
                    screen: screen.as_str().to_string(),
                    phase: phase_for(&probe, screen, sig.in_game).to_string(),
                    in_game: sig.in_game,
                    finish_count: sig.finish_count,
                    updated_at: now(),
                    error: None,
                };
                // The settlement screen has no reliable DOM/canvas
                // fingerprint; "was in game, now not, and the screen is not
                // the lobby/login" is the useful phase for random exits.
                if snap.phase == "unknown"
                    && !snap.in_game
                    && (last.as_ref().map(|p| p.in_game).unwrap_or(false)
                        || snap.finish_count > 0)
                {
                    snap.phase = "settlement".into();
                }
                snap
            }
            Err(e) => {
                let err = format!("{e:#}");
                cdp = None;
                if !closed_logged {
                    closed_logged = true;
                    let _ = logger.log(
                        TestEvent::new("page_closed")
                            .slot(slot.clone())
                            .account(account.clone())
                            .error(err.clone()),
                    );
                }
                let snap = PageSnapshot {
                    slot: slot.clone(),
                    account: account.clone(),
                    phase: "closed".into(),
                    screen: "closed".into(),
                    updated_at: now(),
                    error: Some(err),
                    ..PageSnapshot::default()
                };
                pages.write().await.insert(slot.clone(), snap.clone());
                if connected_logged {
                    connected_logged = false;
                }
                last = Some(snap);
                tokio::time::sleep(Duration::from_millis(800)).await;
                continue;
            }
        };

        let current_now = current.read().await.clone();
        let changed = last
            .as_ref()
            .map(|p| {
                p.url != snap.url
                    || p.ready_state != snap.ready_state
                    || p.screen != snap.screen
                    || p.phase != snap.phase
                    || p.in_game != snap.in_game
                    || p.finish_count != snap.finish_count
                    || p.error != snap.error
            })
            .unwrap_or(true);

        if changed {
            if last.as_ref().map(|p| p.url.as_str()) != Some(snap.url.as_str())
                && !snap.url.is_empty()
            {
                let _ = event_for(&logger, "page_navigated", &slot, &account, &snap, &current_now);
            }
            if snap.ready_state == "complete"
                && last.as_ref().map(|p| p.ready_state.as_str()) != Some("complete")
            {
                let _ = event_for(&logger, "page_loaded", &slot, &account, &snap, &current_now);
            } else if last.is_none() && snap.ready_state != "complete" {
                let _ = event_for(&logger, "page_loading", &slot, &account, &snap, &current_now);
            }
            if last.as_ref().map(|p| p.screen.as_str()) != Some(snap.screen.as_str()) {
                let _ = event_for(&logger, "screen_changed", &slot, &account, &snap, &current_now);
            }
            if last.as_ref().map(|p| p.phase.as_str()) != Some(snap.phase.as_str()) {
                let _ = event_for(&logger, "phase_changed", &slot, &account, &snap, &current_now);
            }
            if last
                .as_ref()
                .map(|p| (p.in_game, p.finish_count))
                != Some((snap.in_game, snap.finish_count))
            {
                let _ = event_for(&logger, "page_state", &slot, &account, &snap, &current_now);
            }
            last = Some(snap.clone());
        }

        if heartbeat > Duration::ZERO && last_heartbeat.elapsed() >= heartbeat {
            last_heartbeat = Instant::now();
            let _ = event_for(&logger, "page_heartbeat", &slot, &account, &snap, &current_now);
        }

        pages.write().await.insert(slot.clone(), snap);
        tokio::time::sleep(interval).await;
    }

    if let Some(c) = cdp.take() {
        drop(c);
    }
}
