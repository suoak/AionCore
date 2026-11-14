//! Durable NDJSON event journal for headless Host resume across process restart (M9 E14).
//!
//! Brand: CSBU WorkMate. No AGPL.
//!
//! Retention is bounded twice: by count (the last `keep` events) and by bytes
//! ([`JournalLimits`]). Events larger than `max_event_bytes` are not written;
//! an `omitted` marker records their cursor instead, so sequence numbers stay
//! monotonic across a restart and a resuming client sees a `gap`, never a silent
//! hole. If an append fails, the journal stops writing for the rest of the
//! process and bumps the stored epoch, so the next start cannot reuse sequence
//! numbers that clients have already seen.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use aionui_api_types::{EventCursor, WebSocketMessage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

const EPOCH_FILE: &str = "epoch";
const JOURNAL_FILE: &str = "events.ndjson";

/// Default cap on the journal file size (compaction keeps it at or below half).
pub const DEFAULT_JOURNAL_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Default cap on one serialized event; larger events are memory-only.
pub const DEFAULT_JOURNAL_MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

/// Byte bounds for the on-disk journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalLimits {
    /// Compact when the file grows past this; a file found larger than twice
    /// this on open is discarded (and the epoch bumped) instead of being read.
    pub max_bytes: u64,
    /// Serialized events larger than this are not journaled.
    pub max_event_bytes: usize,
}

impl Default for JournalLimits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_JOURNAL_MAX_BYTES,
            max_event_bytes: DEFAULT_JOURNAL_MAX_EVENT_BYTES,
        }
    }
}

impl JournalLimits {
    fn normalized(self) -> Self {
        let max_event_bytes = self.max_event_bytes.max(64);
        Self {
            max_bytes: self.max_bytes.max(max_event_bytes as u64 * 2),
            max_event_bytes,
        }
    }
}

/// Journal line recording that the event at `omitted` was not persisted.
#[derive(Debug, Serialize, Deserialize)]
struct OmittedMarker {
    omitted: EventCursor,
}

/// On-disk companion to [`crate::EventLog`]: persists epoch + recent envelopes.
#[derive(Debug)]
pub struct EventJournal {
    dir: PathBuf,
    keep: usize,
    limits: JournalLimits,
    appends_since_compact: usize,
    /// The epoch file existed before this open (a previous Host run).
    reopened: bool,
    /// An append failed; no further writes this process.
    failed: bool,
}

#[derive(Debug, Clone)]
pub struct JournalSnapshot {
    pub epoch: u64,
    pub events: Vec<WebSocketMessage<Value>>,
    /// Highest sequence recorded for `epoch` (events and omitted markers);
    /// 0 when nothing was recorded.
    pub last_sequence: u64,
}

fn invalid_data(err: impl std::error::Error + Send + Sync + 'static) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, err)
}

impl EventJournal {
    /// Open (or create) a journal under `dir` with default byte limits. `keep`
    /// bounds how many events are retained after compaction (and loaded on open).
    pub fn open(dir: impl AsRef<Path>, keep: usize) -> std::io::Result<Self> {
        Self::open_with_limits(dir, keep, JournalLimits::default())
    }

    pub fn open_with_limits(dir: impl AsRef<Path>, keep: usize, limits: JournalLimits) -> std::io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let keep = keep.max(1);
        let limits = limits.normalized();
        fs::create_dir_all(&dir)?;
        let epoch_path = dir.join(EPOCH_FILE);
        let reopened = epoch_path.exists();
        if !reopened {
            fs::write(&epoch_path, b"1\n")?;
        }
        let journal = Self {
            dir,
            keep,
            limits,
            appends_since_compact: 0,
            reopened,
            failed: false,
        };
        let journal_path = journal.journal_path();
        match fs::metadata(&journal_path) {
            Ok(meta) if meta.len() > limits.max_bytes.saturating_mul(2) => {
                warn!(
                    bytes = meta.len(),
                    "Event journal exceeds its size bound; discarding it and starting a new epoch"
                );
                let epoch = journal.read_epoch().unwrap_or(1);
                journal.start_new_epoch(epoch)?;
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                File::create(&journal_path)?;
            }
            Err(err) => return Err(err),
        }
        Ok(journal)
    }

    fn journal_path(&self) -> PathBuf {
        self.dir.join(JOURNAL_FILE)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn limits(&self) -> JournalLimits {
        self.limits
    }

    /// True when the epoch file was left by a previous Host run.
    pub fn reopened(&self) -> bool {
        self.reopened
    }

    /// True once an append has failed; the journal no longer writes.
    pub fn has_failed(&self) -> bool {
        self.failed
    }

    pub fn read_epoch(&self) -> std::io::Result<u64> {
        let raw = fs::read_to_string(self.dir.join(EPOCH_FILE))?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(1);
        }
        trimmed.parse::<u64>().map(|e| e.max(1)).map_err(invalid_data)
    }

    pub fn write_epoch(&self, epoch: u64) -> std::io::Result<()> {
        fs::write(self.dir.join(EPOCH_FILE), format!("{}\n", epoch.max(1)))
    }

    /// Persist `after + 1` as the epoch and empty the journal. Returns the new epoch.
    pub fn start_new_epoch(&self, after: u64) -> std::io::Result<u64> {
        let epoch = after.max(1).saturating_add(1);
        self.write_epoch(epoch)?;
        File::create(self.journal_path())?;
        Ok(epoch)
    }

    /// Load up to `keep` most recent valid envelopes for the journal's epoch,
    /// within the byte budget. Lines longer than `max_event_bytes` (plus slack
    /// for markers) are skipped without being buffered whole.
    pub fn load_snapshot(&self) -> std::io::Result<JournalSnapshot> {
        let epoch = self.read_epoch()?;
        let file = File::open(self.journal_path())?;
        let mut reader = BufReader::new(file);
        let line_cap = self.limits.max_event_bytes as u64 + 1024;
        let mut events = Vec::new();
        let mut last_sequence = 0u64;
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = (&mut reader).take(line_cap + 1).read_until(b'\n', &mut line)?;
            if read == 0 {
                break;
            }
            if line.last() != Some(&b'\n') && read as u64 > line_cap {
                warn!("Skipping oversized event journal line");
                // Drain the rest of the line without buffering it.
                let mut sink = Vec::new();
                loop {
                    sink.clear();
                    let n = (&mut reader).take(64 * 1024).read_until(b'\n', &mut sink)?;
                    if n == 0 || sink.last() == Some(&b'\n') {
                        break;
                    }
                }
                continue;
            }
            let trimmed = line.trim_ascii();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_slice::<WebSocketMessage<Value>>(trimmed) {
                Ok(msg) => {
                    if let Some(c) = msg.cursor
                        && c.epoch == epoch
                    {
                        last_sequence = last_sequence.max(c.sequence);
                        events.push(msg);
                    }
                }
                Err(err) => match serde_json::from_slice::<OmittedMarker>(trimmed) {
                    Ok(marker) => {
                        if marker.omitted.epoch == epoch {
                            last_sequence = last_sequence.max(marker.omitted.sequence);
                        }
                    }
                    Err(_) => warn!(error = %err, "Skipping corrupt event journal line"),
                },
            }
        }
        if events.len() > self.keep {
            events = events.split_off(events.len() - self.keep);
        }
        Ok(JournalSnapshot {
            epoch,
            events,
            last_sequence,
        })
    }

    /// Append one stamped envelope. Compacts every `keep` appends and whenever
    /// the file grows past `max_bytes`. After a failed write the journal bumps
    /// the stored epoch (best effort) and stops writing.
    pub fn append(&mut self, event: &WebSocketMessage<Value>) -> std::io::Result<()> {
        if self.failed {
            return Ok(());
        }
        match self.try_append(event) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.failed = true;
                let epoch = event.cursor.map(|c| c.epoch).unwrap_or(1);
                if let Err(bump) = self.write_epoch(epoch.max(1).saturating_add(1)) {
                    warn!(error = %bump, "Failed to retire the event journal epoch after a write error");
                }
                Err(err)
            }
        }
    }

    fn try_append(&mut self, event: &WebSocketMessage<Value>) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(event).map_err(invalid_data)?;
        if line.len() > self.limits.max_event_bytes {
            warn!(
                name = %event.name,
                bytes = line.len(),
                "Event too large for the durable journal; recording an omitted marker"
            );
            let cursor = event.cursor.unwrap_or_default();
            line = serde_json::to_vec(&OmittedMarker { omitted: cursor }).map_err(invalid_data)?;
        }
        line.push(b'\n');
        let path = self.journal_path();
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        file.write_all(&line)?;
        file.flush()?;
        let len = file.metadata()?.len();
        drop(file);
        self.appends_since_compact = self.appends_since_compact.saturating_add(1);
        if self.appends_since_compact >= self.keep || len > self.limits.max_bytes {
            self.compact()?;
            self.appends_since_compact = 0;
        }
        Ok(())
    }

    /// Rewrite the journal keeping only the newest same-epoch events: at most
    /// `keep`, and at most half of `max_bytes` so compaction is not repeated on
    /// every append. The highest recorded sequence is preserved with a marker
    /// when the newest record was an omitted event.
    pub fn compact(&self) -> std::io::Result<()> {
        let snap = self.load_snapshot()?;
        let budget = self.limits.max_bytes / 2;
        let mut lines: Vec<Vec<u8>> = Vec::with_capacity(snap.events.len());
        let mut total = 0u64;
        for ev in snap.events.iter().rev() {
            let mut line = serde_json::to_vec(ev).map_err(invalid_data)?;
            line.push(b'\n');
            if total + line.len() as u64 > budget {
                break;
            }
            total += line.len() as u64;
            lines.push(line);
        }
        lines.reverse();
        let newest_kept = snap
            .events
            .last()
            .and_then(|e| e.cursor)
            .map(|c| c.sequence)
            .unwrap_or(0);
        let tmp = self.dir.join(format!("{JOURNAL_FILE}.tmp"));
        {
            let mut file = File::create(&tmp)?;
            for line in &lines {
                file.write_all(line)?;
            }
            if snap.last_sequence > newest_kept {
                let marker = OmittedMarker {
                    omitted: EventCursor::new(snap.epoch, snap.last_sequence),
                };
                serde_json::to_writer(&mut file, &marker).map_err(invalid_data)?;
                file.write_all(b"\n")?;
            }
            file.flush()?;
        }
        fs::rename(&tmp, self.journal_path())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::EventCursor;
    use serde_json::json;

    fn tmp_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "workmate-event-journal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    #[test]
    fn open_persists_epoch_and_roundtrips_events() {
        let dir = tmp_dir();
        let mut journal = EventJournal::open(&dir, 8).unwrap();
        assert_eq!(journal.read_epoch().unwrap(), 1);
        let a = WebSocketMessage::new("a", json!({})).with_cursor(EventCursor::new(1, 1));
        let b = WebSocketMessage::new("b", json!({})).with_cursor(EventCursor::new(1, 2));
        journal.append(&a).unwrap();
        journal.append(&b).unwrap();
        drop(journal);

        let journal = EventJournal::open(&dir, 8).unwrap();
        let snap = journal.load_snapshot().unwrap();
        assert_eq!(snap.epoch, 1);
        assert_eq!(snap.events.len(), 2);
        assert_eq!(snap.events[0].name, "a");
        assert_eq!(snap.events[1].name, "b");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compact_keeps_last_n() {
        let dir = tmp_dir();
        let mut journal = EventJournal::open(&dir, 2).unwrap();
        for i in 1..=5 {
            let ev = WebSocketMessage::new(format!("e{i}"), json!({})).with_cursor(EventCursor::new(1, i));
            journal.append(&ev).unwrap();
        }
        journal.compact().unwrap();
        let snap = journal.load_snapshot().unwrap();
        assert_eq!(snap.events.len(), 2);
        assert_eq!(snap.events[0].name, "e4");
        assert_eq!(snap.events[1].name, "e5");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compaction_respects_byte_budget() {
        let dir = tmp_dir();
        let limits = JournalLimits {
            max_bytes: 4096,
            max_event_bytes: 512,
        };
        let mut journal = EventJournal::open_with_limits(&dir, 1000, limits).unwrap();
        for i in 1..=200u64 {
            let ev = WebSocketMessage::new(format!("e{i}"), json!({ "pad": "y".repeat(100) }))
                .with_cursor(EventCursor::new(1, i));
            journal.append(&ev).unwrap();
            let len = fs::metadata(dir.join(JOURNAL_FILE)).unwrap().len();
            assert!(len <= limits.max_bytes, "journal grew to {len}");
        }
        let snap = journal.load_snapshot().unwrap();
        assert_eq!(snap.last_sequence, 200);
        assert_eq!(snap.events.last().unwrap().name, "e200");
        let seqs: Vec<u64> = snap.events.iter().map(|e| e.cursor.unwrap().sequence).collect();
        assert!(
            seqs.windows(2).all(|w| w[1] == w[0] + 1),
            "kept events must be the newest run"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_event_becomes_marker_and_survives_compaction() {
        let dir = tmp_dir();
        let limits = JournalLimits {
            max_bytes: 64 * 1024,
            max_event_bytes: 256,
        };
        let mut journal = EventJournal::open_with_limits(&dir, 8, limits).unwrap();
        let small = WebSocketMessage::new("small", json!({})).with_cursor(EventCursor::new(1, 1));
        let big = WebSocketMessage::new("big", json!({ "blob": "x".repeat(1024) })).with_cursor(EventCursor::new(1, 2));
        journal.append(&small).unwrap();
        journal.append(&big).unwrap();
        let raw = fs::read_to_string(dir.join(JOURNAL_FILE)).unwrap();
        assert!(!raw.contains("xxxx"), "oversized payload must not be written");
        let snap = journal.load_snapshot().unwrap();
        assert_eq!(snap.events.len(), 1);
        assert_eq!(snap.last_sequence, 2);
        journal.compact().unwrap();
        let snap = journal.load_snapshot().unwrap();
        assert_eq!(snap.events.len(), 1);
        assert_eq!(snap.last_sequence, 2, "compaction keeps the high-water mark");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_line_is_skipped_on_load() {
        let dir = tmp_dir();
        let limits = JournalLimits {
            max_bytes: 64 * 1024,
            max_event_bytes: 256,
        };
        let journal = EventJournal::open_with_limits(&dir, 8, limits).unwrap();
        let ok = WebSocketMessage::new("ok", json!({})).with_cursor(EventCursor::new(1, 2));
        let long = format!("{{\"name\":\"huge\",\"data\":\"{}\"}}", "z".repeat(4096));
        let content = format!("{long}\n{}\n", serde_json::to_string(&ok).unwrap());
        fs::write(dir.join(JOURNAL_FILE), content).unwrap();
        let snap = journal.load_snapshot().unwrap();
        assert_eq!(snap.events.len(), 1);
        assert_eq!(snap.events[0].name, "ok");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn journal_far_over_bound_is_discarded_with_new_epoch() {
        let dir = tmp_dir();
        let limits = JournalLimits {
            max_bytes: 1024,
            max_event_bytes: 256,
        };
        drop(EventJournal::open_with_limits(&dir, 8, limits).unwrap());
        fs::write(dir.join(JOURNAL_FILE), vec![b'x'; 4096]).unwrap();
        let journal = EventJournal::open_with_limits(&dir, 8, limits).unwrap();
        assert_eq!(journal.read_epoch().unwrap(), 2);
        assert_eq!(fs::metadata(dir.join(JOURNAL_FILE)).unwrap().len(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_append_retires_epoch_and_stops_writing() {
        let dir = tmp_dir();
        let mut journal = EventJournal::open(&dir, 8).unwrap();
        let a = WebSocketMessage::new("a", json!({})).with_cursor(EventCursor::new(1, 1));
        journal.append(&a).unwrap();
        // Make the journal path unwritable as a file.
        fs::remove_file(dir.join(JOURNAL_FILE)).unwrap();
        fs::create_dir(dir.join(JOURNAL_FILE)).unwrap();
        let b = WebSocketMessage::new("b", json!({})).with_cursor(EventCursor::new(1, 2));
        assert!(journal.append(&b).is_err());
        assert!(journal.has_failed());
        assert_eq!(journal.read_epoch().unwrap(), 2, "next start must not reuse epoch 1");
        let c = WebSocketMessage::new("c", json!({})).with_cursor(EventCursor::new(1, 3));
        assert!(journal.append(&c).is_ok(), "later appends are skipped, not retried");
        let _ = fs::remove_dir_all(&dir);
    }
}
