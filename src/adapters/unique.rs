//! Shared unique-temp sequence generator.
//!
//! Several modules previously each kept their own `AtomicU64` for
//! unique temporary names (`PART_SEQUENCE`, `SNAPSHOT_SEQUENCE`,
//! `TEMP_SEQUENCE` (×2), `FAILURE_SEQUENCE`, `REQUEST_SEQUENCE`).
//! All of them were independent monotonic counters whose only job was
//! to make a `<pid>-<seq>` name unique inside one process. A single
//! shared counter is sufficient and removes the duplication: every
//! caller still gets a unique value, and the exact numbers differ
//! from before (harmless — the names are never compared across runs).

use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Next unique sequence number in this process.
#[inline]
pub(crate) fn next() -> u64 {
    SEQ.fetch_add(1, Ordering::Relaxed)
}
