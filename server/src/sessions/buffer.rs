//! Ring buffer with `tokio::sync::Notify` for efficient subscriber wakeup.
//!
//! [`OutputBuffer`] stores sequenced output entries from a shell session. When
//! the buffer is full, the oldest entries are evicted. Subscribers (and
//! long-poll waiters) are woken via a shared [`Notify`].

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, Notify};

use super::journal::JournalEntry;

/// Which output stream produced the data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
    /// Synthetic messages from the session runtime (e.g. "Process exited with code 0").
    System,
}

impl OutputStream {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::System => "system",
        }
    }
}

/// A single sequenced output entry.
#[derive(Debug, Clone)]
pub struct OutputEntry {
    /// Monotonically increasing sequence number (unique within a session).
    pub seq: u64,
    /// Which stream produced this entry.
    pub stream: OutputStream,
    /// The output data (lossy UTF-8).
    pub data: String,
    /// Unix timestamp in milliseconds when the entry was created.
    pub timestamp_ms: u64,
}

/// Ring buffer of [`OutputEntry`] items with subscriber notification.
pub struct OutputBuffer {
    entries: VecDeque<OutputEntry>,
    next_seq: u64,
    max_entries: usize,
    notify: Arc<Notify>,
    /// Optional channel to the journal writer task.
    journal_tx: Option<mpsc::Sender<JournalEntry>>,
}

impl OutputBuffer {
    /// Create a new buffer that holds at most `max_entries` items.
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(max_entries.min(256)),
            next_seq: 1,
            max_entries,
            notify: Arc::new(Notify::new()),
            journal_tx: None,
        }
    }

    /// Attach a journal writer channel. Entries pushed after this call will
    /// also be sent to the journal.
    pub fn set_journal(&mut self, tx: mpsc::Sender<JournalEntry>) {
        self.journal_tx = Some(tx);
    }

    /// Push a new entry, evicting the oldest if full, and notify all waiters.
    /// Also sends the entry to the journal if one is attached.
    pub fn push(&mut self, stream: OutputStream, data: String) {
        let seq = self.next_seq;
        self.next_seq += 1;

        #[allow(clippy::cast_possible_truncation)]
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);

        if self.entries.len() >= self.max_entries {
            self.entries.pop_front();
        }

        let entry = OutputEntry {
            seq,
            stream,
            data,
            timestamp_ms,
        };

        // Send to journal (non-blocking, best-effort — must not block under Mutex)
        if let Some(ref tx) = self.journal_tx {
            let _ = tx.try_send(JournalEntry::from_output_entry(&entry));
        }

        self.entries.push_back(entry);
        self.notify.notify_waiters();
    }

    /// Read all entries with `seq > since`.
    ///
    /// Returns `(entries, dropped_count)` where `dropped_count > 0` if entries
    /// between `since` and the oldest available entry were evicted.
    pub fn read_since(&self, since: u64) -> (Vec<OutputEntry>, u64) {
        let oldest_available = self.entries.front().map_or(self.next_seq, |e| e.seq);
        let dropped = if oldest_available > since.saturating_add(1) {
            oldest_available - since - 1
        } else {
            0
        };

        let entries: Vec<OutputEntry> = self
            .entries
            .iter()
            .filter(|e| e.seq > since)
            .cloned()
            .collect();

        (entries, dropped)
    }

    /// Quick check: are there entries with `seq > since`?
    pub fn has_entries_since(&self, since: u64) -> bool {
        self.entries.back().is_some_and(|e| e.seq > since)
    }

    /// Get a clone of the `Arc<Notify>` for external waiting.
    pub fn notifier(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }

    /// Current next sequence number (i.e. number of entries ever pushed).
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }
}
