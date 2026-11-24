use serde::{Deserialize, Serialize};

/// One account in the rotation pool.
///
/// `id` is the stable key the frontend selects by. `label` is the account's
/// real nickname (shown in the UI), `uid` the numeric Majsoul id used to
/// verify login state. Both are displayed; `uid` is also the authoritative
/// cross-check after a switch (nickname alone can collide).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PoolAccount {
    pub id: String,
    pub label: String,
    pub uid: String,
    /// Login email (lowercase). Sent into the Unity login form via CDP keys.
    /// Stored here in plaintext per the chosen design — see SECURITY note in
    /// the integration doc.
    pub email: String,
    /// Login password, plaintext (same note as `email`).
    pub password: String,
    /// Optional non-email login identifier. Majsoul stores the account name
    /// in `localStorage.account`; for accounts registered with a username the
    /// value is the username, not the email, so login-state checks must accept
    /// both. Empty = email only.
    #[serde(default)]
    pub login_name: Option<String>,
    /// Account-specific canvas click points (normalised 0..1 relative to the
    /// game canvas). Keys: `login_email`, `login_password`, `login_submit`,
    /// `settings`, `logout`, `logout_confirm`, `ranked`, `back`; plus a nested
    /// `rooms` table. Normalised coords work at any window size via
    /// `CanvasRect::pixel`, matching autoplay's non-coordinate-dependent clicks.
    pub coords: std::collections::BTreeMap<String, PoolCoord>,
}

/// A click target: either a single normalised point `[nx, ny]`, or a nested
/// table (used for `rooms`, keyed by room name). Kept flexible so one field
/// can carry both shape forms from the existing accounts.json data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PoolCoord {
    Point([f64; 2]),
    Table(std::collections::BTreeMap<String, [f64; 2]>),
}

impl PoolCoord {
    pub fn point(&self) -> Option<[f64; 2]> {
        match self {
            PoolCoord::Point(p) => Some(*p),
            _ => None,
        }
    }
    pub fn table_get(&self, key: &str) -> Option<[f64; 2]> {
        match self {
            PoolCoord::Table(t) => t.get(key).copied(),
            _ => None,
        }
    }
}

impl Default for PoolAccount {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            uid: String::new(),
            email: String::new(),
            password: String::new(),
            login_name: None,
            coords: std::collections::BTreeMap::new(),
        }
    }
}

/// Room + mode target for auto-enter. Mirrors the external bridge's
/// `normalizeMode`/`normalizeRoom`; values are the canonical keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PoolTarget {
    /// Room key, e.g. `copper`/`silver`/`gold`/`jade`/`throne`.
    pub room: String,
    /// Mode key: `mode4e` / `mode4s` / `mode3e` / `mode3s`.
    pub mode: String,
}

impl Default for PoolTarget {
    fn default() -> Self {
        Self {
            room: String::new(),
            mode: String::new(),
        }
    }
}

/// Native account-pool config. Persisted in `[pool]` of config.toml and
/// mirrored to the frontend via `get_config` / `update_config`.
///
/// All fields are runtime-tunable: the pool task re-reads this section every
/// iteration (like `AutoplayConfig::enabled`), so toggling from the UI takes
/// effect without restarting Aogara.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PoolConfig {
    /// The accounts available for rotation.
    pub accounts: Vec<PoolAccount>,
    /// Currently selected account id (empty = none).
    pub selected_id: String,
    /// Target room/mode for auto-enter.
    pub target: PoolTarget,
    /// Auto-start the next game when the current one finishes.
    pub auto_next: bool,
    /// Timed stop: enabled flag + duration in hours.
    pub timer_enabled: bool,
    pub timer_hours: f64,
    /// Launch the game browser(s) when Aogara starts. Off by default: the app
    /// boots headless and each session's browser is opened on demand (first
    /// `session_pool_start`, or the switch on the pool page).
    pub start_capture_on_launch: bool,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            selected_id: String::new(),
            target: PoolTarget::default(),
            auto_next: false,
            timer_enabled: false,
            timer_hours: 4.0,
            start_capture_on_launch: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_section_gets_defaults() {
        let c: PoolConfig = toml::from_str("").unwrap();
        assert!(c.accounts.is_empty());
        assert!(!c.auto_next);
        assert_eq!(c.timer_hours, 4.0);
    }

    #[test]
    fn partial_section_fills_remaining_defaults() {
        let c: PoolConfig = toml::from_str("auto_next = true\n[[accounts]]\nid = \"a\"\nlabel = \"昵称\"\nuid = \"123\"\n").unwrap();
        assert!(c.auto_next);
        assert_eq!(c.accounts.len(), 1);
        assert_eq!(c.accounts[0].label, "昵称");
        assert_eq!(c.accounts[0].uid, "123");
        assert_eq!(c.timer_hours, 4.0);
    }
}
