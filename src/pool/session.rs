//! Log-derived game session state + config helpers for the native pool driver.
//!
//! Mirrors the external bridge's `aogara.js::gameState()` and `pool.js`:
//! - Game in-progress / finished is read from `all.log` (NOT from the in-memory
//!   GameTracker, which resets on restart). Markers: `FastTest.enterGame` /
//!   `ActionNewRound` start a game; `NotifyGameFinishReward` ends a whole match.
//! - Accounts without their own `coords` inherit the `main` account's coords
//!   (all three share the same Unity UI layout).

use crate::config::{PoolAccount, PoolConfig};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Latest `<dir>/YYYYMMDD-HHMMSS/` session folder (by name, which is a
/// timestamp). Same convention as the external tool's `latestLog()`.
pub fn latest_session_dir(logs_dir: &Path) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(logs_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.len() >= 15 && n.chars().all(|c| c.is_ascii_digit() || c == '-'))
                .unwrap_or(false)
        })
        .collect();
    dirs.sort();
    dirs.pop()
}

/// Read the session log tail (last `max_bytes`) as a String.
fn read_log_tail(session_dir: &Path, max_bytes: u64) -> Option<String> {
    let path = session_dir.join("all.log");
    let mut file = std::fs::File::open(&path).ok()?;
    let len = std::fs::metadata(&path).ok()?.len();
    let skip = len.saturating_sub(max_bytes);
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(skip)).ok()?;
    let mut buf = Vec::with_capacity((len - skip) as usize + 1);
    file.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Game session state derived from the log, matching `aogara.js::gameState()`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionState {
    pub session: String,
    pub in_game: bool,
    pub last_enter: Option<String>,
    pub last_finish: Option<String>,
    /// Monotonic counter of `NotifyGameFinishReward` lines in the tail — used
    /// by auto-next to detect a *new* finish since it started.
    pub finish_count: usize,
}

/// Read game state from the latest session log. The tail window (8 MiB)
/// mirrors the bridge and is wide enough that enterGame isn't pushed out
/// during long games.
pub fn session_state(logs_dir: &Path) -> SessionState {
    let empty = || SessionState {
        session: String::new(),
        in_game: false,
        last_enter: None,
        last_finish: None,
        finish_count: 0,
    };
    let dir = match latest_session_dir(logs_dir) {
        Some(d) => d,
        None => return empty(),
    };
    let txt = match read_log_tail(&dir, 8 * 1024 * 1024) {
        Some(t) => t,
        None => return empty(),
    };
    let mut last_enter: Option<String> = None;
    let mut last_finish: Option<String> = None;
    let mut last_round: Option<String> = None;
    let mut finish_count = 0usize;
    for line in txt.lines() {
        if !(line.contains("NOTIFY") || line.contains("REQUEST") || line.contains("RESPONSE")) {
            continue;
        }
        let ts: String = line.chars().take(23).collect();
        // 復帰 (reconnect) can resume a game without a new enterGame; treat a
        // recent ActionNewRound as an in-game signal too (same as the bridge).
        if line.contains(".lq.FastTest.enterGame") {
            last_enter = Some(ts.clone());
        }
        if line.contains("NotifyGameFinishReward") {
            last_finish = Some(ts.clone());
            finish_count += 1;
        }
        if line.contains("ActionNewRound") {
            last_round = Some(ts);
        }
    }
    let last_start = [last_enter.clone(), last_round]
        .into_iter()
        .flatten()
        .max();
    let in_game = match (&last_start, &last_finish) {
        (Some(s), Some(f)) => s > f,
        (Some(_), None) => true,
        _ => false,
    };
    SessionState {
        session: dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string(),
        in_game,
        last_enter,
        last_finish,
        finish_count,
    }
}

/// Resolve an account's coordinate table with inheritance from `main`.
/// Returns an owned table so callers can index without lifetime juggling.
pub fn coords_for<'a>(cfg: &'a PoolConfig, acc: &'a PoolAccount) -> Option<&'a std::collections::BTreeMap<String, crate::config::PoolCoord>> {
    if !acc.coords.is_empty() {
        return Some(&acc.coords);
    }
    cfg.accounts
        .iter()
        .find(|a| a.id == "main" && !a.coords.is_empty())
        .map(|a| &a.coords)
}

/// Return a copy of `acc` with its coordinate table filled from `main` when
/// empty (accounts without their own calibration inherit main's UI layout).
pub fn resolve_account(cfg: &PoolConfig, acc: &PoolAccount) -> PoolAccount {
    let mut out = acc.clone();
    if out.coords.is_empty() {
        if let Some(base) = cfg.accounts.iter().find(|a| a.id == "main") {
            out.coords = base.coords.clone();
        }
    }
    out
}

/// Map a logical key to the literal key used in the config. The external
/// accounts.json uses camelCase (`loginEmail`, `logoutConfirm`) while the
/// original native module used snake_case; accept both.
pub fn canon_key_public(key: &str) -> &str {
    canon_key(key)
}

fn canon_key(key: &str) -> &str {
    match key {
        "login_email" => "loginEmail",
        "login_password" => "loginPassword",
        "login_submit" => "loginSubmit",
        "logout_confirm" => "logoutConfirm",
        _ => key,
    }
}

/// Point for a plain key (e.g. `ranked`, `settings`).
pub fn point(cfg: &PoolConfig, acc: &PoolAccount, key: &str) -> Option<[f64; 2]> {
    let table = coords_for(cfg, acc)?;
    table
        .get(canon_key(key))
        .or_else(|| table.get(key))
        .and_then(|c| c.point())
}

/// Point inside a nested table (`rooms` / `modes`).
pub fn table_point(cfg: &PoolConfig, acc: &PoolAccount, table: &str, key: &str) -> Option<[f64; 2]> {
    coords_for(cfg, acc)?
        .get(table)
        .and_then(|c| c.table_get(key))
}

/// Canonical room/mode keys, mirroring `pool.js::normalizeRoom/normalizeMode`.
pub const ROOM_KEYS: &[&str] = &["铜之间", "银之间", "金之间", "玉之间", "王座之间"];
pub const MODE_KEYS: &[&str] = &["mode4e", "mode4s", "mode3e", "mode3s"];

/// Normalise a room value; also accepts ASCII names used by the frontend.
pub fn normalize_room(room: &str) -> Option<String> {
    match room {
        "铜之间" | "copper" => Some("铜之间".into()),
        "银之间" | "silver" => Some("银之间".into()),
        "金之间" | "gold" => Some("金之间".into()),
        "玉之间" | "jade" => Some("玉之间".into()),
        "王座之间" | "throne" => Some("王座之间".into()),
        _ => None,
    }
}

/// Normalise a mode value; legacy `east`/`south` map by seat count.
pub fn normalize_mode(mode: &str, seats: u8) -> Option<String> {
    match mode {
        "mode4e" => Some("mode4e".into()),
        "mode4s" => Some("mode4s".into()),
        "mode3e" => Some("mode3e".into()),
        "mode3s" => Some("mode3s".into()),
        "east" | "tonpu" | "e" => Some(if seats == 3 { "mode3e" } else { "mode4e" }.into()),
        "south" | "hanchan" | "s" => Some(if seats == 3 { "mode3s" } else { "mode4s" }.into()),
        _ => None,
    }
}

/// Wall-clock timestamp string for logs/status, using local time via the
/// system clock (no extra crate). Returns seconds since epoch as a string,
/// sufficient for monotonic comparisons within one run.
pub fn now_marker() -> String {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_rooms_and_modes() {
        assert_eq!(normalize_room("copper").as_deref(), Some("铜之间"));
        assert_eq!(normalize_room("王座之间").as_deref(), Some("王座之间"));
        assert_eq!(normalize_room("???"), None);
        assert_eq!(normalize_mode("east", 4).as_deref(), Some("mode4e"));
        assert_eq!(normalize_mode("east", 3).as_deref(), Some("mode3e"));
        assert_eq!(normalize_mode("mode4s", 4).as_deref(), Some("mode4s"));
    }

    #[test]
    fn coords_inherit_main() {
        let cfg: PoolConfig = toml::from_str(
            "[[accounts]]\nid='main'\n[accounts.coords.ranked]\n\"x\" = [1.0, 2.0]\n[[accounts]]\nid='alt'\n",
        )
        .unwrap();
        let main = cfg.accounts.iter().find(|a| a.id == "main").unwrap();
        let alt = cfg.accounts.iter().find(|a| a.id == "alt").unwrap();
        // main's coords carry a nested `ranked` table in this fixture; point()
        // intentionally returns None for a table, table_point finds it.
        assert!(coords_for(&cfg, alt).is_some());
        assert!(point(&cfg, main, "ranked").is_none());
    }
}
