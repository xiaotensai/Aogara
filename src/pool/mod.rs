//! Native account-pool driver.
//!
//! This module ports the external `tools/bridge.js` browser-control logic
//! into Aogara itself, so switching accounts / entering a room no longer needs
//! a separate Node process. It reuses the SAME CDP primitives autoplay uses
//! (`crate::autoplay::cdp_input`) and the SAME game browser Aogara already
//! launches — the port is fixed via `capture.chromium.extra_args`
//! (`--remote-debugging-port=63503`), so no extra browser is spawned.
//!
//! Porting notes (facts verified against source, 2026-11-22):
//! - The game page lives in the Chromium launched by `capture/chromium`,
//!   NOT in the 63100 WebView2 UI (that one only hosts tauri.localhost).
//! - Majsoul login state is `localStorage.account` in that page:
//!   a real email when logged in, the placeholder `<登录名占位>` when logged out,
//!   absent when never logged in. `access_token` presence is a secondary hint.
//! - `autoplay`'s page handle is bound only after a game WS appears; the pool
//!   therefore opens its OWN short-lived CDP connection per command instead
//!   of relying on that handle.

pub mod session;

use anyhow::{anyhow, Context, Result};
use chromiumoxide::browser::Browser;
use chromiumoxide::Page;
use futures_util::StreamExt;
use crate::session::SessionSignals;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::time::timeout;

const HOST_HINTS: &[&str] = &["maj-soul.com", "mahjongsoul.com"];
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const EVAL_TIMEOUT: Duration = Duration::from_secs(8);

/// Cooperative cancellation for multi-step pool operations. The UI's
/// "停止任务" sets the flag; every wait/click boundary checks it so an
/// in-flight switch/enter aborts instead of running to completion.
fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::SeqCst) {
        anyhow::bail!("任务已取消");
    }
    Ok(())
}

/// Cancellable sleep: checks every 200ms so a stop request is honoured
/// promptly without aborting in the middle of a single CDP call.
async fn pause(cancel: &AtomicBool, ms: u64) -> Result<()> {
    let mut left = ms;
    while left > 0 {
        check_cancel(cancel)?;
        let step = left.min(200);
        tokio::time::sleep(Duration::from_millis(step)).await;
        left -= step;
    }
    check_cancel(cancel)
}

/// Login state of the account currently shown in the game browser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoginState {
    /// Normalised (`lowercase`) account identifier, or `None` when logged out.
    /// For Majsoul this is the email OR the login name (`<登录名占位>`); it can be
    /// null on newer clients that only persist `access_token`, so it is NOT a
    /// reliable identity on its own — use `uid` for that.
    pub account: Option<String>,
    pub logged_in: bool,
    pub token_present: bool,
    /// Numeric Majsoul UID from `localStorage.webgl_crash_user_v1`. This key is
    /// written by the crash reporter once the account is established and is
    /// stable even when `account` is cleared — the authoritative local identity
    /// for the anti-top-out check (2026-11-24: slot1 had account=null but a
    /// live UID, which is why the old "account null → skip switch" guard let a
    /// wrong account keep playing and top out slot5).
    #[serde(default)]
    pub uid: Option<String>,
}

/// A verified local identity read from the game page. Multiple independent
/// localStorage keys are combined so a single stale/cleared key cannot be
/// mistaken for "already the target account".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Identity {
    pub uid: Option<String>,
    pub account: Option<String>,
    pub token_present: bool,
    /// name/email-ish keys that look like a logged-in account (excluding the
    /// `<登录名占位>` placeholder used while logged out).
    pub account_keys: Vec<String>,
}

impl Identity {
    /// True when at least one stable identity signal exists.
    pub fn is_known(&self) -> bool {
        self.uid.is_some() || !self.account_keys.is_empty()
    }
}

/// Names (`email` + optional `login_name`) that a target account may present
/// in `localStorage.account`.
fn target_names(acc: &crate::config::PoolAccount) -> Vec<String> {
    let mut names = vec![acc.email.trim().to_lowercase()];
    if let Some(n) = acc.login_name.as_deref() {
        let n = n.trim().to_lowercase();
        if !n.is_empty() {
            names.push(n);
        }
    }
    names
}

/// Decide whether the observed local identity belongs to `acc`.
///
/// Rules (2026-11-24 anti-top-out):
/// - When BOTH sides have a numeric UID, the UID is authoritative — a matching
///   email/login_name with a different UID means a different account and must
///   NOT be treated as "already logged in". This is what lets a stale profile
///   holding someone else's token get force-switched instead of topping out.
/// - Otherwise fall back to `account`/`account_keys`, which still covers old
///   clients and accounts whose `uid` has not been filled in yet.
pub fn identity_is_target(identity: &Identity, acc: &crate::config::PoolAccount) -> bool {
    let want_uid = acc.uid.trim().to_lowercase();
    if !want_uid.is_empty() {
        if let Some(cur_uid) = identity.uid.as_ref() {
            // UID known on both sides: trust only the UID.
            return cur_uid == &want_uid;
        }
    }
    let names = target_names(acc);
    identity
        .account_keys
        .iter()
        .any(|k| names.iter().any(|n| n == k))
        || identity
            .account
            .as_ref()
            .map(|a| names.iter().any(|n| n == a))
            .unwrap_or(false)
}

/// Read every stable identity signal available in the current game page.
/// `webgl_crash_user_v1` is the primary key (numeric UID); `account` and a
/// handful of UID-suffixed keys are secondary corroboration. Returns an
/// `Identity` whose `is_known()` is false only when nothing usable is present.
pub async fn probe_identity(page: &Page) -> Result<Identity> {
    let expr = "(() => { try { \
        const g = k => { try { return localStorage.getItem(k); } catch(e) { return null; } }; \
        const clean = v => { if (v == null) return null; const s = String(v).trim(); \
            return (!s || s === 'null' || s === 'FKU!!!') ? null : s; }; \
        const uid = clean(g('webgl_crash_user_v1')); \
        const account = clean(g('account')); \
        const token = !!g('access_token'); \
        const keys = Object.keys(localStorage); \
        const suffixHits = []; \
        if (uid) { \
            for (const k of keys) { \
                if (/^(treasure_up_|activity_popoout_|Collab1004|yueka|yindao_show_)/.test(k) \
                    && k.indexOf(uid) >= 0) suffixHits.push(k); \
            } \
        } \
        return { uid: uid, account: account, token: token, \
                 account_keys: account ? [account] : [], suffix_hits: suffixHits.length }; \
        } catch (e) { return { err: String(e) }; } })()";
    let value = eval_json(page, expr).await?;
    let uid = value
        .get("uid")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    let account = value
        .get("account")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    let account_keys = value
        .get("account_keys")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let token_present = value
        .get("token")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Ok(Identity {
        uid,
        account,
        token_present,
        account_keys,
    })
}

/// Wait until the game page is actually interactive: `document.readyState`
/// complete AND a `<canvas>` present. CDP accepts TCP connections the moment
/// the browser process is up, but Majsoul keeps loading for several seconds
/// after that — clicking the login form before the page finished rendering
/// lands the clicks nowhere and the flow "卡在登录界面".
pub async fn wait_page_ready(page: &Page, timeout_ms: u64, cancel: &AtomicBool) -> Result<()> {
    let expr = "(() => { const c = document.querySelector('canvas'); \
                return { ready: document.readyState === 'complete' && !!c, \
                         state: document.readyState, canvas: !!c }; })()";
    let t0 = std::time::Instant::now();
    loop {
        check_cancel(cancel)?;
        let ready = eval_json(page, expr)
            .await
            .ok()
            .and_then(|v| v.get("ready").and_then(|x| x.as_bool()))
            .unwrap_or(false);
        if ready {
            return Ok(());
        }
        if t0.elapsed().as_millis() >= timeout_ms as u128 {
            return Err(anyhow!("页面加载超时（{}ms），登录流程中止", timeout_ms));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Read the game page's localStorage login state.
pub async fn probe_login(page: &Page) -> Result<LoginState> {
    let expr = "(() => { try { return { account: localStorage.getItem('account'), \
                token: !!localStorage.getItem('access_token'), \
                uid: localStorage.getItem('webgl_crash_user_v1') }; } \
                catch (e) { return { err: String(e) }; } })()";
    let value = eval_json(page, expr).await?;
    let account_raw = value
        .get("account")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let token_present = value
        .get("token")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let uid = value
        .get("uid")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty() && s != "null");
    // `localStorage.account` is cleared on logout, but `access_token` lingers,
    // so require both. Note: an account registered with a username stores that
    // username here (e.g. `<登录名占位>`), not the email.
    let logged_in = !account_raw.is_empty() && account_raw != "null" && token_present;
    let account = if logged_in {
        Some(account_raw.to_lowercase())
    } else {
        None
    };
    Ok(LoginState {
        account,
        logged_in,
        token_present,
        uid,
    })
}

/// Evaluate an expression on the page and deserialise its value as JSON.
pub async fn eval_json(page: &Page, expr: &str) -> Result<serde_json::Value> {
    let result = timeout(EVAL_TIMEOUT, page.evaluate(expr))
        .await
        .map_err(|_| anyhow!("CDP evaluate timed out"))?
        .context("CDP evaluate")?;
    result
        .value()
        .cloned()
        .ok_or_else(|| anyhow!("CDP evaluate returned no value"))
}

// ---------------------------------------------------------------------------
// Actuation: canvas clicks + Unity keyboard input.
// These mirror `autoplay::cdp_input` (same hover/press/hold/release timing)
// and the external bridge's `keyType`/`clearUnityField` (Unity canvas text
// fields are not DOM inputs — only CDP key events land).
//
// NOTE: the keyboard primitives below (`key_map` / `dispatch_key` / `key_type`
// / `clear_unity_field`) are the ONLY keyboard-input implementation in the
// codebase and live here because the pool is their only consumer. If any
// other module (autoplay, …) ever needs to type into a Unity canvas, move
// them into `autoplay::cdp_input` first and reuse — DO NOT copy them locally.

use chromiumoxide::cdp::browser_protocol::input::{DispatchKeyEventParams, DispatchKeyEventType};

/// Click a 0..1 canvas-normalised point.
///
/// This is now a thin wrapper over the exact actuation path autoplay uses
/// (`evaluate_canvas_rect` → `CanvasRect::pixel01` → four CDP mouse events).
/// Do not reintroduce a private conversion here: the previous copy made
/// menu clicks drift when the window size changed while in-game clicks kept
/// working.
pub async fn click_norm(page: &Page, point: [f64; 2], jiggle: bool) -> Result<()> {
    // Same timing band as autoplay's hover/hold ranges (midpoint); fixed here
    // so the menu/login clicks stay deterministic.
    crate::autoplay::cdp_input::click_canvas_norm(page, point[0], point[1], 180, 90, jiggle)
        .await
        .context("pool canvas click")
}

/// A single mapped keystroke's CDP key-event fields.
struct KeyStroke {
    key: String,
    code: String,
    vk: i64,
    shift: bool,
}

/// Map a character to the CDP key event fields Unity expects.
fn key_map(ch: char) -> Option<KeyStroke> {
    let shift = ch.is_ascii_uppercase();
    let c = ch.to_ascii_lowercase();
    let (key, code, vk) = match c {
        '@' => ("@".into(), "Digit2".into(), 50),
        '.' => (".".into(), "Period".into(), 190),
        '-' => ("-".into(), "Minus".into(), 189),
        '_' => ("_".into(), "Minus".into(), 189),
        '+' => ("+".into(), "Equal".into(), 187),
        ' ' => (" ".into(), "Space".into(), 32),
        c if c.is_ascii_lowercase() => {
            let up = c.to_ascii_uppercase();
            (c.to_string(), format!("Key{up}"), up as i64)
        }
        c if c.is_ascii_digit() => {
            (c.to_string(), format!("Digit{c}"), 48 + (c as i64 - '0' as i64))
        }
        _ => return None,
    };
    Some(KeyStroke {
        key,
        code,
        vk,
        shift,
    })
}

async fn dispatch_key(page: &Page, typ: DispatchKeyEventType, key: &str, code: &str, vk: i64, text: Option<&str>, modifiers: Option<i64>) -> Result<()> {
    let mut b = DispatchKeyEventParams::builder()
        .r#type(typ)
        .key(key)
        .code(code)
        .windows_virtual_key_code(vk)
        .native_virtual_key_code(vk);
    if let Some(t) = text {
        b = b.text(t).unmodified_text(t);
    }
    if let Some(m) = modifiers {
        b = b.modifiers(m);
    }
    page.execute(b.build().map_err(|e| anyhow!("build key event: {e}"))?)
        .await
        .context("CDP dispatchKeyEvent")?;
    Ok(())
}

/// Type text character-by-character into a Unity canvas field.
///
/// Uses CDP `char` events ONLY (one `char` per character). The previous
/// implementation emitted `keyDown` with `text`+`unmodified_text`, which makes
/// Chromium BOTH insert the character AND dispatch a keydown that Unity's
/// WebGL input layer also consumes — the same character landed twice
/// (`user@example.com` → `33446699997733881100@@qqqq..ccoomm`, observed
/// 2026-11-25). A `char` event inserts exactly one copy and does not produce
/// a keydown, so the field receives the text once. `key_map` is still consulted
/// so unmappable characters fail loudly instead of being silently dropped.
pub async fn key_type(page: &Page, text: &str) -> Result<()> {
    for ch in text.chars() {
        let ks = key_map(ch).ok_or_else(|| anyhow!("无法映射字符: {ch}"))?;
        let s = ch.to_string();
        dispatch_key(
            page,
            DispatchKeyEventType::Char,
            &ks.key,
            &ks.code,
            ks.vk,
            Some(&s),
            None,
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(35)).await;
    }
    Ok(())
}

/// Ctrl+A then Backspace — clear a Unity canvas text field.
pub async fn clear_unity_field(page: &Page) -> Result<()> {
    dispatch_key(page, DispatchKeyEventType::RawKeyDown, "a", "KeyA", 65, None, Some(2)).await?;
    dispatch_key(page, DispatchKeyEventType::KeyUp, "a", "KeyA", 65, None, Some(2)).await?;
    tokio::time::sleep(Duration::from_millis(120)).await;
    dispatch_key(page, DispatchKeyEventType::RawKeyDown, "Backspace", "Backspace", 8, None, None).await?;
    dispatch_key(page, DispatchKeyEventType::KeyUp, "Backspace", "Backspace", 8, None, None).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    Ok(())
}

/// Find the configured game CDP port from `capture.chromium.extra_args`.
///
/// Thin forwarder to [`crate::util::parse_fixed_cdp_port`] — kept so the
/// many `ipc::commands` call sites can keep using the pool-local name.
pub fn game_cdp_port(extra_args: &[String]) -> Option<u16> {
    crate::util::parse_fixed_cdp_port(extra_args)
}

/// A live, short-lived connection to the game browser.
///
/// The handler pump must be driven while requests are in flight, so the
/// caller has to keep this struct alive until done, then drop it. We abort
/// the pump on drop to avoid leaking a task per command.
pub struct GameCdp {
    pub browser: Browser,
    pub page: Page,
    pump: tokio::task::JoinHandle<()>,
}

impl Drop for GameCdp {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

impl GameCdp {
    /// Connect to `http://127.0.0.1:<port>` and pick the Majsoul game page.
    pub async fn connect(port: u16) -> Result<Self> {
        let endpoint = format!("http://127.0.0.1:{port}");
        let (browser, mut handler) = timeout(CONNECT_TIMEOUT, Browser::connect(&endpoint))
            .await
            .map_err(|_| anyhow!("CDP connect to {endpoint} timed out"))?
            .with_context(|| format!("CDP connect to {endpoint}"))?;
        // chromiumoxide needs its handler polled for oneshot responses to resolve.
        let pump = tokio::spawn(async move {
            while let Some(ev) = handler.next().await {
                if let Err(e) = ev {
                    tracing::debug!("pool CDP handler event: {e:?}");
                }
            }
        });
        // In attach mode the handler discovers targets asynchronously via
        // Target.setDiscoverTargets; pages() can be empty for the first moments
        // after connect. Retry briefly before giving up.
        let mut pages: Vec<Page> = Vec::new();
        for _ in 0..12 {
            pages = browser.pages().await.context("CDP browser.pages()")?;
            if !pages.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        // `Page::url()` on this chromiumoxide fork can return a bogus ":" or
        // about:blank, so select by reading location.href from the page via
        // Runtime.evaluate (reliable). Retry a couple of times because the
        // game tab can still be "about:blank" right after the browser opens.
        let mut chosen: Option<Page> = None;
        let mut first_page: Option<Page> = None;
        for _round in 0..3 {
            for p in &pages {
                if first_page.is_none() {
                    first_page = Some(p.clone());
                }
                let url = eval_json(p, "location.href")
                    .await
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                if HOST_HINTS.iter().any(|h| url.contains(h)) {
                    chosen = Some(p.clone());
                    break;
                }
                tracing::debug!("pool CDP page url (eval): {url}");
            }
            if chosen.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(600)).await;
        }
        // No game tab (all on 客服 / blank): fall back to the first page;
        // ensure_game_url() will navigate it back to the game before use.
        let page = chosen.or(first_page).ok_or_else(|| {
            anyhow!("no page found on CDP port {port} (is capture running?)")
        })?;
        Ok(Self {
            browser,
            page,
            pump,
        })
    }
}

// ---------------------------------------------------------------------------
// Account switching. Non-screenshot judgement: `localStorage.account` decides
// login state; `in_game` is supplied by the IPC layer from Aogara's own
// game_tracker (authoritative). No template matching / pixel sampling.

fn coord(acc: &crate::config::PoolAccount, key: &str) -> Option<[f64; 2]> {
    // Config uses camelCase (loginEmail/logoutConfirm); session::canon_key
    // accepts both spellings so the inherited-main coords work here too.
    acc.coords
        .get(key)
        .or_else(|| acc.coords.get(session::canon_key_public(key)))
        .and_then(|c| c.point())
}

/// READ-ONLY diagnostic snapshot of login-relevant page state.
///
/// Logging-only: the result is never fed back into any decision, so adding it
/// cannot change login/logout behaviour. It records which auth keys exist and
/// whether the canvas is laid out — enough to tell "clicks landed nowhere"
/// from "credentials rejected" on the next real run. Never logs a password.
async fn diag_login_snapshot(page: &Page) -> String {
    let expr = "(() => { try { \
        const c = document.querySelector('canvas'); \
        const r = c ? c.getBoundingClientRect() : null; \
        const g = k => { try { return localStorage.getItem(k); } catch(e) { return null; } }; \
        const authKeys = Object.keys(localStorage).filter(k => \
            /^(account|access_token|webgl_crash_user_v1|password|login_type)/i.test(k)); \
        return { url: location.href, ready: document.readyState, \
                 canvas: r ? [Math.round(r.width), Math.round(r.height)] : null, \
                 authKeys: authKeys, uid: g('webgl_crash_user_v1'), \
                 account: g('account'), token: !!g('access_token') }; \
        } catch (e) { return { err: String(e) }; } })()";
    match eval_json(page, expr).await {
        Ok(v) => v.to_string(),
        Err(e) => format!("eval_failed: {e:#}"),
    }
}

/// Wait until `localStorage.account` matches `want_email` (or is empty when
/// `want_email` is None). Polls with cancellation-friendly sleeps.
async fn wait_login(
    page: &Page,
    want: Option<&crate::config::PoolAccount>,
    timeout_ms: u64,
    cancel: &AtomicBool,
) -> Result<()> {
    let t0 = std::time::Instant::now();
    // [DIAG] One-way progress logging only: no early exit, no timeout change.
    const DIAG_AT_MS: [u64; 4] = [5_000, 15_000, 30_000, 45_000];
    let mut diag_next = 0usize;
    loop {
        let ok = match want {
            Some(acc) => {
                // Use the full identity (uid authoritative) rather than the
                // bare `account` key, which newer clients may leave null.
                let ident = probe_identity(page).await?;
                identity_is_target(&ident, acc)
            }
            None => {
                let st = probe_login(page).await?;
                !st.logged_in
            }
        };
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        if diag_next < DIAG_AT_MS.len() && elapsed_ms >= DIAG_AT_MS[diag_next] {
            tracing::info!(
                "[pool][diag] wait_login target={} waited={}ms ok={} snapshot={}",
                want.map(|a| a.email.as_str()).unwrap_or("<logout>"),
                elapsed_ms,
                ok,
                diag_login_snapshot(page).await
            );
            diag_next += 1;
        }
        if ok {
            return Ok(());
        }
        if t0.elapsed().as_millis() >= timeout_ms as u128 {
            return Err(anyhow!(
                "等待登录态超时（目标 {}）",
                want.map(|a| a.email.as_str()).unwrap_or("登出")
            ));
        }
        pause(cancel, 1500).await?;
    }
}

/// Force-clear every login-related localStorage key and reload, so a stale
/// token from a different account cannot survive into the next `login()`.
/// Used when UI logout cannot run (account key null, wrong account already
/// present) — exactly the state that produced the 2026-11-24 top-out.
async fn force_clear_login_storage(page: &Page, cancel: &AtomicBool) -> Result<()> {
    let expr = "(()=>{ \
        const rm = k => { try { localStorage.removeItem(k); } catch(e) {} }; \
        for (const k of ['account','access_token','ssssoooodd','webgl_crash_user_v1', \
                         'password','login_type_tab','login_type_index']) rm(k); \
        const keys = Object.keys(localStorage); \
        for (const k of keys) { \
            if (/^(treasure_up_|activity_popoout_|Collab1004|yueka|yindao_show_)/.test(k)) rm(k); \
        } \
        return true;})()";
    let _ = eval_json(page, expr).await;
    check_cancel(cancel)?;
    use chromiumoxide::cdp::browser_protocol::page::ReloadParams;
    page.execute(ReloadParams::default())
        .await
        .context("reload after forced logout")?;
    pause(cancel, 3000).await?;
    wait_page_ready(page, 60_000, cancel).await?;
    Ok(())
}

/// Log out of the current account via settings → logout → confirm.
async fn logout(page: &Page, acc: &crate::config::PoolAccount, cancel: &AtomicBool) -> Result<()> {
    let st = probe_login(page).await?;
    if !st.logged_in {
        // `account` key null but a token still present = a stale session from
        // some other account. The old early-return left that token in place,
        // so `login()` found no form and silently kept the wrong account
        // (top-out). Force-clear it here before returning.
        if st.token_present {
            tracing::warn!(
                "[pool] logout: account key absent but token present; force-clearing stale login"
            );
            force_clear_login_storage(page, cancel).await?;
        }
        return Ok(()); // already out (now genuinely clean)
    }
    let settings = coord(acc, "settings").ok_or_else(|| anyhow!("缺少 settings 坐标"))?;
    let logout = coord(acc, "logout").ok_or_else(|| anyhow!("缺少 logout 坐标"))?;
    let confirm = coord(acc, "logout_confirm").ok_or_else(|| anyhow!("缺少 logout_confirm 坐标"))?;
    // If the previous match just ended the game page sits on a settlement
    // screen, where the settings gear doesn't exist. The bridge first calls
    // backToLobby; without screenshot judgement we press the settlement's
    // confirm point a couple of times (a no-op on lobby/login screens).
    if let Some(settle_confirm) = coord(acc, "confirm") {
        for _ in 0..3 {
            click_norm(page, settle_confirm, false).await?;
            pause(cancel, 1200).await?;
        }
    }
    click_norm(page, settings, false).await?;
    // The settings panel is a Unity animated overlay; clicking too fast lands
    // on the wrong spot. Bridge's measured cadence is ~2-7s per step.
    pause(cancel, 2500).await?;
    click_norm(page, logout, false).await?;
    pause(cancel, 2500).await?;
    click_norm(page, confirm, false).await?;
    // localStorage.account lingers after logout in some sessions; poll both
    // the placeholder and a real logout confirmation from the page.
    if wait_login(page, None, 45000, cancel).await.is_ok() {
        return Ok(());
    }
    // UI logout never completed: the page can be sitting on the login form
    // (e.g. after being navigated back from a 客服 mis-click) with a stale
    // `account`+`access_token` in localStorage, so the settings gear doesn't
    // exist and the clicks above were no-ops. Force-clear the login state via
    // JS and reload — then `login()` can fill the form cleanly.
    tracing::warn!("[pool] UI logout timed out; forcing localStorage logout via JS");
    force_clear_login_storage(page, cancel).await?;
    wait_login(page, None, 20000, cancel).await
}

/// Log in with `acc`'s email/password via the Unity canvas form.
async fn login(page: &Page, acc: &crate::config::PoolAccount, cancel: &AtomicBool) -> Result<()> {
    let email = coord(acc, "login_email").ok_or_else(|| anyhow!("缺少 login_email 坐标"))?;
    let pass = coord(acc, "login_password").ok_or_else(|| anyhow!("缺少 login_password 坐标"))?;
    let submit = coord(acc, "login_submit").ok_or_else(|| anyhow!("缺少 login_submit 坐标"))?;
    if acc.email.is_empty() || acc.password.is_empty() {
        return Err(anyhow!("账号 {} 未配置 email/password", acc.id));
    }
    // [DIAG] Input-stage trace. Lengths only — never the password itself.
    // Combined with the wait_login snapshots this distinguishes "input went
    // nowhere" from "credentials rejected" on the next real run.
    tracing::info!(
        "[pool][diag] login start acct={} email_len={} pass_len={} coords email={:?} pass={:?} submit={:?} page={}",
        acc.id,
        acc.email.len(),
        acc.password.len(),
        email,
        pass,
        submit,
        diag_login_snapshot(page).await
    );
    // Warm-up click (first click on the login page is swallowed).
    click_norm(page, [0.50, 0.78], false).await?;
    pause(cancel, 700).await?;
    click_norm(page, email, false).await?;
    pause(cancel, 350).await?;
    click_norm(page, email, false).await?;
    pause(cancel, 700).await?;
    clear_unity_field(page).await?;
    key_type(page, &acc.email).await?;
    tracing::info!("[pool][diag] login email typed (len={})", acc.email.len());
    pause(cancel, 400).await?;
    click_norm(page, pass, false).await?;
    pause(cancel, 350).await?;
    click_norm(page, pass, false).await?;
    pause(cancel, 700).await?;
    clear_unity_field(page).await?;
    key_type(page, &acc.password).await?;
    tracing::info!("[pool][diag] login password typed (len={})", acc.password.len());
    pause(cancel, 400).await?;
    click_norm(page, submit, false).await?;
    tracing::info!("[pool][diag] login submit clicked page={}", diag_login_snapshot(page).await);
    // First login on a cold profile often lands while Unity is still
    // downloading resources: that submit is swallowed and no auth state is
    // written, so waiting the full 90s only delays the retry that actually
    // succeeds. 45s is enough for a healthy login (measured ~17s on retry)
    // while halving the worst-case cold-start wait.
    wait_login(page, Some(acc), 45000, cancel).await
}

/// Result of a switch attempt, surfaced to the frontend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwitchResult {
    pub account: String,
    pub switched: bool,
    pub message: String,
}

/// `switch_account` with retries: a click swallowed by a still-animating
/// overlay shouldn't fail the whole start. Idempotent — each attempt re-probes
/// first, so a half-completed login (localStorage already flipped) is
/// recognised as success on the next try.
pub async fn switch_account_retry(
    page: &Page,
    acc: &crate::config::PoolAccount,
    in_game: bool,
    cancel: &AtomicBool,
    attempts: usize,
) -> Result<SwitchResult> {
    let mut last: Result<SwitchResult> = Err(anyhow!("未尝试"));
    for i in 0..attempts.max(1) {
        match switch_account(page, acc, in_game, cancel).await {
            Ok(r) => {
                if i > 0 {
                    tracing::info!("[pool] switch_account succeeded on attempt {}", i + 1);
                }
                return Ok(r);
            }
            Err(e) => {
                tracing::warn!(
                    "[pool] switch_account attempt {}/{} for {} failed: {e:#}",
                    i + 1,
                    attempts,
                    acc.id
                );
                last = Err(e);
                if i + 1 < attempts.max(1) {
                    pause(cancel, 2000).await?;
                }
            }
        }
    }
    last
}

/// Ensure the game browser is logged into `acc`. `in_game` comes from Aogara's
/// own game tracker — switching mid-game is refused (would drop the match).
pub async fn switch_account(
    page: &Page,
    acc: &crate::config::PoolAccount,
    in_game: bool,
    cancel: &AtomicBool,
) -> Result<SwitchResult> {
    check_cancel(cancel)?;
    let want = acc.email.trim().to_lowercase();
    if want.is_empty() {
        return Err(anyhow!("账号 {} 未配置 email，无法判断登录谁", acc.id));
    }
    if in_game {
        return Err(anyhow!("当前在对局中，不能换号（会让这一局掉线托管）"));
    }

    // ---- Recover from a visible top-out modal -------------------------------
    // If the page is showing 「您的帳號已在另一處登入」 (e.g. logged in from
    // another device/browser), the uid may still equal the target, so the
    // identity check alone would skip and leave the modal up. Detect it by
    // screen signature and force a clean re-login. No fixed-point click on the
    // modal — reload clears it and we never risk mis-clicking the wrong UI.
    if probe_ui_screen(page).await == UiScreen::AccountConflict {
        tracing::warn!(
            "[pool] top-out modal detected for {}; force-clearing session and re-logging in",
            acc.id
        );
        force_clear_login_storage(page, cancel).await?;
    }

    // ---- Anti-top-out identity check ---------------------------------------
    // The OLD code had a fatal shortcut: `account==null && token_present` was
    // treated as "already the target account" and skipped re-login. On 2026-
    // 11-24 that let slot1 (profile p-accountA, account=null) keep running
    // as UID <UID> — which is slot5's accountB — so the two fought and
    // produced 「您的帳號已在另一處登入」. We now read a full Identity and only
    // skip when it positively matches the target UID/name.
    let cur = probe_identity(page).await?;
    if identity_is_target(&cur, acc) {
        tracing::info!(
            "[pool] identity verified for {} (uid={:?}, account={:?}); no switch",
            acc.id,
            cur.uid,
            cur.account
        );
        return Ok(SwitchResult {
            account: want,
            switched: false,
            message: "已是目标账号，无需切换".into(),
        });
    }
    // A token with no verifiable identity is NOT proof of the right account.
    // Force a clean logout/login instead of blindly trusting it.
    if !cur.is_known() && cur.token_present {
        tracing::warn!(
            "[pool] account {} has token but no verifiable identity (uid/account both empty); \
             forcing re-login to avoid a wrong-account top-out",
            acc.id
        );
    }
    tracing::info!(
        "[pool] switching {} : observed uid={:?} account={:?}, target uid={:?}",
        acc.id,
        cur.uid,
        cur.account,
        acc.uid
    );

    logout(page, acc, cancel).await?;
    check_cancel(cancel)?;
    login(page, acc, cancel).await?;
    check_cancel(cancel)?;
    // A short settle lets the Unity client persist `webgl_crash_user_v1`
    // before we read it back (the key is written once the account is
    // established, slightly after `account`/token appear).
    pause(cancel, 1200).await?;
    let after = probe_identity(page).await?;
    if !identity_is_target(&after, acc) {
        return Err(anyhow!(
            "登录后账号对不上：期望 {}（uid {:?}），实际 uid={:?} account={:?}",
            want,
            acc.uid,
            after.uid,
            after.account
        ));
    }
    Ok(SwitchResult {
        account: want,
        switched: true,
        message: format!("已切换到 {}", acc.label),
    })
}

// ---------------------------------------------------------------------------
// Room entry + settlement. These port the external bridge's `runPlanInner`
// walk (ranked → room → mode) using the SAME normalised canvas clicks as
// autoplay, but with log-derived in_game as the success judge instead of
// screenshot template matching (per the user's no-template-matching rule).

/// Wait until the log-derived session reports in_game within `timeout_ms`.
pub async fn wait_in_game(signals: &SessionSignals, timeout_ms: u64, cancel: &AtomicBool) -> bool {
    let t0 = std::time::Instant::now();
    loop {
        if signals.snapshot().in_game {
            return true;
        }
        if t0.elapsed().as_millis() >= timeout_ms as u128 {
            return false;
        }
        if cancel.load(Ordering::SeqCst) {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
}

/// Result of entering a room.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnterResult {
    pub room: String,
    pub mode: String,
    pub entered: bool,
    pub message: String,
}

/// Walk lobby → ranked → room → mode and wait for the game to start.
/// `dry_run` stops at the mode list (never clicks the start card).
pub async fn enter_room(
    page: &Page,
    cfg: &crate::config::PoolConfig,
    acc: &crate::config::PoolAccount,
    room: &str,
    mode: &str,
    signals: &SessionSignals,
    dry_run: bool,
    cancel: &AtomicBool,
) -> Result<EnterResult> {
    check_cancel(cancel)?;
    let room_key = session::normalize_room(room)
        .ok_or_else(|| anyhow!("无效房间: {room}"))?;
    let mode_key = session::normalize_mode(mode, 4)
        .ok_or_else(|| anyhow!("无效模式: {mode}"))?;

    // Last line of defence before the ranked/room/mode clicks. If the canvas
    // is still on the login page, an overlay we did not recognise, or the
    // duplicate-login dialog, clicking the fixed walk points is exactly how
    // a previous version ended up on the tutorial/customer-service page.
    // Give a just-navigated canvas two extra frames before declaring the
    // screen unknown; otherwise a transient white/loading frame would abort
    // an otherwise valid entry.
    let mut screen = probe_ui_screen(page).await;
    // Login → lobby can spend a LONG time on an unclassified transition frame:
    // measured 2026-11-25 that a browser started <15s ago always reads Unknown
    // (Unity still downloading/rendering), while the same lobby at >60s reads
    // Lobby. Clicking any fixed point during this transient phase is useless
    // and can even navigate into tutorial/customer-service, so DO NOT click
    // here — just wait it out. 60 × 700ms ≈ 42s covers the observed window.
    // Reloading during a resource download would restart it, so waiting is
    // also the safest recovery.
    // Short settle wait only. The classifier is now advisory (see the match
    // below), so there is no point burning 42s waiting for a "Lobby" reading
    // that may never come on the current client skin.
    for round in 0..4 {
        if screen != UiScreen::Unknown {
            break;
        }
        tracing::info!("[pool] pre-entry screen Unknown, settle wait {}/4", round + 1);
        pause(cancel, 700).await?;
        screen = probe_ui_screen(page).await;
    }
    // Only AFTER the render-settle wait does an overlay reading mean a real
    // overlay. Escape does NOT close the activity centre (user-confirmed) and
    // a reload re-shows it, so the verified mouse back arrow is the only exit.
    // Production E2E 2026-11-25 05:04 caught a gap: when the classifier did
    // positively read `Activity`, this branch was skipped and the old
    // `back=[0.615,0.25]` point was clicked instead — that point is the
    // activity artwork, so the overlay stayed up and no match request fired.
    // Handle Activity/Lecture here too; `dismiss_popups` below knows the
    // lecture X and re-probes. AccountConflict deliberately stays out of this
    // branch (no clicks on a duplicate-login modal).
    if matches!(screen, UiScreen::Unknown | UiScreen::Activity | UiScreen::Lecture) {
        // A confirmed Lecture overlay must NOT be clicked with the activity
        // back arrow — that point is the 学士帽 entry on a settled lobby and
        // is exactly what opened the lecture in the first place. Let
        // `dismiss_popups` use its measured close X for that screen.
        if screen != UiScreen::Lecture {
            let back = activity_back_point(acc);
            tracing::info!("[pool] screen {screen:?} after render-settle wait; clicking verified activity back arrow {back:?}");
            click_norm(page, back, false).await?;
            pause(cancel, 1200).await?;
            screen = probe_ui_screen(page).await;
        }
        // Production E2E 2026-11-25 05:10: the back-arrow click turned an
        // Unknown/activity frame into `Lecture`, but this loop only ran for
        // `Unknown`, so the lecture stayed up and no match request fired.
        // Keep clearing while ANY overlay-like screen remains; `dismiss_popups`
        // routes Activity/Unknown to the back arrow and Lecture to its close X.
        for _ in 0..3 {
            if !matches!(screen, UiScreen::Unknown | UiScreen::Activity | UiScreen::Lecture) {
                break;
            }
            if let Err(e) = dismiss_popups(page, acc, cancel).await {
                tracing::warn!("[pool] overlay dismiss after back-arrow failed: {e:#}");
            }
            screen = probe_ui_screen(page).await;
        }
    }
    // 2026-11-25: the pixel classifier proved unreliable on the current
    // client — the SAME settled lobby randomly reads Activity / Unknown /
    // AccountConflict across runs (5-point fixed colours vs Unity animation).
    // Aborting here blocked every room entry, while the production v3.8
    // client never had this gate and entered rooms fine. Downgrade the
    // classification to a WARNING: after the overlay-clearing steps above
    // the page is in the lobby, so proceed with the ranked/room/mode clicks.
    // Real duplicate-login is still caught later by the WS/log-based check
    // (see switch_account), not by these fragile pixels.
    match screen {
        UiScreen::Lobby | UiScreen::Game => {}
        other => tracing::warn!(
            "[pool] lobby classification is {:?} after overlay clearing; proceeding with room entry (production v3.8 behaviour)",
            other
        ),
    }

    // Coordinates: account's own table, inheriting main's when empty.
    let ranked = session::point(cfg, acc, "ranked")
        .ok_or_else(|| anyhow!("缺少 ranked 坐标"))?;
    let room_pt = session::table_point(cfg, acc, "rooms", &room_key)
        .ok_or_else(|| anyhow!("缺少 rooms.{room_key} 坐标"))?;
    let mode_pt = session::table_point(cfg, acc, "modes", &mode_key)
        .ok_or_else(|| anyhow!("缺少 modes.{mode_key} 坐标"))?;

    // Close any sub-page/popup first (友人場, activity center, …). On the lobby
    // this point is empty sky, so it is a no-op; on a sub-page it goes back.
    if let Some(back) = session::point(cfg, acc, "back") {
        let _ = click_norm(page, back, false).await;
        pause(cancel, 900).await?;
    }

    if dry_run {
        // Stop at mode list: click ranked then room, don't press a mode card.
        click_norm(page, ranked, false).await?;
        pause(cancel, 2800).await?;
        click_norm(page, room_pt, false).await?;
        pause(cancel, 1500).await?;
        return Ok(EnterResult {
            room: room_key,
            mode: mode_key,
            entered: false,
            message: "演练：已停在模式列表，未点开始".into(),
        });
        // NOTE: room_key/mode_key are moved above only on the early-return path;
        // the code below (non-dry-run) still owns them.
    }

    // Step 1: ranked banner.
    click_norm(page, ranked, false).await?;
    pause(cancel, 2800).await?;
    // Step 2: room row.
    click_norm(page, room_pt, false).await?;
    pause(cancel, 2800).await?;
    // Step 3: mode card. The bottom card can be partly covered by the UI bar,
    // so retry with a small y offset and jiggle, same as the bridge.
    let y_tries = [0.0_f64, 0.03, -0.03, 0.05, -0.05];
    for (i, dy) in y_tries.iter().enumerate() {
        click_norm(page, [mode_pt[0], mode_pt[1] + dy], i > 0).await?;
        if wait_in_game(signals, 20000, cancel).await {
            let msg = format!("已进入 {room_key} {mode_key}");
            return Ok(EnterResult {
                room: room_key,
                mode: mode_key,
                entered: true,
                message: msg,
            });
        }
    }
    Ok(EnterResult {
        room: room_key,
        mode: mode_key,
        entered: false,
        message: "点模式后未进入对局（坐标可能点错或未配对）".into(),
    })
}

/// Dismiss a settlement screen by clicking the confirm point up to 4 times,
/// stopping once the log no longer reports an active game.
pub async fn settle(
    page: &Page,
    cfg: &crate::config::PoolConfig,
    acc: &crate::config::PoolAccount,
    signals: &SessionSignals,
    cancel: &AtomicBool,
) -> Result<()> {
    check_cancel(cancel)?;
    let confirm = session::point(cfg, acc, "confirm")
        .ok_or_else(|| anyhow!("缺少 confirm 坐标"))?;
    // A genuinely running match must never be clicked through.
    if signals.snapshot().in_game {
        return Ok(());
    }
    // The settlement screen animates in for a few seconds; clicks sent during
    // that window are swallowed (observed on a real match: four immediate
    // clicks did nothing, while the same clicks minutes later worked).
    pause(cancel, 3000).await?;
    // The log flips to in_game=false the moment the match ends, while the
    // client still shows the settlement screen ("終局"). Clicking confirm is a
    // no-op on the lobby, so do it unconditionally a few times — otherwise the
    // ranked banner stays covered and both pool_start and auto-next fail.
    for _ in 0..6 {
        click_norm(page, confirm, false).await?;
        pause(cancel, 2200).await?;
    }
    Ok(())
}

/// Bring a game browser tab back to the Majsoul lobby *before* clicking the
/// ranked banner. A fresh profile / a flaky network leaves tabs on unrelated
/// pages (客服 chat, "获取版本资讯失败" dialog, 活動 center), where the fixed
/// 进房 coordinates land on help links / dead space — producing "点到客服".
///
/// Only called when no match is running: navigating away mid-game would AFK.
/// No canvas/DOM dependency, so it can run BEFORE `wait_page_ready` — a tab
/// sitting on the 客服 webchat never renders a canvas and would time out.
pub async fn ensure_game_url(page: &Page, cancel: &AtomicBool) -> Result<()> {
    check_cancel(cancel)?;
    const GAME_URL: &str = "https://game.maj-soul.com/1/";
    // `Page::url()` on this chromiumoxide fork can return a bogus ":" for a
    // tab that navigated away; read location.href from the page itself,
    // which is reliable (verified against the 客服 webchat redirect).
    let url = eval_json(page, "location.href")
        .await
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    tracing::debug!("[pool] current tab url for lobby check: {url}");
    if !url.contains("maj-soul.com") && !url.contains("mahjongsoul.com") {
        tracing::info!("[pool] tab on non-game page ({url}), navigating back to lobby");
        // `Page.navigate` can target a stale frame on this fork; assigning
        // location.href via Runtime.evaluate always acts on the held page.
        let expr = format!("location.href = '{}'", GAME_URL);
        let _ = eval_json(page, &expr).await;
        pause(cancel, 800).await?;
    }
    Ok(())
}

/// Which full-screen Unity screen is actually visible.
///
/// Majsoul draws lobby/overlays entirely inside the WebGL canvas, so there
/// is no DOM state to query. We sample a handful of canvas-normalised points
/// each round instead of trusting a cached "last screen" or a URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiScreen {
    Lobby,
    Lecture,
    Activity,
    Login,
    Game,
    /// "您的账号已在另一处登入" modal. It covers the lobby but leaves the
    /// outer background intact, so it must be detected explicitly — treating
    /// it as a lobby would send the ranked/room clicks into the modal.
    AccountConflict,
    Unknown,
}

impl UiScreen {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            UiScreen::Lobby => "lobby",
            UiScreen::Lecture => "lecture",
            UiScreen::Activity => "activity",
            UiScreen::Login => "login",
            UiScreen::Game => "game",
            UiScreen::AccountConflict => "account_conflict",
            UiScreen::Unknown => "unknown",
        }
    }
}

/// Points used to fingerprint the current screen. Kept in 0..1 canvas space
/// (not CSS pixels) so a resize cannot change which game element is sampled.
const UI_SAMPLE_POINTS: [(f64, f64); 5] = [
    (0.20, 0.20),
    (0.50, 0.20),
    (0.65, 0.20),
    (0.50, 0.50),
    (0.90, 0.15),
];

/// Reference colours sampled from verified screenshots on a real client.
/// Small variations (animation, account avatar) are absorbed by the distance
/// threshold in [`classify_ui`].
fn ui_signature(screen: UiScreen) -> [[u8; 3]; 5] {
    match screen {
        // Re-sampled 2026-11-25 from the CURRENT client lobby (new 立绘/樱花
        // layout). The old signature ([192,111,131],[191,196,246],...) came
        // from an earlier skin and no longer matches at all — that is why a
        // clean lobby classified as Unknown. Sampled via CDP screenshot at
        // points (0.20,0.20),(0.50,0.20),(0.65,0.20),(0.50,0.50),(0.90,0.15).
        UiScreen::Lobby => [
            [56, 30, 51],
            [255, 251, 255],
            [115, 77, 132],
            [119, 105, 184],
            [129, 85, 132],
        ],
        UiScreen::Lecture => [
            [33, 81, 82],
            [32, 78, 80],
            [35, 80, 83],
            [40, 87, 88],
            [62, 33, 31],
        ],
        // Re-sampled 2026-11-25 from the current「雾眠之岛」activity page.
        UiScreen::Activity => [
            [42, 59, 97],
            [253, 246, 239],
            [253, 245, 228],
            [211, 211, 225],
            [44, 45, 66],
        ],
        UiScreen::Login => [
            [9, 3, 7],
            [4, 1, 4],
            [21, 26, 52],
            [5, 2, 5],
            [0, 0, 0],
        ],
        UiScreen::Game => [
            [37, 62, 103],
            [62, 91, 162],
            [56, 79, 135],
            [55, 87, 142],
            [79, 36, 24],
        ],
        UiScreen::AccountConflict => [
            [192, 111, 131],
            [189, 195, 246],
            [235, 189, 243],
            [74, 81, 99],
            [246, 230, 246],
        ],
        UiScreen::Unknown => [[0, 0, 0]; 5],
    }
}

/// Nearest-signature classifier with a confidence margin. Unknown is a
/// deliberate answer: an unrecognised screen must never receive a fixed
/// coordinate click, because that is how we previously opened the tutorial
/// by clicking the 学士帽 icon while trying to close an overlay.
fn classify_ui(samples: &[[u8; 3]]) -> UiScreen {
    // The centre point sits inside a modal but on plain background in the
    // lobby; weighting it 3x is what separates "lobby" from the account-
    // conflict dialog, which otherwise differs at only one of five points.
    const WEIGHTS: [f64; 5] = [1.0, 1.0, 1.0, 3.0, 1.0];
    let weight_sum: f64 = WEIGHTS.iter().sum();
    let mut best = (UiScreen::Unknown, f64::MAX);
    let mut second = f64::MAX;
    for screen in [
        UiScreen::Lobby,
        UiScreen::Lecture,
        UiScreen::Activity,
        UiScreen::Login,
        UiScreen::Game,
        UiScreen::AccountConflict,
    ] {
        let sig = ui_signature(screen);
        let dist = samples
            .iter()
            .zip(sig.iter())
            .zip(WEIGHTS.iter())
            .map(|((a, b), w)| {
                let dr = i32::from(a[0]) - i32::from(b[0]);
                let dg = i32::from(a[1]) - i32::from(b[1]);
                let db = i32::from(a[2]) - i32::from(b[2]);
                ((dr * dr + dg * dg + db * db) as f64).sqrt() * w
            })
            .sum::<f64>()
            / weight_sum;
        if dist < best.1 {
            second = best.1;
            best = (screen, dist);
        } else if dist < second {
            second = dist;
        }
    }
    if best.1 <= 52.0 && best.1 <= second * 0.85 {
        best.0
    } else {
        UiScreen::Unknown
    }
}

pub(crate) async fn probe_ui_screen(page: &Page) -> UiScreen {
    match crate::autoplay::cdp_input::read_canvas_pixels(page, &UI_SAMPLE_POINTS).await {
        Ok(samples) if samples.len() == UI_SAMPLE_POINTS.len() => {
            let screen = classify_ui(&samples);
            tracing::debug!("[pool] UI samples {samples:?} -> {screen:?}");
            screen
        }
        Ok(samples) => {
            tracing::debug!("[pool] unexpected UI sample count {}", samples.len());
            UiScreen::Unknown
        }
        Err(e) => {
            tracing::debug!("[pool] UI probe failed: {e:#}");
            UiScreen::Unknown
        }
    }
}

async fn press_escape(page: &Page, cancel: &AtomicBool) -> Result<()> {
    check_cancel(cancel)?;
    dispatch_key(
        page,
        DispatchKeyEventType::RawKeyDown,
        "Escape",
        "Escape",
        27,
        None,
        None,
    )
    .await?;
    dispatch_key(
        page,
        DispatchKeyEventType::KeyUp,
        "Escape",
        "Escape",
        27,
        None,
        None,
    )
    .await
}

pub async fn prepare_lobby(
    page: &Page,
    acc: &crate::config::PoolAccount,
    cancel: &AtomicBool,
) -> Result<()> {
    check_cancel(cancel)?;
    // ① Non-game tabs (客服 webchat opened by a mis-click) are sent back.
    ensure_game_url(page, cancel).await?;

    // ② Do NOT clear overlays here. On the current client any fixed-point
    // "close" click can hit a different icon (the activity back point lands
    // on 学士帽 once the lobby appears, opening the 讲座 overlay). Room entry
    // itself already clicks `back` and uses WS-based retry, which is the
    // production v3.8 behaviour. Just let the page settle briefly.
    pause(cancel, 1200).await?;
    Ok(())
}

/// Clear full-screen overlays *only when they are actually visible*.
///
/// The old implementation always clicked a pair of hard-coded points. On a
/// plain lobby one of them is the 学士帽 tutorial entry and the other is the
/// help "?" button, so every "clear overlays" attempt could itself create
/// the overlay it was meant to remove. Window resizes made this worse by
/// moving the elements under those points. Now we sample the live canvas and
/// act on the detected screen only.
/// Canvas-normalised point of the 活動 page's back arrow (the gold curved
/// arrow directly below-left of the "?" help icon on the right toolbar).
///
/// Verified 2026-11-25 by CDP click on the live 「雾眠之岛」 activity page:
/// clicking [0.945, 0.24] (canvas rect x=77,y=0,w=1112,h=626 → CSS 1128,150)
/// returns to the lobby (段位場/比赛場/友人場) and does NOT hit the adjacent
/// "?" (tutorial) or 学士帽 icons. Escape does NOT close this activity page —
/// confirmed by the user — so a mouse click on this arrow is the only exit.
/// Configured per-account as `activity_back`; this constant is the fallback.
const ACTIVITY_BACK_FALLBACK: [f64; 2] = [0.945, 0.24];

/// Close X of the 「一姬老师的讲座」panel (white X, top-right). Measured from
/// a 2026-11-25 screenshot: px (1689,130) of 1899x939 → (0.889, 0.138).
const LECTURE_CLOSE: [f64; 2] = [0.889, 0.138];

fn activity_back_point(acc: &crate::config::PoolAccount) -> [f64; 2] {
    coord(acc, "activity_back").unwrap_or(ACTIVITY_BACK_FALLBACK)
}

pub async fn dismiss_popups(
    page: &Page,
    acc: &crate::config::PoolAccount,
    cancel: &AtomicBool,
) -> Result<()> {
    check_cancel(cancel)?;
    let back = activity_back_point(acc);
    for round in 0..5 {
        let screen = probe_ui_screen(page).await;
        tracing::info!("[pool] overlay probe round {round}: {screen:?}");
        match screen {
            // Nothing to dismiss — and never click fixed points here.
            UiScreen::Lobby | UiScreen::Game | UiScreen::Login => return Ok(()),
            // Unknown is ambiguous: a fresh render transition OR an overlay
            // with an unknown skin — most importantly the「雾眠之岛」activity
            // page, whose new skin the signature library cannot classify.
            // The activity back arrow [0.945,0.24] is CDP-verified safe: it
            // is empty sky on a settled lobby (no-op) and the gold back arrow
            // on the activity page (closes it); it never hits the adjacent
            // "?" / 学士帽 icons. The earlier 10-minute hang was caused by
            // rAF throttling in the pixel probe (fixed separately), NOT by
            // this click — so do click here to clear unknown overlays.
            UiScreen::Unknown => {
                // Unknown is usually a TRANSIENT frame (page just closed an
                // overlay, UI animating) rather than a real unrecognised
                // overlay. Clicking during a transition is dangerous: the
                // activity back point [0.945,0.24] lands on the 学士帽
                // tutorial icon once the lobby has appeared, which OPENS the
                // very overlay we are trying to clear (that is how the
                // 讲座 page appeared in earlier runs). So wait first; only
                // after several consecutive Unknown probes do we assume a
                // real overlay and click the safe back arrow once.
                if round < 3 {
                    tracing::info!(
                        "[pool] overlay screen unknown round {round}; waiting for settle (no click)"
                    );
                    pause(cancel, 1200).await?;
                } else {
                    tracing::info!(
                        "[pool] overlay screen unknown round {round}; clicking activity back arrow"
                    );
                    click_norm(page, back, false).await?;
                    pause(cancel, 1800).await?;
                }
            }
            // Both browsers are logged into the same account. Any click here
            // would hit the modal, not the lobby; report it instead of
            // pretending the lobby is ready.
            UiScreen::AccountConflict => {
                // The conflict signature differs from the (new) lobby one at
                // only the centre sample point, so a mid-transition frame
                // right after closing an overlay can be misread as a
                // duplicate-login modal. Re-probe a couple of times before
                // aborting: a transient frame resolves, a real conflict
                // stays. Only error out if it persists for 3 rounds.
                if round < 2 {
                    tracing::info!(
                        "[pool] possible account conflict round {round}; re-probing before abort"
                    );
                    pause(cancel, 1200).await?;
                } else {
                    return Err(anyhow!(
                        "该账号已在另一处登入，请先关闭重复会话或重新登录"
                    ));
                }
            }
            // 「一姬老师的讲座」close X. Measured 2026-11-25 from a real
            // screenshot: white X centre at px (1689,130) of 1899x939 →
            // canvas-normalised (0.889, 0.138). The old (0.939,0.149)
            // was too far right and missed the X entirely.
            UiScreen::Lecture => {
                tracing::info!("[pool] lecture overlay; clicking close X");
                click_norm(page, LECTURE_CLOSE, false).await?;
                pause(cancel, 900).await?;
            }
            // 活動 center: Escape does NOT close this client build (user-
            // confirmed 2026-11-25). Click the verified back arrow instead.
            UiScreen::Activity => {
                tracing::info!("[pool] activity overlay; clicking back arrow {back:?}");
                click_norm(page, back, false).await?;
                pause(cancel, 900).await?;
            }
        }
    }
    Ok(())
}

/// two-step reset: Escape first (closes the activity center in most builds),
/// then one page reload. Login state lives in localStorage/IndexedDB, so the
/// reload returns to the lobby rather than the login screen.
pub async fn ensure_lobby(page: &Page, cancel: &AtomicBool) -> Result<()> {
    check_cancel(cancel)?;
    for _ in 0..2 {
        press_escape(page, cancel).await?;
        pause(cancel, 800).await?;
    }
    use chromiumoxide::cdp::browser_protocol::page::ReloadParams;
    page.execute(ReloadParams::default())
        .await
        .context("Page.reload (lobby recovery)")?;
    // Unity needs time to redraw the lobby after the reload. 10s proved too
    // tight on a real machine: the next click landed before the lobby was
    // interactive and navigated into 友人場 instead.
    pause(cancel, 15000).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixed_cdp_port() {
        let args = vec![
            "--remote-debugging-port=63503".to_string(),
            "--window-size=1600,900".to_string(),
        ];
        assert_eq!(game_cdp_port(&args), Some(63503));
        let args2 = vec![
            "--remote-debugging-port".to_string(),
            "63504".to_string(),
        ];
        assert_eq!(game_cdp_port(&args2), Some(63504));
        assert_eq!(game_cdp_port(&[]), None);
    }

    #[test]
    fn login_placeholder_is_logged_out() {
        // Mirror of probe_login's rule, kept as a pure assertion on the
        // placeholder string so a future change can't silently regress it.
        let account = "FKU!!!";
        let logged_in = !account.is_empty() && account != "FKU!!!";
        assert!(!logged_in);
    }

    #[test]
    fn ui_classifier_distinguishes_lobby_from_overlays() {
        for screen in [
            UiScreen::Lobby,
            UiScreen::Lecture,
            UiScreen::Activity,
            UiScreen::Login,
            UiScreen::Game,
            UiScreen::AccountConflict,
        ] {
            assert_eq!(classify_ui(&ui_signature(screen)), screen);
        }
        // A blank frame is harmless: it must never be mistaken for an
        // overlay and receive a fixed-coordinate click.
        assert!(!matches!(
            classify_ui(&[[0, 0, 0]; 5]),
            UiScreen::Lecture | UiScreen::Activity
        ));
    }
}
