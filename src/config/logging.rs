use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectorMode {
    /// Do not write or broadcast inspector entries at all.
    Off,
    /// Keep game events, bot reactions and annotated HTTP metadata only.
    Lite,
    /// Current behaviour: everything the capture backends emit.
    Full,
}

impl Default for InspectorMode {
    fn default() -> Self {
        Self::Lite
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InspectorConfig {
    pub mode: InspectorMode,
    /// Flush buffered JSONL at most this often. 0 disables periodic flush
    /// (data still flushes when the buffer fills or the session drops).
    pub flush_ms: u64,
    /// Keep raw WebSocket frames. Off in lite mode by design.
    pub raw_ws: bool,
    /// Keep HTTP bodies. Metadata and annotations are always kept in lite mode.
    pub http_bodies: bool,
}

impl Default for InspectorConfig {
    fn default() -> Self {
        Self {
            mode: InspectorMode::Lite,
            flush_ms: 500,
            raw_ws: false,
            http_bodies: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub dir: PathBuf,
    pub level: String,
    pub all_level: String,
    pub inspector: InspectorConfig,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("./logs"),
            level: "info".to_string(),
            all_level: "info".to_string(),
            inspector: InspectorConfig::default(),
        }
    }
}
