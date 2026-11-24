//! History storage configuration.
//!
//! Before multi-session this lived at a hard-coded `./history` next to the
//! executable. With several sessions in one process each one needs its own
//! root so `index.jsonl` appends cannot interleave.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryConfig {
    pub dir: PathBuf,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("./history"),
        }
    }
}
