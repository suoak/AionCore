//! Epoch/sequence event ring for headless Host reconnect (E14).
//!
//! Brand: CSBU WorkMate. No AGPL.
//! Optional durable NDJSON journal keeps the same epoch across Host restarts
//! so clients can resume without a gap when events are still retained.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use aionui_api_types::{EventCursor, WebSocketMessage};
use serde_json::Value;
use tracing::warn;

use crate::event_journal::{EventJournal, JournalLimits, JournalSnapshot};

/// Default ring capacity (recent live events only).
pub const DEFAULT_EVENT_LOG_CAPACITY: usize = 512;

/// Outcome of a resume request against the in-memory ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeStatus {
    /// Same epoch; events with `sequence > after.sequence` were available.
    Ok,
    /// Client cursor epoch differs from the Host (restart without journal /
    /// rotation), or the ring/journal evicted needed events.
    Gap,
}

#[derive(Debug, Clone)]
pub struct ResumeSlice {
    pub status: ResumeStatus,
    pub current: EventCursor,
    pub events: Vec<WebSocketMessage<Value>>,
}

/// Process-local ring of recently broadcast WebSocket envelopes, stamped with
/// [`EventCursor`]. When opened via [`EventLog::open_durable`], the epoch is
/// reused across Host restarts and recent events are rehydrated from disk.
pub struct EventLog {
    epoch: u64,
    next_sequence: AtomicU64,
    capacity: usize,
    ring: Mutex<VecDeque<WebSocketMessage<Value>>>,
    journal: Option<Mutex<EventJournal>>,
}

impl EventLog {
    /// Create a log with `capacity` slots. Epoch starts at `1` (0 reserved for
    /// "unset / legacy client"). In-memory only — Host restart starts a new
    /// epoch unless [`Self::open_durable`] is used.
    pub fn new(capacity: usize) -> Self {
        Self::with_epoch(1, capacity)
    }

    pub fn with_epoch(epoch: u64, capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            epoch: epoch.max(1),
            next_sequence: AtomicU64::new(1),
            capacity,
            ring: Mutex::new(VecDeque::with_capacity(capacity)),
            journal: None,
        }
    }

    /// Open a durable log under `dir` (`epoch` + `events.ndjson`). Reuses the
    /// stored epoch and rehydrates up to `capacity` events so WS resume can
    /// succeed after Host process restart.
    ///
    /// Sequence numbers continue from the highest one the journal recorded. When
    /// a previous run left the epoch but no record of it (journal emptied,
    /// discarded or corrupt), continuity cannot be proven, so a new epoch starts
    /// and resuming clients get `gap` instead of reused sequence numbers.
    pub fn open_durable(dir: impl AsRef<Path>, capacity: usize) -> std::io::Result<Self> {
        Self::open_durable_with_limits(dir, capacity, JournalLimits::default())
    }

    pub fn open_durable_with_limits(
        dir: impl AsRef<Path>,
        capacity: usize,
        limits: JournalLimits,
    ) -> std::io::Result<Self> {
        let capacity = capacity.max(1);
        let journal = EventJournal::open_with_limits(dir.as_ref(), capacity, limits)?;
        let mut snap = journal.load_snapshot()?;
        if journal.reopened() && snap.last_sequence == 0 {
            let epoch = journal.start_new_epoch(snap.epoch)?;
            snap = JournalSnapshot {
                epoch,
                events: Vec::new(),
                last_sequence: 0,
            };
        }
        let epoch = snap.epoch.max(1);
        let next_sequence = snap.last_sequence.saturating_add(1).max(1);
        let mut ring = VecDeque::with_capacity(capacity);
        for ev in snap.events {
            if ring.len() >= capacity {
                ring.pop_front();
            }
            ring.push_back(ev);
        }
        Ok(Self {
            epoch,
            next_sequence: AtomicU64::new(next_sequence),
            capacity,
            ring: Mutex::new(ring),
            journal: Some(Mutex::new(journal)),
        })
    }

    /// Best-effort durable open: falls back to an in-memory log on I/O errors.
    pub fn open_durable_or_memory(dir: impl AsRef<Path>, capacity: usize) -> Self {
        match Self::open_durable(dir.as_ref(), capacity) {
            Ok(log) => log,
            Err(err) => {
                warn!(
                    error = %err,
                    path = %dir.as_ref().display(),
                    "Failed to open durable event journal; using in-memory EventLog"
                );
                Self::new(capacity)
            }
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn is_durable(&self) -> bool {
        self.journal.is_some()
    }

    /// Last assigned cursor (sequence 0 when nothing has been published yet).
    pub fn current_cursor(&self) -> EventCursor {
        let seq = self.next_sequence.load(Ordering::Relaxed).saturating_sub(1);
        EventCursor::new(self.epoch, seq)
    }

    /// Stamp `event` with the next Host cursor (always Host-owned), append a
    /// clone to the ring (and durable journal when configured), and return the
    /// stamped envelope for broadcast.
    pub fn record(&self, mut event: WebSocketMessage<Value>) -> WebSocketMessage<Value> {
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        event.cursor = Some(EventCursor::new(self.epoch, sequence));
        // Best-effort durable append; never fail the in-memory publish path.
        if let Some(journal) = &self.journal
            && let Ok(mut j) = journal.lock()
            && let Err(err) = j.append(&event)
        {
            warn!(error = %err, "Failed to append event to durable journal");
        }
        {
            let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
            if ring.len() >= self.capacity {
                ring.pop_front();
            }
            ring.push_back(event.clone());
        }
        event
    }

    /// Events strictly after `after` in the current epoch.
    ///
    /// The status is `Ok` only when the replay is exactly the contiguous run
    /// `after.sequence + 1 ..= current.sequence`. Anything else is a `Gap`: a
    /// different epoch (replay all retained events), events evicted from the
    /// ring or never journaled, or a client cursor ahead of the Host.
    pub fn events_after(&self, after: EventCursor) -> ResumeSlice {
        let current = self.current_cursor();
        let ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());

        if after.epoch != self.epoch {
            let events: Vec<_> = ring.iter().cloned().collect();
            return ResumeSlice {
                status: ResumeStatus::Gap,
                current,
                events,
            };
        }

        let events: Vec<_> = ring
            .iter()
            .filter(|ev| match ev.cursor {
                Some(c) => after.precedes(c),
                None => false,
            })
            .cloned()
            .collect();

        let mut expected = after.sequence.saturating_add(1);
        let mut contiguous = after.sequence <= current.sequence;
        for ev in &events {
            match ev.cursor {
                Some(c) if c.sequence == expected => expected = expected.saturating_add(1),
                _ => {
                    contiguous = false;
                    break;
                }
            }
        }
        if contiguous && expected != current.sequence.saturating_add(1) {
            contiguous = false;
        }

        ResumeSlice {
            status: if contiguous {
                ResumeStatus::Ok
            } else {
                ResumeStatus::Gap
            },
            current,
            events,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    #[test]
    fn record_stamps_monotonic_sequence() {
        let log = EventLog::new(8);
        let a = log.record(WebSocketMessage::new("a", json!({})));
        let b = log.record(WebSocketMessage::new("b", json!({})));
        assert_eq!(a.cursor, Some(EventCursor::new(1, 1)));
        assert_eq!(b.cursor, Some(EventCursor::new(1, 2)));
        assert_eq!(log.current_cursor(), EventCursor::new(1, 2));
    }

    #[test]
    fn events_after_same_epoch_filters() {
        let log = EventLog::new(8);
        log.record(WebSocketMessage::new("a", json!({})));
        log.record(WebSocketMessage::new("b", json!({})));
        log.record(WebSocketMessage::new("c", json!({})));
        let slice = log.events_after(EventCursor::new(1, 1));
        assert_eq!(slice.status, ResumeStatus::Ok);
        assert_eq!(slice.events.len(), 2);
        assert_eq!(slice.events[0].name, "b");
        assert_eq!(slice.events[1].name, "c");
    }

    #[test]
    fn epoch_mismatch_is_gap_and_replays_retained() {
        let log = EventLog::with_epoch(3, 8);
        log.record(WebSocketMessage::new("x", json!({})));
        let slice = log.events_after(EventCursor::new(1, 99));
        assert_eq!(slice.status, ResumeStatus::Gap);
        assert_eq!(slice.events.len(), 1);
        assert_eq!(slice.current.epoch, 3);
    }

    #[test]
    fn ring_evicts_oldest() {
        let log = EventLog::new(2);
        log.record(WebSocketMessage::new("1", json!({})));
        log.record(WebSocketMessage::new("2", json!({})));
        log.record(WebSocketMessage::new("3", json!({})));
        let slice = log.events_after(EventCursor::default());
        assert_eq!(slice.status, ResumeStatus::Gap);
        assert_eq!(slice.events.len(), 2);
        assert_eq!(slice.events[0].name, "2");
        assert_eq!(slice.events[1].name, "3");
    }

    #[test]
    fn eviction_gap_when_cursor_behind_oldest() {
        let log = EventLog::new(2);
        log.record(WebSocketMessage::new("1", json!({})));
        log.record(WebSocketMessage::new("2", json!({})));
        log.record(WebSocketMessage::new("3", json!({})));
        // Client last saw seq 1; ring now holds 2,3 — contiguous after 1 → Ok
        let contiguous = log.events_after(EventCursor::new(1, 1));
        assert_eq!(contiguous.status, ResumeStatus::Ok);
        assert_eq!(contiguous.events.len(), 2);
        // Client still at default/0 while oldest is 2 → gap (seq 1 dropped)
        let gapped = log.events_after(EventCursor::new(1, 0));
        assert_eq!(gapped.status, ResumeStatus::Gap);
    }

    #[test]
    fn overwrites_pre_stamped_cursor_with_host_epoch() {
        let log = EventLog::new(4);
        let stamped = WebSocketMessage::new("pre", json!({})).with_cursor(EventCursor::new(9, 7));
        let out = log.record(stamped);
        assert_eq!(out.cursor, Some(EventCursor::new(1, 1)));
        let auto = log.record(WebSocketMessage::new("auto", json!({})));
        assert_eq!(auto.cursor, Some(EventCursor::new(1, 2)));
    }

    #[test]
    fn durable_reuses_epoch_across_reopen() {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "workmate-event-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let log = EventLog::open_durable(&dir, 8).unwrap();
        assert!(log.is_durable());
        log.record(WebSocketMessage::new("a", json!({})));
        log.record(WebSocketMessage::new("b", json!({})));
        let cursor = log.current_cursor();
        drop(log);

        let log2 = EventLog::open_durable(&dir, 8).unwrap();
        assert_eq!(log2.epoch(), cursor.epoch);
        assert_eq!(log2.current_cursor(), cursor);
        let slice = log2.events_after(EventCursor::new(cursor.epoch, 1));
        assert_eq!(slice.status, ResumeStatus::Ok);
        assert_eq!(slice.events.len(), 1);
        assert_eq!(slice.events[0].name, "b");
        let _ = fs::remove_dir_all(&dir);
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "workmate-event-log-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir
    }

    #[test]
    fn client_cursor_ahead_of_host_is_gap() {
        let log = EventLog::new(8);
        log.record(WebSocketMessage::new("a", json!({})));
        let slice = log.events_after(EventCursor::new(1, 5));
        assert_eq!(slice.status, ResumeStatus::Gap);
        assert!(slice.events.is_empty());
        assert_eq!(slice.current, EventCursor::new(1, 1));
        let caught_up = log.events_after(EventCursor::new(1, 1));
        assert_eq!(caught_up.status, ResumeStatus::Ok);
        assert!(caught_up.events.is_empty());
    }

    #[test]
    fn emptied_journal_starts_new_epoch_instead_of_reusing_sequences() {
        let dir = tmp_dir("emptied");
        let log = EventLog::open_durable(&dir, 8).unwrap();
        for name in ["a", "b", "c"] {
            log.record(WebSocketMessage::new(name, json!({})));
        }
        let seen = log.current_cursor();
        drop(log);
        fs::write(dir.join("events.ndjson"), b"").unwrap();

        let log2 = EventLog::open_durable(&dir, 8).unwrap();
        assert_eq!(log2.epoch(), seen.epoch + 1);
        let slice = log2.events_after(seen);
        assert_eq!(slice.status, ResumeStatus::Gap);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn idle_restart_keeps_working() {
        let dir = tmp_dir("idle");
        drop(EventLog::open_durable(&dir, 8).unwrap());
        let log = EventLog::open_durable(&dir, 8).unwrap();
        let first = log.record(WebSocketMessage::new("a", json!({})));
        assert_eq!(first.cursor.unwrap().sequence, 1);
        let slice = log.events_after(EventCursor::new(log.epoch(), 0));
        assert_eq!(slice.status, ResumeStatus::Ok);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_event_is_memory_only_and_restart_reports_gap() {
        let dir = tmp_dir("oversized");
        let limits = JournalLimits {
            max_bytes: 64 * 1024,
            max_event_bytes: 256,
        };
        let log = EventLog::open_durable_with_limits(&dir, 8, limits).unwrap();
        log.record(WebSocketMessage::new("small", json!({})));
        log.record(WebSocketMessage::new("big", json!({ "blob": "x".repeat(1024) })));
        let seen_small = EventCursor::new(log.epoch(), 1);
        // Live process still replays the big event from memory.
        assert_eq!(log.events_after(seen_small).status, ResumeStatus::Ok);
        let before = log.current_cursor();
        drop(log);

        let log2 = EventLog::open_durable_with_limits(&dir, 8, limits).unwrap();
        // Sequence continues past the omitted event; nothing is reused.
        assert_eq!(log2.current_cursor(), before);
        let slice = log2.events_after(seen_small);
        assert_eq!(slice.status, ResumeStatus::Gap);
        assert!(slice.events.is_empty());
        assert_eq!(log2.events_after(before).status, ResumeStatus::Ok);
        let next = log2.record(WebSocketMessage::new("next", json!({})));
        assert_eq!(next.cursor.unwrap().sequence, before.sequence + 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_middle_line_reports_gap_after_restart() {
        let dir = tmp_dir("corrupt");
        let log = EventLog::open_durable(&dir, 8).unwrap();
        for name in ["a", "b", "c"] {
            log.record(WebSocketMessage::new(name, json!({})));
        }
        let epoch = log.epoch();
        drop(log);
        let path = dir.join("events.ndjson");
        let raw = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        fs::write(&path, format!("{}\n{{not json\n{}\n", lines[0], lines[2])).unwrap();

        let log2 = EventLog::open_durable(&dir, 8).unwrap();
        assert_eq!(log2.epoch(), epoch);
        assert_eq!(log2.current_cursor(), EventCursor::new(epoch, 3));
        let slice = log2.events_after(EventCursor::new(epoch, 1));
        assert_eq!(slice.status, ResumeStatus::Gap);
        assert_eq!(slice.events.len(), 1);
        assert_eq!(log2.events_after(EventCursor::new(epoch, 2)).status, ResumeStatus::Ok);
        let _ = fs::remove_dir_all(&dir);
    }
}
