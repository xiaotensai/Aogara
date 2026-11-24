//! Inspector data path — captures every WS frame, MJAI event, and bot
//! reaction into one canonical timeline.
//!
//! The Logs → Inspector tab is the only consumer; this module is the
//! plumbing that gets pipeline events out of the proxy / bridge / mjai
//! bus / bot manager and into one place.
//!
//! Design mirrors `crate::logger::stream` (and lives at the same lifetime
//! tier — owned by `Session`):
//!
//! - Two files of serialized `schema::InspectorEntry` lines:
//!   `<session>/sensitive/inspector.jsonl` holds every entry in full and is
//!   what the Inspector tab reads; `<session>/inspector.jsonl` is the
//!   shareable copy, where frames off the bridge's allowlist and unrecognized
//!   HTTP exchanges are withheld (see [`InspectorWriter::record_sensitive`]).
//! - One broadcast: `tokio::sync::broadcast::Sender<InspectorEntry>` of
//!   capacity 1024. The IPC `subscribe_inspector` command grabs a
//!   receiver per frontend channel; slow consumers see `Lagged(n)` and
//!   the forwarder injects a synthetic record so the UI reflects the gap.
//! - Single `InspectorWriter` cloned via `Arc` to every emitter
//!   (proxy/handler.rs, capture/chromium/cdp.rs, mjai_bus subscriber,
//!   bot manager). Writers serialize to disk synchronously, then
//!   `try_send` the broadcast — file write is the durable path, the bus
//!   is best-effort.
//!
//! Why disk-write before broadcast: the wire side is lossy by design
//! (broadcast capacity), the file is not. If a slow frontend drops live
//! entries, the user can still load the full session from disk.

pub mod annotate;

use crate::config::{InspectorConfig, InspectorMode};
use crate::schema::{InspectorEntry, WITHHELD};
use anyhow::Result;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// Bus type for inspector live-tail. One `Sender` per `Session`; clones
/// are handed to each emitting subsystem.
pub type InspectorBus = broadcast::Sender<InspectorEntry>;

/// Append-only sink for inspector entries.
///
/// Cheap to clone — internally `Arc`-wrapped. The writer is shared
/// across the proxy handler, chromium capture, mjai-bus subscriber, and
/// bot manager; each calls `record(...)` independently.
#[derive(Clone)]
pub struct InspectorWriter {
    inner: Arc<Inner>,
}

struct Inner {
    shareable: Mutex<BufWriter<File>>,
    full: Mutex<BufWriter<File>>,
    tx: InspectorBus,
    options: InspectorConfig,
    last_flush_shareable: Mutex<Instant>,
    last_flush_full: Mutex<Instant>,
}

impl InspectorWriter {
    /// Open both files for append and build the writer + a broadcast
    /// `Sender`. The caller passes the same `Sender` into `Session` so the
    /// IPC layer can hand out receivers later.
    pub fn open(
        shareable: &Path,
        full: &Path,
        capacity: usize,
        options: InspectorConfig,
    ) -> Result<(Self, InspectorBus)> {
        let open = |p: &Path| -> Result<BufWriter<File>> {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)?;
            Ok(BufWriter::with_capacity(64 * 1024, file))
        };
        let (tx, _) = broadcast::channel(capacity);
        let writer = Self {
            inner: Arc::new(Inner {
                shareable: Mutex::new(open(shareable)?),
                full: Mutex::new(open(full)?),
                tx: tx.clone(),
                options,
                last_flush_shareable: Mutex::new(Instant::now()),
                last_flush_full: Mutex::new(Instant::now()),
            }),
        };
        Ok((writer, tx))
    }

    fn should_record(&self, entry: &InspectorEntry) -> bool {
        match self.inner.options.mode {
            InspectorMode::Off => false,
            InspectorMode::Lite => match entry {
                InspectorEntry::MjaiEvent { .. } | InspectorEntry::BotReaction { .. } => true,
                InspectorEntry::Http { exchange, .. } => !exchange.annotations.is_empty(),
                InspectorEntry::WsFrame { .. } => self.inner.options.raw_ws,
            },
            InspectorMode::Full => match entry {
                InspectorEntry::WsFrame { .. } => self.inner.options.raw_ws,
                _ => true,
            },
        }
    }

    fn strip_http_body(entry: InspectorEntry) -> InspectorEntry {
        match entry {
            InspectorEntry::Http {
                ts_ms,
                source,
                mut exchange,
            } => {
                if let Some(body) = &mut exchange.body {
                    body.text = None;
                    body.skipped = Some(WITHHELD.to_string());
                }
                InspectorEntry::Http {
                    ts_ms,
                    source,
                    exchange,
                }
            }
            other => other,
        }
    }

    fn for_shareable(&self, entry: &InspectorEntry) -> InspectorEntry {
        let redacted = match entry {
            InspectorEntry::Http { .. } => entry.redacted_http(),
            _ => entry.clone(),
        };
        if self.inner.options.http_bodies {
            redacted
        } else {
            Self::strip_http_body(redacted)
        }
    }

    fn for_full(&self, entry: InspectorEntry) -> InspectorEntry {
        if self.inner.options.http_bodies {
            entry
        } else {
            Self::strip_http_body(entry)
        }
    }

    /// Record an entry whose content may be shared. Best-effort: failures in
    /// either the file write or the broadcast are swallowed so emit-site code
    /// stays simple (the inspector is observability, not an authoritative store).
    pub fn record(&self, entry: InspectorEntry) {
        if !self.should_record(&entry) {
            return;
        }
        let shareable = self.for_shareable(&entry);
        self.append(
            &self.inner.shareable,
            &self.inner.last_flush_shareable,
            &shareable,
        );
        self.record_full(entry);
    }

    /// Record an entry in full under `sensitive/` only; the shareable copy
    /// gets [`InspectorEntry::withheld`]. For frames off the bridge's
    /// allowlist and HTTP exchanges no recognizer understood.
    pub fn record_sensitive(&self, entry: InspectorEntry) {
        if !self.should_record(&entry) {
            return;
        }
        if let Some(stub) = entry.withheld() {
            let stub = self.for_shareable(&stub);
            self.append(
                &self.inner.shareable,
                &self.inner.last_flush_shareable,
                &stub,
            );
        }
        self.record_full(entry);
    }

    /// [`Self::record`] or [`Self::record_sensitive`], by `shareable`.
    pub fn record_as(&self, entry: InspectorEntry, shareable: bool) {
        if shareable {
            self.record(entry);
        } else {
            self.record_sensitive(entry);
        }
    }

    fn record_full(&self, entry: InspectorEntry) {
        let entry = self.for_full(entry);
        self.append(&self.inner.full, &self.inner.last_flush_full, &entry);
        if self.inner.tx.receiver_count() > 0 {
            let _ = self.inner.tx.send(entry);
        }
    }

    fn append(
        &self,
        file: &Mutex<BufWriter<File>>,
        last_flush: &Mutex<Instant>,
        entry: &InspectorEntry,
    ) {
        if let Ok(mut f) = file.lock() {
            if serde_json::to_writer(&mut *f, entry).is_ok() {
                let _ = f.write_all(b"\n");
            }
            let flush_ms = self.inner.options.flush_ms;
            if flush_ms > 0 {
                let mut last = last_flush.lock().unwrap();
                if last.elapsed() >= Duration::from_millis(flush_ms) {
                    let _ = f.flush();
                    *last = Instant::now();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        CaptureSource, FrameDirection, FrameRaw, HttpExchange, HttpHeader, HttpPhase,
        InspectorEntry, ParsedFrame,
    };
    use tempfile::TempDir;

    fn sample_frame() -> InspectorEntry {
        InspectorEntry::WsFrame {
            ts_ms: 1,
            direction: FrameDirection::Up,
            flow_id: "test:1".into(),
            size: 4,
            raw: FrameRaw::Binary("c2VjcmV0".into()),
            parsed: Some(ParsedFrame {
                method: ".lq.Lobby.login".into(),
                args: serde_json::json!({ "password": "fake-password" }),
            }),
            emitted: 0,
        }
    }

    fn sample_http() -> InspectorEntry {
        InspectorEntry::Http {
            ts_ms: 1,
            source: CaptureSource::Mitm,
            exchange: HttpExchange {
                exchange_id: None,
                phase: HttpPhase::Request,
                method: "GET".into(),
                url: "https://example.invalid/cb?code=fake-code".into(),
                host: "example.invalid".into(),
                version: "HTTP/1.1".into(),
                status: None,
                headers: vec![
                    HttpHeader {
                        name: "Cookie".into(),
                        value: "sid=fake".into(),
                    },
                    HttpHeader {
                        name: "Accept".into(),
                        value: "*/*".into(),
                    },
                ],
                body: None,
                annotations: Vec::new(),
            },
        }
    }

    fn open(tmp: &TempDir) -> (InspectorWriter, InspectorBus) {
        InspectorWriter::open(
            &tmp.path().join("inspector.jsonl"),
            &tmp.path().join("full.jsonl"),
            8,
            InspectorConfig {
                mode: InspectorMode::Full,
                flush_ms: 1,
                raw_ws: true,
                http_bodies: true,
            },
        )
        .unwrap()
    }

    fn lines(tmp: &TempDir, name: &str) -> Vec<InspectorEntry> {
        std::fs::read_to_string(tmp.path().join(name))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn record_writes_both_files_and_broadcasts() {
        let tmp = TempDir::new().unwrap();
        let (writer, tx) = open(&tmp);
        let mut rx = tx.subscribe();
        let entry = sample_frame();
        writer.record(entry.clone());

        assert_eq!(lines(&tmp, "inspector.jsonl"), vec![entry.clone()]);
        assert_eq!(lines(&tmp, "full.jsonl"), vec![entry.clone()]);
        assert_eq!(rx.try_recv().unwrap(), entry);
    }

    #[test]
    fn record_skips_broadcast_when_no_receivers() {
        // No receivers → files should still be written, broadcast no-op.
        let tmp = TempDir::new().unwrap();
        let (writer, _tx) = open(&tmp);
        writer.record(sample_frame());
        assert_eq!(lines(&tmp, "full.jsonl").len(), 1);
    }

    /// Regression: a frame off the bridge's allowlist used to land in the
    /// shareable file with its raw bytes and decoded payload.
    #[test]
    fn record_sensitive_keeps_only_metadata_in_the_shareable_file() {
        let tmp = TempDir::new().unwrap();
        let (writer, tx) = open(&tmp);
        let mut rx = tx.subscribe();
        let entry = sample_frame();
        writer.record_sensitive(entry.clone());

        assert_eq!(lines(&tmp, "full.jsonl"), vec![entry.clone()]);
        assert_eq!(
            rx.try_recv().unwrap(),
            entry,
            "the live view gets the full frame"
        );
        let shared = std::fs::read_to_string(tmp.path().join("inspector.jsonl")).unwrap();
        assert!(!shared.contains("fake-password") && !shared.contains("c2VjcmV0"));
        match &lines(&tmp, "inspector.jsonl")[..] {
            [InspectorEntry::WsFrame {
                raw: FrameRaw::Withheld,
                parsed: Some(p),
                size: 4,
                ..
            }] => {
                assert_eq!(p.method, ".lq.Lobby.login");
                assert!(p.args.is_null());
            }
            other => panic!("expected one withheld frame, got {other:?}"),
        }
    }

    #[test]
    fn record_sensitive_leaves_http_out_of_the_shareable_file() {
        let tmp = TempDir::new().unwrap();
        let (writer, _tx) = open(&tmp);
        writer.record_sensitive(sample_http());
        assert!(lines(&tmp, "inspector.jsonl").is_empty());
        assert_eq!(lines(&tmp, "full.jsonl"), vec![sample_http()]);
    }

    fn annotated(kind: &str) -> InspectorEntry {
        let mut e = sample_http();
        if let InspectorEntry::Http { exchange, .. } = &mut e {
            exchange.annotations.push(crate::schema::HttpAnnotation {
                kind: kind.into(),
                summary: String::new(),
                data: serde_json::Value::Null,
            });
            exchange.body = Some(crate::schema::HttpBody {
                text: Some("fake-body".into()),
                bytes: Some(9),
                skipped: None,
            });
        }
        e
    }

    #[test]
    fn a_recognized_exchange_keeps_its_content_but_not_credentials() {
        let tmp = TempDir::new().unwrap();
        let (writer, _tx) = open(&tmp);
        writer.record(annotated("sls_beacon"));
        let shared = std::fs::read_to_string(tmp.path().join("inspector.jsonl")).unwrap();
        assert!(!shared.contains("sid=fake"));
        assert!(
            shared.contains("*/*") && shared.contains("fake-code") && shared.contains("fake-body")
        );
        assert_eq!(lines(&tmp, "full.jsonl"), vec![annotated("sls_beacon")]);
    }

    /// Regression: an unattributed HTTP/2 response is marked `aogara_unpaired`
    /// and recorded even with `record_all` off, so its body and headers went
    /// into the shareable file.
    #[test]
    fn an_exchange_with_only_aogara_notes_keeps_only_metadata() {
        let tmp = TempDir::new().unwrap();
        let (writer, _tx) = open(&tmp);
        writer.record(annotated("aogara_unpaired"));
        let shared = std::fs::read_to_string(tmp.path().join("inspector.jsonl")).unwrap();
        for secret in ["sid=fake", "*/*", "fake-code", "fake-body"] {
            assert!(!shared.contains(secret), "{secret} leaked: {shared}");
        }
        assert!(shared.contains("example.invalid/cb") && shared.contains("aogara_unpaired"));
    }
}
