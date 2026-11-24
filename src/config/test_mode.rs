//! Test-mode configuration.
//!
//! This module is intentionally inert unless `[test].enabled = true`. The
//! production app loads the defaults (all false/zero) and never creates a
//! test logger, page monitor, or stress runner.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForceExitMode {
    /// Play the match to the end; no forced exit.
    None,
    /// Exit at the first configured phase that is observed.
    Immediate,
    /// Pick one configured phase + a random delay for each match.
    Random,
}

impl Default for ForceExitMode {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TestConfig {
    /// Master switch. When false, no test code is spawned and no test log is
    /// written. This is the only production gate.
    pub enabled: bool,
    /// Start the stress runner automatically after launch (still requires
    /// `enabled`). Useful for unattended restart tests.
    pub auto_start: bool,
    /// Resume the last unfinished run from `state.json` instead of creating a
    /// fresh run directory. A finished/stopped checkpoint starts a new run.
    pub resume: bool,
    /// How many full passes over the account list to run.
    pub rounds: u32,
    /// Slot ids or pool account ids, in execution order. Empty = every live
    /// session, sorted by slot (`slot1`, `slot2`, …).
    pub accounts: Vec<String>,
    /// Room/mode override. Empty strings inherit the session's own target.
    pub room: String,
    pub mode: String,
    /// Forced-exit policy for each match.
    pub force_exit: ForceExitMode,
    /// Phases where a forced exit may happen. Recognised values:
    /// `page_load`, `login`, `lobby`, `entering`, `in_game`, `settlement`.
    pub exit_phases: Vec<String>,
    /// Random delay range after the selected phase is first observed.
    pub exit_delay_min_ms: u64,
    pub exit_delay_max_ms: u64,
    /// Page-state polling interval per browser.
    pub monitor_interval_ms: u64,
    /// Emit a heartbeat event this often even when nothing changed.
    pub heartbeat_secs: u64,
    /// Safety cap for one match. On expiry the runner force-exits it and
    /// records a `match_timeout` event.
    pub max_match_secs: u64,
    /// Root for `run-<timestamp>/events.jsonl`, `state.json`, `summary.json`.
    pub log_dir: PathBuf,
    /// Optional deterministic seed for random phase/delay selection.
    pub seed: Option<u64>,
}

impl Default for TestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_start: false,
            resume: true,
            rounds: 3,
            accounts: Vec::new(),
            room: String::new(),
            mode: String::new(),
            force_exit: ForceExitMode::None,
            exit_phases: vec![
                "page_load".to_string(),
                "login".to_string(),
                "lobby".to_string(),
                "entering".to_string(),
                "in_game".to_string(),
                "settlement".to_string(),
            ],
            exit_delay_min_ms: 200,
            exit_delay_max_ms: 4_000,
            // WebGL pixel probes call readPixels, which is a GPU sync point.
            // 1s keeps stress logs useful without queueing five browsers.
            monitor_interval_ms: 1_000,
            heartbeat_secs: 10,
            max_match_secs: 900,
            log_dir: PathBuf::from("./logs/test"),
            seed: None,
        }
    }
}

impl TestConfig {
    /// Normalised phase list, with unknown values dropped and duplicates
    /// removed while preserving order.
    pub fn phases(&self) -> Vec<String> {
        let mut out = Vec::new();
        for phase in &self.exit_phases {
            let p = phase.trim().to_ascii_lowercase();
            if matches!(
                p.as_str(),
                "page_load" | "login" | "lobby" | "entering" | "in_game" | "settlement"
            ) && !out.contains(&p)
            {
                out.push(p);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_inert() {
        let c = TestConfig::default();
        assert!(!c.enabled);
        assert!(!c.auto_start);
        assert_eq!(c.force_exit, ForceExitMode::None);
        assert_eq!(c.rounds, 3);
    }

    #[test]
    fn phases_are_filtered_and_deduplicated() {
        let c = TestConfig {
            exit_phases: vec![
                "in_game".into(),
                "IN_GAME".into(),
                "bogus".into(),
                "lobby".into(),
                " in_game ".into(),
            ],
            ..TestConfig::default()
        };
        assert_eq!(c.phases(), vec!["in_game", "lobby"]);
    }
}
