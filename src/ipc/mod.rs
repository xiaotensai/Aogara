//! Backend ↔ frontend integration over Tauri.
//!
//! Two halves wired together by [`install`]:
//!
//! - **Outbound (backend → frontend)**: forwarder tasks subscribe to each
//!   `event_bus` channel and `app.emit()` the payload to every webview.
//!   Status buses additionally mirror the latest value into `AppState`
//!   so a fresh frontend can ask `get_status` for a snapshot.
//! - **Inbound (frontend → backend)**: `#[tauri::command]` handlers in
//!   [`commands`]; register them via the `ipc_handlers!()` macro on the
//!   `tauri::Builder`.
//!
//! Wiring example (in `lib.rs`):
//!
//! ```ignore
//! tauri::Builder::default()
//!     .invoke_handler(aogara::ipc_handlers!())
//!     .setup(move |app| {
//!         aogara::ipc::install(&app.handle(), state.clone())?;
//!         // …rest of setup
//!         Ok(())
//!     })
//!     .run(tauri::generate_context!())?;
//! ```
//!
//! Event names (kebab-case, Tauri convention):
//!
//! | Event             | Payload type                  |
//! |-------------------|-------------------------------|
//! | `mjai-event`       | `schema::MjaiEvent`           |
//! | `bot-response`     | `bot::BotResponse`            |
//! | `bot-status`       | `schema::BotStatus`           |
//! | `capture-status`   | `schema::CaptureStatus`       |
//! | `notify`           | `schema::Notification`        |
//! | `analysis-result`  | `analysis::AnalysisResult`    |
//! | `history-recorded` | `schema::HistoryEvent`        |
//! | `overlay-config`   | `config::OverlayConfig`       |
//!
//! All of the above are broadcast to every webview, which is what lets the
//! [`overlay`] window render suggestions without any dedicated plumbing.
//! `overlay-config` is the exception: it is emitted to the overlay label only.

pub mod capture_supervisor;
pub mod commands;
pub mod overlay;
pub mod state;

pub use state::AppState;

use anyhow::Result;
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tokio::sync::broadcast;
use tracing::warn;

/// Manage `state` on the Tauri app and spawn the five forwarder tasks.
/// Call from inside the builder's `.setup` closure.
pub fn install<R: Runtime>(app: &AppHandle<R>, state: AppState) -> Result<()> {
    app.manage(state.clone());
    spawn_forwarders(app.clone(), state);
    Ok(())
}

fn spawn_forwarders<R: Runtime>(app: AppHandle<R>, state: AppState) {
    forward(app.clone(), state.mjai_bus.subscribe(), "mjai-event");
    forward(
        app.clone(),
        state.bot_response_bus.subscribe(),
        "bot-response",
    );
    forward(app.clone(), state.notify_bus.subscribe(), "notify");
    forward(
        app.clone(),
        state.analysis_bus.subscribe(),
        "analysis-result",
    );
    forward(
        app.clone(),
        state.history_bus.subscribe(),
        "history-recorded",
    );

    // Status buses: forward AND snapshot into AppState.
    spawn_bot_status_forwarder(app.clone(), state.clone());
    spawn_capture_status_forwarder(app.clone(), state.clone());
    spawn_session_status_forwarder(app, state.clone(), state.session.id.clone());
}

/// Emit a small `session-status` ping when a session's match/capture/bot
/// status changes. The frontend uses it to refresh only the session list,
/// instead of polling every two seconds.
pub fn spawn_session_status_forwarder<R: Runtime>(
    app: AppHandle<R>,
    state: AppState,
    slot: String,
) {
    let mut rx = state.mjai_bus.subscribe();
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let interesting = matches!(
                        ev,
                        crate::schema::MjaiEvent::StartGame { .. }
                            | crate::schema::MjaiEvent::EndGame { .. }
                            | crate::schema::MjaiEvent::StartKyoku { .. }
                            | crate::schema::MjaiEvent::EndKyoku { .. }
                    );
                    if interesting {
                        let _ = app.emit("session-status", json!({ "slot": slot }));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

/// Plain forwarder: subscribe, drain, emit. `Lagged` is logged once and
/// the loop continues (broadcast auto-resumes after lag).
fn forward<R, T>(app: AppHandle<R>, mut rx: broadcast::Receiver<T>, event: &'static str)
where
    R: Runtime,
    T: Clone + serde::Serialize + Send + 'static,
{
    forward_named(app, rx, event.to_string())
}

/// Same as [`forward`], but the event name is computed at call time —
/// used for the per-slot `:<slot>` suffixed channels below.
fn forward_named<R, T>(app: AppHandle<R>, mut rx: broadcast::Receiver<T>, event: String)
where
    R: Runtime,
    T: Clone + serde::Serialize + Send + 'static,
{
    tauri::async_runtime::spawn(async move {
        let event = event; // move into the loop task
        loop {
            match rx.recv().await {
                Ok(payload) => {
                    if let Err(e) = app.emit(&event, &payload) {
                        warn!("ipc emit {event} failed: {e}");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("ipc forwarder {event} lagged by {n}");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

/// Per-session forwarders for sessions 2..N: same payloads as the global
/// forwarders, but the event name carries a `:<slot>` suffix so the frontend
/// can route each session's game data to its own dashboard view. Global
/// notifications (`notify`) are re-emitted on the shared channel — toasts
/// are process-wide by design.
///
/// slot1 keeps the unsuffixed names (spawned by [`install`]) for
/// compatibility with existing listeners.
pub fn spawn_slot_forwarders<R: Runtime>(app: AppHandle<R>, state: AppState, slot: &str) {
    let suffix = format!(":{slot}");
    forward_named(
        app.clone(),
        state.mjai_bus.subscribe(),
        format!("mjai-event{suffix}"),
    );
    forward_named(
        app.clone(),
        state.bot_response_bus.subscribe(),
        format!("bot-response{suffix}"),
    );
    forward_named(
        app.clone(),
        state.analysis_bus.subscribe(),
        format!("analysis-result{suffix}"),
    );
    forward_named(
        app.clone(),
        state.history_bus.subscribe(),
        format!("history-recorded{suffix}"),
    );
    forward_named(
        app.clone(),
        state.bot_status_bus.subscribe(),
        format!("bot-status{suffix}"),
    );
    forward_named(
        app.clone(),
        state.capture_status_bus.subscribe(),
        format!("capture-status{suffix}"),
    );
    forward_named(app.clone(), state.notify_bus.subscribe(), "notify".to_string());
    spawn_session_status_forwarder(app, state.clone(), slot.to_string());
}

fn spawn_bot_status_forwarder<R: Runtime>(app: AppHandle<R>, state: AppState) {
    let mut rx = state.bot_status_bus.subscribe();
    let snapshot = state.bot_status.clone();
    let slot = state.session.id.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(s) => {
                    *snapshot.write().await = s.clone();
                    if let Err(e) = app.emit("bot-status", &s) {
                        warn!("ipc emit bot-status failed: {e}");
                    }
                    let _ = app.emit("session-status", json!({ "slot": slot }));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("ipc forwarder bot-status lagged by {n}");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

fn spawn_capture_status_forwarder<R: Runtime>(app: AppHandle<R>, state: AppState) {
    let mut rx = state.capture_status_bus.subscribe();
    let control = state.capture_control.clone();
    let slot = state.session.id.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(s) => {
                    control.lock().await.status = s.clone();
                    if let Err(e) = app.emit("capture-status", &s) {
                        warn!("ipc emit capture-status failed: {e}");
                    }
                    let _ = app.emit("session-status", json!({ "slot": slot }));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("ipc forwarder capture-status lagged by {n}");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}
