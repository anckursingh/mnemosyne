//! SE2-M8 — read-path instrumentation (QA spec M0, TC-PERF-0001..0003),
//! extended SE2-M9 with the key-range skip (QA M1 candidate selection).
//!
//! The counters must move only with real operations — a metric that never
//! moves fails the pin (the QA doc's "not synthetic" rule). The bloom-skip
//! scenario is deterministic by construction: the nine filler segments hold
//! TWO keys each (m = 20 bits, m = 10·n per the M1 spec), all eight of
//! their probe positions below bit 10, and the target probes at least one
//! bit ≥ 10 — so the fillers' blooms provably reject the target. The filler
//! pairs bracket the target lexically ("a…" < "m…" < "z…"), so the M9
//! range skip cannot fire first — the exact 9-skipped / 1-searched split is
//! not a probability and not a range-skip artifact.

mod common;

use aikoql_kernel::knowledge::kom::sha256;
use aikoql_storage_v2::db::{Config, Db};
use aikoql_storage_v2::stats::ReadPathStats;
use common::dir;

/// The M1 bloom spec (m = 10·n, 4 probes, double hashing over sha256),
/// computed independently — the test's expectation must not be the
/// engine's code.
fn probes(key: &[u8], m: u32) -> [u32; 4] {
    let d = sha256(key);
    let h1 = u64::from_le_bytes(d[..8].try_into().expect("sha256 len"));
    let h2 = u64::from_le_bytes(d[8..16].try_into().expect("sha256 len"));
    let mut p = [0u32; 4];
    for (i, slot) in p.iter_mut().enumerate() {
        *slot = ((h1 as u128 + i as u128 * h2 as u128) % m as u128) as u32;
    }
    p
}

/// A key of prefix `p` whose bloom has all four probes in the low half of
/// `m` — any target probing the high half is provably rejected. The search
/// is bounded and deterministic (sha256 is).
fn low_half_key(prefix: &str, seed: u32, m: u32) -> String {
    for i in 0..10_000u32 {
        let k = format!("{prefix}{seed}-{i}");
        if probes(k.as_bytes(), m).iter().all(|p| *p < m / 2) {
            return k;
        }
    }
    panic!("no low-half filler found for {prefix}{seed}");
}

/// A key of prefix `p` that probes the high half of `m` at least once.
fn high_half_key(prefix: &str, m: u32) -> String {
    for i in 0..10_000u32 {
        let k = format!("{prefix}{i}");
        if probes(k.as_bytes(), m).iter().any(|p| *p >= m / 2) {
            return k;
        }
    }
    panic!("no high-half key found for {prefix}");
}

/// Nine two-key segments whose blooms reject `target` by construction and
/// whose key ranges bracket it (so only the bloom can skip them), plus the
/// segment that holds it. Flush is explicit (memtable threshold off).
fn ten_segments(tag: &str) -> (Db, String) {
    let target = high_half_key("m", 20);
    let mut cfg = Config::new(dir(tag));
    cfg.memtable_bytes = usize::MAX;
    // SE2-M10: these scenarios pin the exact 10-segment layout and the
    // counters-move-only-with-real-operations invariant — the L0 trigger's
    // compaction would clobber both mid-setup (compaction I/O is counted).
    cfg.l0_compact_trigger = 0;
    cfg.block_target = 256;
    let db = Db::open(cfg).unwrap();
    // Target FIRST so it lives in the oldest segment: get walks newest-first,
    // so the nine filler segments are bloom-rejected before the hit.
    db.put(target.as_bytes(), &[b'v'; 200][..]).unwrap();
    db.flush().unwrap();
    for i in 0..9 {
        let low = low_half_key("a", i, 20);
        let high = low_half_key("z", i, 20);
        db.put(low.as_bytes(), &[b'v'; 200][..]).unwrap();
        db.put(high.as_bytes(), &[b'v'; 200][..]).unwrap();
        db.flush().unwrap();
    }
    (db, target)
}

/// Nine newer single-key segments whose ranges provably exclude the target
/// ("f{i}-x" vs "m…") — only the range skip can reject them.
fn ten_disjoint_segments(tag: &str) -> (Db, String) {
    let target = "m000-target".to_string();
    let mut cfg = Config::new(dir(tag));
    cfg.memtable_bytes = usize::MAX;
    // SE2-M10: pins the exact 10-segment layout (see ten_segments).
    cfg.l0_compact_trigger = 0;
    cfg.block_target = 256;
    let db = Db::open(cfg).unwrap();
    db.put(target.as_bytes(), &[b'v'; 200][..]).unwrap();
    db.flush().unwrap();
    for i in 0..9 {
        let filler = format!("f{i}-x");
        db.put(filler.as_bytes(), &[b'v'; 200][..]).unwrap();
        db.flush().unwrap();
    }
    (db, target)
}

#[test]
fn instrumented_point_lookup_populates_metrics() {
    let (db, target) = ten_segments("perf-0001");
    let zero = db.read_path_stats();
    assert_eq!(
        zero,
        ReadPathStats::default(),
        "fresh stats must be all-zero — counters move only with real operations"
    );
    assert_eq!(
        db.get(target.as_bytes()).unwrap().as_deref(),
        Some(&[b'v'; 200][..])
    );
    let s = db.read_path_stats();
    assert_eq!(s.lookups, 1);
    assert_eq!(
        s.segments_considered, 10,
        "every segment is iterated (candidate selection is a later milestone)"
    );
    assert_eq!(s.blocks_read, 1, "exactly one physical block read");
    assert!(s.bytes_read > 0, "bytes_read must count the real read");
    assert!(s.entries_decoded >= 1, "the winning entry was decoded");
    assert!(s.block_io_ns > 0, "block_io_ns must measure a real read");
    assert!(
        s.memtable_lookup_ns > 0,
        "memtable_lookup_ns must measure the real memtable probe"
    );
}

#[test]
fn bloom_skip_evidence() {
    let (db, target) = ten_segments("perf-0002");
    db.get(target.as_bytes()).unwrap();
    let s = db.read_path_stats();
    assert_eq!(
        s.segments_range_skipped, 0,
        "the filler ranges bracket the target — the range skip cannot fire"
    );
    assert_eq!(
        s.segments_bloom_skipped, 9,
        "the nine non-matching segments are bloom-rejected by construction"
    );
    assert_eq!(
        s.segments_index_searched, 1,
        "only the containing segment searches its block index"
    );
}

#[test]
fn key_range_skip_evidence() {
    // SE2-M9 — a segment whose [key_min, key_max] excludes the target is
    // rejected before the bloom is ever probed; considered stays "iterated".
    let (db, target) = ten_disjoint_segments("perf-0003");
    assert_eq!(
        db.get(target.as_bytes()).unwrap().as_deref(),
        Some(&[b'v'; 200][..])
    );
    let s = db.read_path_stats();
    assert_eq!(s.segments_considered, 10);
    assert_eq!(
        s.segments_range_skipped, 9,
        "out-of-range segments skip before the bloom probe"
    );
    assert_eq!(
        s.segments_bloom_skipped, 0,
        "the range skip fires before the bloom is consulted"
    );
    assert_eq!(s.segments_index_searched, 1);
}

#[test]
fn cache_hit_skips_physical_io() {
    let (db, target) = ten_segments("perf-0004");
    db.get(target.as_bytes()).unwrap();
    let first = db.read_path_stats();
    assert_eq!(first.blocks_read, 1);
    assert!(
        first.block_cache_misses >= 1,
        "the cold read missed the cache"
    );
    db.get(target.as_bytes()).unwrap();
    let second = db.read_path_stats();
    assert!(second.block_cache_hits >= 1, "the warm read hit the cache");
    assert_eq!(
        second.blocks_read, first.blocks_read,
        "a cached hit performs no second physical block read"
    );
    assert_eq!(
        second.bytes_read, first.bytes_read,
        "a cached hit reads no further bytes"
    );
}
