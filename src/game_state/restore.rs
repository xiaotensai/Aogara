//! Restore an in-progress game into a fresh `GameTracker` after an Aogara
//! restart, so autoplay can take over a match that is still running in the
//! browser — without reloading the page (a reload would send the user back
//! to the Majsoul login screen and feel like opening a fresh window).
//!
//! Why this works: autoplay reads seat + snapshot + legal actions exclusively
//! from the shared `GameTracker` (`tracker.our_seat()` / `snapshot()` /
//! `our_seat_can_act()`); the bridge only *feeds* events. The bridge's `authGame`
//! handshake (which normally resolves the seat) happened before this process
//! started, so CDP can't see it — but every emitted `MjaiEvent` was persisted
//! as a JSON line in `<session>/majsoul/majsoul_<ts>.mjai.jsonl`. Replaying
//! those lines straight into the tracker reconstructs the exact live state.
//!
//! Crucially, replay calls `tracker.handle()` directly and NEVER publishes to
//! the MJAI bus. The autoplay manager and bot manager subscribe to that bus,
//! so they never see these historical events and cannot fire stale discards.
//! Only *new* frames arriving from CDP after attach flow through the bus.

use crate::game_state::tracker::GameTracker;
use crate::schema::mjai::MjaiEvent;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing::{info, warn};

/// A replay log older than this can't represent a game that's still running:
/// an active match writes a new event every few seconds. Without this guard
/// a stale half-finished log from a previous crash would be replayed into a
/// phantom game state after every restart (the browser is actually in the
/// lobby). 5 minutes is generous — it covers a player sitting in a long
/// thought — while excluding the tens-of-minutes-old leftovers.
pub const RESTORE_MAX_AGE: Duration = Duration::from_secs(5 * 60);

/// Description of a restored game, for logging / UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreInfo {
    pub source: PathBuf,
    pub events: usize,
    pub seat: Option<u8>,
    pub num_players: u8,
    pub last_event: String,
    /// The final event, kept so startup can re-emit a synthetic TrackedEvent
    /// to the bot manager if it's our turn to act at the moment of restart.
    pub last_mjai: Option<MjaiEvent>,
}

/// Find the most recently modified `.mjai.jsonl` that represents a game still
/// in progress (last line is not `end_game`) and that has a `start_game`.
///
/// Scans every session dir under `logs_dir` — after a restart Aogara creates a
/// NEW session dir, while the unfinished match's log lives in the OLD one.
pub fn find_latest_unfinished(logs_dir: &Path) -> Option<PathBuf> {
    find_latest_unfinished_before(logs_dir, SystemTime::now(), RESTORE_MAX_AGE)
}

/// Same as [`find_latest_unfinished`] but with an explicit `now` / `max_age`,
/// so unit tests can pin the freshness window deterministically.
pub fn find_latest_unfinished_before(
    logs_dir: &Path,
    now: SystemTime,
    max_age: Duration,
) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    let sessions = fs::read_dir(logs_dir).ok()?;
    for sess in sessions.flatten() {
        let majsoul = sess.path().join("majsoul");
        let entries = match fs::read_dir(&majsoul) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for ent in entries.flatten() {
            let path = ent.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let text = match fs::read_to_string(&path) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let mut lines = text.lines().filter(|l| !l.trim().is_empty());
            let first = match lines.next() {
                Some(l) => l,
                None => continue,
            };
            // Must be a real game log: starts with start_game.
            if !first.contains("\"type\":\"start_game\"") {
                continue;
            }
            // Unfinished: last non-empty line is not end_game / no more lines.
            let last = lines.last().unwrap_or(first);
            if last.contains("\"type\":\"end_game\"") {
                continue;
            }
            let mtime = ent.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            // Skip stale leftovers: a still-running match writes continuously.
            let fresh = now
                .duration_since(mtime)
                .map(|age| age <= max_age)
                .unwrap_or(false);
            if !fresh {
                continue;
            }
            if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                best = Some((mtime, path));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Replay every `MjaiEvent` line in `path` into `tracker` directly.
///
/// Returns `Ok(None)` if the file can't be read / has no replayable events;
/// individual malformed lines are skipped with a warning (never abort the
/// whole restore over one bad line).
pub fn restore_into(path: &Path, tracker: &mut GameTracker) -> Result<Option<RestoreInfo>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading replay log {}", path.display()))?;
    let mut events = 0usize;
    let mut seat = None;
    let mut num_players = 0u8;
    let mut last_event = String::new();
    let mut last_mjai: Option<MjaiEvent> = None;

    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let ev: MjaiEvent = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(e) => {
                warn!("restore: skipping malformed replay line: {e}");
                continue;
            }
        };
        if let MjaiEvent::StartGame { id, num_players: np, .. } = &ev {
            seat = *id;
            num_players = *np;
        }
        last_event = match &ev {
            MjaiEvent::StartGame { .. } => "start_game".to_string(),
            _ => serde_json::to_value(&ev)
                .ok()
                .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
                .unwrap_or_else(|| "?".to_string()),
        };
        // Direct handle — no mjai_bus publish, so no stale bot decisions.
        if let Err(e) = tracker.handle(&ev) {
            warn!("restore: tracker rejected an event ({e}); continuing");
        }
        last_mjai = Some(ev);
        events += 1;
    }

    if events == 0 {
        return Ok(None);
    }
    let info = RestoreInfo {
        source: path.to_path_buf(),
        events,
        seat,
        num_players,
        last_event,
        last_mjai,
    };
    info!(
        "restore: replayed {} events from {} (seat={:?}, {}p, last={})",
        info.events,
        path.display(),
        info.seat,
        info.num_players,
        info.last_event
    );
    Ok(Some(info))
}

/// Convenience: find + restore in one call.
pub fn try_restore(logs_dir: &Path, tracker: &mut GameTracker) -> Option<RestoreInfo> {
    let path = find_latest_unfinished(logs_dir)?;
    match restore_into(&path, tracker) {
        Ok(info) => info,
        Err(e) => {
            warn!("restore: failed to restore from {}: {e:#}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_log(root: &Path, session: &str, end: bool) -> PathBuf {
        let dir = root.join(session).join("majsoul");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("g.mjai.jsonl");
        let mut body = "{\"type\":\"start_game\",\"names\":[\"a\"],\"id\":1,\"num_players\":4}\n{\"type\":\"tsumo\",\"actor\":1,\"pai\":\"1m\"}\n".to_string();
        if end {
            body.push_str("{\"type\":\"end_game\"}\n");
        }
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn skips_finished_logs_and_honours_freshness_window() {
        let root = std::env::temp_dir().join(format!("aogara-restore-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let now = SystemTime::now();

        // A finished log is never restored.
        write_log(&root, "s1", true);
        assert!(find_latest_unfinished_before(&root, now, RESTORE_MAX_AGE).is_none());

        // An unfinished log inside the window is picked.
        let fresh = write_log(&root, "s2", false);
        assert_eq!(
            find_latest_unfinished_before(&root, now, RESTORE_MAX_AGE).as_deref(),
            Some(fresh.as_path())
        );

        // A zero-length window excludes everything (simulates staleness).
        assert!(find_latest_unfinished_before(&root, now, Duration::ZERO).is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn replay_rebuilds_seat_and_state() {
        let root = std::env::temp_dir().join(format!("aogara-restore-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let p = write_log(&root, "s", false);
        let mut t = crate::game_state::tracker::GameTracker::new();
        let info = restore_into(&p, &mut t).unwrap().unwrap();
        assert_eq!(info.seat, Some(1));
        assert_eq!(info.events, 2);
        assert!(t.snapshot().is_some(), "tracker should hold rebuilt state");
        let _ = std::fs::remove_dir_all(&root);
    }
}
