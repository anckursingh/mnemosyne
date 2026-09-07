//! SE2-M8 — read-path instrumentation (QA spec M0): cumulative atomics on
//! the Db and its SegmentReaders. Counters move only with real operations;
//! timings run only when stats are attached (readers opened directly carry
//! None — zero overhead in the format/golden suites).
//!
//! Counter scope, by owner: the Db-level counters (`lookups`,
//! `memtable_*`, `segments_*`) count only `Db::get` — the W1/W2 diagnosis
//! they exist for. The SegmentReader-level counters (`block_*`, `index_*`,
//! `bytes_read`, `entries_decoded`) count every block load that reader
//! serves — scans and compaction pulls included. That is the honest split:
//! a compaction's I/O is not a point read.

use std::sync::atomic::{AtomicU64, Ordering};

/// A snapshot of the cumulative read-path counters (the QA doc's
/// `ReadPathMetrics`; `value_decode_ns` is folded into `block_decode_ns` —
/// values decode with their entries, a separate counter would be fiction).
/// `segments_range_skipped` fires before the bloom probe (SE2-M9).
/// SE2-M21 adds the attribution closure: `lock_wait_ns` (state-guard wait),
/// `bloom_probe_ns` (the bloom pre-check — untimed it would be ~a quarter
/// of a warm cache hit, so the accounting could not close), `get_wall_ns`
/// (the whole get — the denominator the residual is bounded against).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReadPathStats {
    pub lookups: u64,
    pub memtable_lookup_ns: u64,
    pub memtable_hits: u64,
    pub segments_considered: u64,
    pub segments_range_skipped: u64,
    pub segments_bloom_skipped: u64,
    pub segments_index_searched: u64,
    pub index_lookup_ns: u64,
    pub block_cache_lookup_ns: u64,
    pub block_cache_hits: u64,
    pub block_cache_misses: u64,
    pub block_io_ns: u64,
    pub block_decode_ns: u64,
    pub blocks_read: u64,
    pub bytes_read: u64,
    pub entries_decoded: u64,
    pub lock_wait_ns: u64,
    pub bloom_probe_ns: u64,
    pub get_wall_ns: u64,
}

/// The live counters — one per field, relaxed atomics (~ns overhead).
#[derive(Debug, Default)]
pub(crate) struct Stats {
    pub(crate) lookups: AtomicU64,
    pub(crate) memtable_lookup_ns: AtomicU64,
    pub(crate) memtable_hits: AtomicU64,
    pub(crate) segments_considered: AtomicU64,
    pub(crate) segments_range_skipped: AtomicU64,
    pub(crate) segments_bloom_skipped: AtomicU64,
    pub(crate) segments_index_searched: AtomicU64,
    pub(crate) index_lookup_ns: AtomicU64,
    pub(crate) block_cache_lookup_ns: AtomicU64,
    pub(crate) block_cache_hits: AtomicU64,
    pub(crate) block_cache_misses: AtomicU64,
    pub(crate) block_io_ns: AtomicU64,
    pub(crate) block_decode_ns: AtomicU64,
    pub(crate) blocks_read: AtomicU64,
    pub(crate) bytes_read: AtomicU64,
    pub(crate) entries_decoded: AtomicU64,
    pub(crate) lock_wait_ns: AtomicU64,
    pub(crate) bloom_probe_ns: AtomicU64,
    pub(crate) get_wall_ns: AtomicU64,
}

impl Stats {
    pub(crate) fn snapshot(&self) -> ReadPathStats {
        ReadPathStats {
            lookups: self.lookups.load(Ordering::Relaxed),
            memtable_lookup_ns: self.memtable_lookup_ns.load(Ordering::Relaxed),
            memtable_hits: self.memtable_hits.load(Ordering::Relaxed),
            segments_considered: self.segments_considered.load(Ordering::Relaxed),
            segments_range_skipped: self.segments_range_skipped.load(Ordering::Relaxed),
            segments_bloom_skipped: self.segments_bloom_skipped.load(Ordering::Relaxed),
            segments_index_searched: self.segments_index_searched.load(Ordering::Relaxed),
            index_lookup_ns: self.index_lookup_ns.load(Ordering::Relaxed),
            block_cache_lookup_ns: self.block_cache_lookup_ns.load(Ordering::Relaxed),
            block_cache_hits: self.block_cache_hits.load(Ordering::Relaxed),
            block_cache_misses: self.block_cache_misses.load(Ordering::Relaxed),
            block_io_ns: self.block_io_ns.load(Ordering::Relaxed),
            block_decode_ns: self.block_decode_ns.load(Ordering::Relaxed),
            blocks_read: self.blocks_read.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            entries_decoded: self.entries_decoded.load(Ordering::Relaxed),
            lock_wait_ns: self.lock_wait_ns.load(Ordering::Relaxed),
            bloom_probe_ns: self.bloom_probe_ns.load(Ordering::Relaxed),
            get_wall_ns: self.get_wall_ns.load(Ordering::Relaxed),
        }
    }
}
