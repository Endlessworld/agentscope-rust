//! Read-side counters: what one tick's reads cost.
//!
//! The write side already answers "how many transactions and bytes does a turn cost"
//! ([`crate::store::file::write_stats`]); these are its mirror image, so that "how much of the
//! conversation does a tick read" is a number and not an argument. They are relaxed atomic
//! adds and nothing else — no disk, no lock, no allocation — and unlike a `cfg(test)` copy they
//! are visible to the integration tests that measure a whole tick, which is the only place the
//! question is interesting.

use std::sync::atomic::{AtomicU64, Ordering};

static JOURNAL_READS: AtomicU64 = AtomicU64::new(0);
static JOURNAL_ENTRIES: AtomicU64 = AtomicU64::new(0);
static KEYS_RETURNED: AtomicU64 = AtomicU64::new(0);
static CONTROL_READS: AtomicU64 = AtomicU64::new(0);
static SCOPE_PARSES: AtomicU64 = AtomicU64::new(0);
static SCOPE_PICKS: AtomicU64 = AtomicU64::new(0);

/// What this process read since the last reset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadStats {
    /// Journal pages served (one transaction each).
    pub journal_reads: u64,
    /// Journal entries handed to the caller, across those pages.
    pub journal_entries: u64,
    /// Keys a store answered for a `keys()` call.
    pub keys_returned: u64,
    /// Control polls answered while an invocation held the lease.
    pub control_reads: u64,
    /// Scope documents parsed whole (a file-store transaction).
    pub scope_parses: u64,
    /// Scope documents scanned for only some of their parts (a file-store control read).
    pub scope_picks: u64,
}

/// The counters above, as of now.
pub fn read_stats() -> ReadStats {
    ReadStats {
        journal_reads: JOURNAL_READS.load(Ordering::Relaxed),
        journal_entries: JOURNAL_ENTRIES.load(Ordering::Relaxed),
        keys_returned: KEYS_RETURNED.load(Ordering::Relaxed),
        control_reads: CONTROL_READS.load(Ordering::Relaxed),
        scope_parses: SCOPE_PARSES.load(Ordering::Relaxed),
        scope_picks: SCOPE_PICKS.load(Ordering::Relaxed),
    }
}

/// Zeroes the counters; a measurement takes one before and one after the work it costs.
pub fn reset_read_stats() {
    JOURNAL_READS.store(0, Ordering::Relaxed);
    JOURNAL_ENTRIES.store(0, Ordering::Relaxed);
    KEYS_RETURNED.store(0, Ordering::Relaxed);
    CONTROL_READS.store(0, Ordering::Relaxed);
    SCOPE_PARSES.store(0, Ordering::Relaxed);
    SCOPE_PICKS.store(0, Ordering::Relaxed);
}

pub(crate) fn count_journal_read(entries: usize) {
    JOURNAL_READS.fetch_add(1, Ordering::Relaxed);
    JOURNAL_ENTRIES.fetch_add(entries as u64, Ordering::Relaxed);
}

pub(crate) fn count_keys(keys: usize) {
    KEYS_RETURNED.fetch_add(keys as u64, Ordering::Relaxed);
}

pub(crate) fn count_control_read() {
    CONTROL_READS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn count_scope_parse() {
    SCOPE_PARSES.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn count_scope_pick() {
    SCOPE_PICKS.fetch_add(1, Ordering::Relaxed);
}
