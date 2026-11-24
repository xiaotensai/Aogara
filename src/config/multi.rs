//! Multi-session configuration.
//!
//! The main `config.toml` stays the launch configuration (and the first
//! session). Additional sessions are listed here and each points at its own
//! full config file, so the existing `AppConfig` loader and per-session
//! `update_config` persistence keep working unchanged.
//!
//! ```toml
//! [multi]
//! max_sessions = 8
//! start_on_launch = true
//!
//! [[multi.sessions]]
//! slot = "slot2"
//! config = "<安装目录>/app/configs/slot-alt3.toml"
//! enabled = true
//! ```

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MultiConfig {
    /// Hard upper bound. The UI exposes 2–8; values above 8 are clamped.
    pub max_sessions: usize,
    /// Start every `enabled` session when Aogara launches (legacy, ignored).
    pub start_on_launch: bool,
    /// Open the game browser(s) when Aogara launches. Off by default: the app
    /// boots headless and each session's browser is opened on demand from the
    /// 多开会话 page. This is a single process-wide switch.
    pub start_capture_on_launch: bool,
    /// Close an idle session's browser after this many seconds. 0 disables.
    /// "Idle" means no active match, no autoplay and auto-next is off.
    pub close_idle_browser_secs: u64,
    /// Extra sessions. The main `config.toml` is always session 1.
    pub sessions: Vec<MultiSession>,
}

impl Default for MultiConfig {
    fn default() -> Self {
        Self {
            max_sessions: 8,
            start_on_launch: true,
            start_capture_on_launch: false,
            close_idle_browser_secs: 0,
            sessions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MultiSession {
    /// `slot1` … `slot8`.
    pub slot: String,
    /// Absolute path to the session's own config.toml.
    pub config: String,
    pub enabled: bool,
}

impl Default for MultiSession {
    fn default() -> Self {
        Self {
            slot: String::new(),
            config: String::new(),
            enabled: false,
        }
    }
}
