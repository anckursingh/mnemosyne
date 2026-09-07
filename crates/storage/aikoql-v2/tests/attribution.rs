//! SE2-M21 — point-read cost attribution (TDD read-path recovery plan,
//! M21-01..05). The milestone's unit layer: the attribution accounting
//! bound and the get paths the adoption-scale probe measures — including
//! the merged-segment path (M21-05: the compaction output reader must
//! carry the shared cache and stats or its reads go uncounted).
//!
//! The counters this suite pins (SE2-M21 additions to the SE2-M8 truth
//! layer): `lock_wait_ns` (state-guard wait inside `Db::get`),
//! `bloom_probe_ns` (the bloom pre-check — untimed it would be ~a quarter
//! of a warm cache hit, so the accounting could not close), and
//! `get_wall_ns` (the whole get — the denominator the residual is bounded
//! against). The residual is the untimed remainder: entry/exit fetches,
//! the segment-walk loop, the cache insert on a miss, and the phase-
//! boundary timestamps themselves — the fixed per-get cost the unit bound
//! tolerates explicitly (`INSTRUMENTATION_NS_PER_GET`). Counter pins are
//! mechanism asserts; absolute timings are report cells (the M8 rule).
//!
//! The adoption-scale legs (W1/W2 kernel vs engine, memtable/cache
//! hit/cache miss) live in `kse_m7_v2_workloads.rs` behind
//! `SE2M21_ATTRIB=1`.

mod common;

use aikoql_storage_v2::db::{Config, Db};
use aikoql_storage_v2::stats::ReadPathStats;
use common::tmp;

/// The adoption row shape: ~1.4 KB values (M11: ~11 rows per 16 KiB block).
const ROW_BYTES: usize = 1400;

/// The fixed untimed cost of one get: the phase-boundary timestamps
/// themselves (~7–9 `Instant::now` reads per segment get — one per phase
/// boundary) plus the walk-loop glue (counter fetches, range-skip
/// comparisons). Measured ~350–420 ns/get on this machine; the unit bound
/// tolerates it explicitly so a µs-scale get isn't judged by a sub-µs
/// allowance. The adoption-scale probe keeps the PURE 10% bound — there a
/// get is ~35 µs and the same overhead is <2%.
const INSTRUMENTATION_NS_PER_GET: u64 = 500;

/// The timed phases of one get — the parts the accounting sums against
/// `get_wall_ns`. Everything else is the residual.
fn phases(s: ReadPathStats) -> u64 {
    s.lock_wait_ns
        + s.memtable_lookup_ns
        + s.bloom_probe_ns
        + s.index_lookup_ns
        + s.block_cache_lookup_ns
        + s.block_io_ns
        + s.block_decode_ns
}

fn key(i: usize) -> Vec<u8> {
    format!("k/{i:03}").into_bytes()
}

/// Dominance-ratio asserts are timing cells (the M8 rule in this file's
/// header: counter pins are mechanism asserts, absolute timings are report
/// cells) — on a shared CI runner a scheduling pause lands inside one timed
/// phase and the ratio flaps (run 34101854185: decode+cache 644/1707 ns vs
/// the 40% floor on a leg that is ~90% hot on a dev box). They run when
/// SE2M21_ATTRIB=1 — the same strict opt-in knob as the adoption-scale legs
/// in kse_m7_v2_workloads.rs — so milestone runs still get the full cells.
fn timing_cells() -> bool {
    std::env::var_os("SE2M21_ATTRIB").is_some()
}

fn row(b: u8) -> Vec<u8> {
    vec![b; ROW_BYTES]
}

/// M21-01 — the accounting closes: over a workload mixing all three paths
/// (memtable hits, cache hits, cache misses) the untimed residual beyond
/// the documented fixed per-get instrumentation cost is at most 10% of the
/// measured whole. Aggregate, not per-op — a sub-µs memtable hit carries
/// ~100 ns of entry/exit overhead the bound must not hinge on.
#[test]
fn m21_01_attribution_accounting_closes_within_10pct() {
    let path = tmp("attrib-account");
    let mut cfg = Config::new(path.clone());
    cfg.memtable_bytes = 16 * 1024; // force flushes — the segment paths exist
    cfg.l0_compact_trigger = 0; // no merge: the walk visits every segment
    cfg.block_target = 2048; // one row per block — the miss pins are exact
    let db = Db::open(cfg).unwrap();

    // 36 segment keys (flushed out explicitly): 30 warmed `k/` keys, then
    // 6 unwarmed keys under a far prefix (`z/` — their block is never
    // touched by the warm pass, so every get of them is a cache miss),
    // then 10 fresh keys that stay in the active memtable (14 KB < 16 KiB).
    for i in 0..30 {
        db.put(&key(i), &row(b's')).unwrap();
    }
    for i in 30..36 {
        db.put(&format!("z/{i:02}").into_bytes(), &row(b'z'))
            .unwrap();
    }
    db.flush().unwrap();
    for i in 0..30 {
        assert!(db.get(&key(i)).unwrap().is_some());
    }
    for i in 0..10 {
        db.put(&format!("m/{i:02}").into_bytes(), &row(b'm'))
            .unwrap();
    }

    let before = db.read_path_stats();
    for i in 0..30 {
        assert!(db.get(&key(i)).unwrap().is_some()); // cache hits
    }
    for i in 0..10 {
        assert!(db.get(&format!("m/{i:02}").into_bytes()).unwrap().is_some()); // memtable hits
    }
    for i in 30..36 {
        assert!(db.get(&format!("z/{i:02}").into_bytes()).unwrap().is_some()); // unwarmed blocks → misses
    }
    let after = db.read_path_stats();
    let d = common::stats_delta(after, before);

    assert_eq!(d.lookups, 46);
    let total = d.get_wall_ns;
    assert!(total > 0, "the whole-get timer must run");
    let residual = total.saturating_sub(phases(d));
    assert!(
        residual.saturating_sub(INSTRUMENTATION_NS_PER_GET * d.lookups) * 10 <= total,
        "attribution residual {residual} ns beyond the fixed per-get cost exceeds 10% of {total} ns — phases: lock {} memtable {} bloom {} index {} cache {} io {} decode {}",
        d.lock_wait_ns,
        d.memtable_lookup_ns,
        d.bloom_probe_ns,
        d.index_lookup_ns,
        d.block_cache_lookup_ns,
        d.block_io_ns,
        d.block_decode_ns,
    );
    // the mixed workload really exercised all three paths
    assert!(d.memtable_hits >= 10);
    assert!(d.block_cache_hits >= 30);
    assert!(d.block_cache_misses >= 6);
    assert!(d.blocks_read >= 1);
    drop(db);
}

/// M21-02 — the memtable-hit leg: a hit never starts the segment walk.
/// The memtable timer covers the probe and the value clone, so the
/// memtable phase dominates its own leg.
#[test]
fn m21_02_attribution_memtable_hit_leg() {
    let path = tmp("attrib-memtable");
    let mut cfg = Config::new(path.clone());
    cfg.memtable_bytes = usize::MAX; // nothing flushes — every get a hit
    let db = Db::open(cfg).unwrap();
    for i in 0..50 {
        db.put(&key(i), &row(b'm')).unwrap();
    }
    let before = db.read_path_stats();
    for _ in 0..200 {
        assert_eq!(db.get(&key(7)).unwrap(), Some(row(b'm')));
    }
    let after = db.read_path_stats();
    let d = common::stats_delta(after, before);

    assert_eq!(d.lookups, 200);
    assert_eq!(d.memtable_hits, 200);
    // zero segment work — the walk never starts
    assert_eq!(d.segments_considered, 0);
    assert_eq!(d.bloom_probe_ns, 0);
    assert_eq!(d.index_lookup_ns, 0);
    assert_eq!(d.block_cache_lookup_ns, 0);
    assert_eq!(d.block_io_ns, 0);
    assert_eq!(d.block_decode_ns, 0);
    assert_eq!(d.blocks_read, 0);
    assert_eq!(d.bytes_read, 0);
    assert_eq!(d.entries_decoded, 0);
    // the memtable probe + clone dominate the leg's timed work
    let parts = phases(d);
    assert!(parts > 0, "the leg must do timed work");
    assert!(
        d.memtable_lookup_ns * 2 >= parts,
        "memtable phase {} ns is not dominant in a memtable-only leg (parts {parts} ns)",
        d.memtable_lookup_ns
    );
    drop(db);
}

/// M21-03 — the cache-hit leg: a flushed, warmed block serves the get
/// with no physical I/O; decode + cache lookup dominate the timed work.
#[test]
fn m21_03_attribution_cache_hit_leg() {
    let path = tmp("attrib-cachehit");
    let mut cfg = Config::new(path.clone());
    cfg.memtable_bytes = usize::MAX;
    let db = Db::open(cfg).unwrap();
    for i in 0..50 {
        db.put(&key(i), &row(b'c')).unwrap();
    }
    db.flush().unwrap(); // one segment; the 8 MiB cache holds all its blocks
    for i in 0..50 {
        assert!(db.get(&key(i)).unwrap().is_some()); // warm pass (uncounted)
    }
    let before = db.read_path_stats();
    for i in 0..200 {
        assert_eq!(db.get(&key(i % 50)).unwrap(), Some(row(b'c')));
    }
    let after = db.read_path_stats();
    let d = common::stats_delta(after, before);

    assert_eq!(d.lookups, 200);
    assert_eq!(d.memtable_hits, 0, "everything flushed — no memtable hit");
    assert!(d.block_cache_hits >= 200);
    assert_eq!(d.blocks_read, 0, "a cached get performs no physical read");
    assert_eq!(d.bytes_read, 0);
    assert_eq!(d.block_io_ns, 0);
    assert!(d.entries_decoded >= 200);
    let parts = phases(d);
    assert!(parts > 0, "the leg must do timed work");
    if timing_cells() {
        let hot = d.block_decode_ns + d.block_cache_lookup_ns;
        assert!(
            hot * 5 >= parts * 2,
            "decode+cache {} ns is not the dominant phase of a cache hit (parts {parts} ns)",
            hot
        );
    }
    drop(db);
}

/// M21-04 — the cache-miss leg: a 4 KiB cache is consulted (misses count)
/// yet holds nothing (every 2 KiB block exceeds... no — the blocks FIT the
/// cap here; each key is read exactly once, so every get is a first touch
/// and a miss). Small blocks put each get in its own block, so the miss
/// leg's I/O is per-get and dominates.
#[test]
fn m21_04_attribution_cache_miss_leg() {
    let path = tmp("attrib-cachemiss");
    let mut cfg = Config::new(path.clone());
    cfg.memtable_bytes = usize::MAX;
    cfg.cache_bytes = 4096; // attached: every get consults the cache and misses
    cfg.block_target = 2048; // one row per block → every get reads its own block
    let db = Db::open(cfg).unwrap();
    for i in 0..100 {
        db.put(&key(i), &row(b'x')).unwrap();
    }
    db.flush().unwrap();
    let before = db.read_path_stats();
    for i in 0..100 {
        assert_eq!(db.get(&key(i)).unwrap(), Some(row(b'x')));
    }
    let after = db.read_path_stats();
    let d = common::stats_delta(after, before);

    assert_eq!(d.lookups, 100);
    assert_eq!(d.block_cache_misses, 100, "every get consults and misses");
    assert_eq!(d.block_cache_hits, 0);
    assert_eq!(d.blocks_read, 100, "one row per block — one read per get");
    assert!(d.block_io_ns > 0);
    let parts = phases(d);
    assert!(parts > 0, "the leg must do timed work");
    if timing_cells() {
        assert!(
            d.block_io_ns * 2 >= parts,
            "block I/O {} ns is not dominant in a miss leg (parts {parts} ns)",
            d.block_io_ns
        );
    }
    drop(db);
}

/// M21-05 — merged segments serve reads like any segment: the compaction
/// output reader carries the shared cache and stats (the SE2-M21 fix —
/// the merge used to reopen its output reader bare, so every read of an
/// L1 segment was an uncounted, uncached raw read). A warmed get on the
/// merged segment is therefore a counted cache hit that attributes.
#[test]
fn m21_05_merged_segment_reads_are_cached_and_counted() {
    let path = tmp("attrib-merged");
    let mut cfg = Config::new(path.clone());
    cfg.memtable_bytes = usize::MAX;
    let db = Db::open(cfg).unwrap();
    for i in 0..50 {
        db.put(&key(i), &row(b'g')).unwrap();
    }
    db.flush().unwrap();
    db.compact().unwrap();
    for i in 0..50 {
        assert!(db.get(&key(i)).unwrap().is_some()); // warm pass (uncounted)
    }
    let before = db.read_path_stats();
    for i in 0..200 {
        assert_eq!(db.get(&key(i % 50)).unwrap(), Some(row(b'g')));
    }
    let after = db.read_path_stats();
    let d = common::stats_delta(after, before);

    assert_eq!(d.lookups, 200);
    assert!(
        d.block_cache_hits >= 200,
        "merged-segment gets must be cached"
    );
    assert_eq!(
        d.blocks_read, 0,
        "a cached merged get performs no physical read"
    );
    let parts = phases(d);
    let residual = d.get_wall_ns.saturating_sub(parts);
    assert!(
        residual.saturating_sub(INSTRUMENTATION_NS_PER_GET * d.lookups) * 10 <= d.get_wall_ns,
        "merged-segment gets must attribute: residual {residual} ns beyond the fixed per-get cost of {} ns — phases: lock {} memtable {} bloom {} index {} cache {} io {} decode {}",
        d.get_wall_ns,
        d.lock_wait_ns,
        d.memtable_lookup_ns,
        d.bloom_probe_ns,
        d.index_lookup_ns,
        d.block_cache_lookup_ns,
        d.block_io_ns,
        d.block_decode_ns,
    );
    drop(db);
}
