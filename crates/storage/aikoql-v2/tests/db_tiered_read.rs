//! SE2-M17 — read-path certification at tier depth: the size tier (M16)
//! lets L0 pile to ~10-17 segments before the merge fires, so a get walks
//! up to 18 segments vs 2 under count-only. The per-segment walk is
//! range-skip -> bloom -> index (M9/M7/M1), newest first; the pins here
//! prove the fan-out is absorbed mechanically and the answers stay
//! byte-exact against an independent oracle and a count-only twin. The
//! latency cells are the scale probe's job (SE2M17_READS loader phase) —
//! perf numbers are report cells, these are the honesty asserts.

mod common;

use aikoql_storage_v2::db::{manifest_path, Config, Db};
use aikoql_storage_v2::format::{Current, Manifest};
use common::dir;
use std::collections::BTreeMap;

/// One flush per round: 8 puts x ~64 B (memtable accounting) crosses a
/// 512-byte memtable from empty — the M16 fixture geometry.
fn round_put(db: &Db, r: usize) {
    for i in 0..8 {
        let k = format!("k{r:03}{i:02}").into_bytes();
        let v = format!("v{r:03}{i:02}{}", "y".repeat(34)).into_bytes();
        db.put(&k, &v).unwrap();
    }
}

/// Overlapping-range rounds: keys k00r..k07r, so every segment's
/// [key_min, key_max] spans the whole k00...-k07... band — a target inside
/// the band is in-range of EVERY segment (the adversarial fan-out shape,
/// where the range skip cannot prune).
fn band_put(db: &Db, r: usize) {
    for i in 0..8 {
        let k = format!("k{i:02}{r:03}").into_bytes();
        let v = format!("v{i:02}{r:03}{}", "y".repeat(34)).into_bytes();
        db.put(&k, &v).unwrap();
    }
}

fn l0_count(d: &std::path::Path) -> usize {
    let current = Current::read(&d.join("CURRENT")).unwrap();
    let manifest = Manifest::read(&manifest_path(d, current.manifest_generation)).unwrap();
    manifest.segments.iter().filter(|r| r.level == 0).count()
}

/// Drive rounds until the tiered pile holds at least `depth` L0 segments
/// (the M16 skip state), then return the round reached.
fn drive_to_depth(
    db: &Db,
    d: &std::path::Path,
    depth: usize,
    mut put: impl FnMut(&Db, usize),
) -> usize {
    let mut r = 0;
    loop {
        r += 1;
        put(db, r);
        if l0_count(d) >= depth {
            return r;
        }
        assert!(
            r < 40,
            "the tier never piled {depth} L0 segments — merge arithmetic drifted"
        );
    }
}

#[test]
fn tier_depth_answers_match_oracle() {
    // Sequential-key pile (the loader shape) driven to >= 10 L0 + L1:
    // every get and the full scan must stay byte-exact against an
    // independent oracle AND a count-only twin (ratio 0) fed the same
    // ops. The absent key pins the range-skip prune: with disjoint flush
    // ranges every segment is skipped by RANGE, zero blooms probed — the
    // loader-shaped fan-out costs nothing but range checks.
    let d = dir("tiered-read-oracle");
    let mut cfg = Config::new(d.clone());
    cfg.memtable_bytes = 512;
    let db = Db::open(cfg).unwrap();
    let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let rounds = drive_to_depth(&db, &d, 10, |db, r| {
        round_put(db, r);
        for i in 0..8 {
            let k = format!("k{r:03}{i:02}").into_bytes();
            let v = format!("v{r:03}{i:02}{}", "y".repeat(34)).into_bytes();
            oracle.insert(k, v);
        }
    });
    let s0 = db.read_path_stats();
    assert!(db.get(b"k99999").unwrap().is_none(), "absent key must miss");
    let s = db.read_path_stats();
    assert_eq!(
        s.segments_range_skipped - s0.segments_range_skipped,
        s.segments_considered - s0.segments_considered,
        "sequential ranges: every segment must be pruned by range"
    );
    assert_eq!(
        s.segments_bloom_skipped - s0.segments_bloom_skipped,
        0,
        "no bloom probed past a range skip"
    );
    assert_eq!(
        s.blocks_read - s0.blocks_read,
        0,
        "an absent get past every range skip reads zero blocks"
    );
    assert!(
        s.segments_considered - s0.segments_considered >= 11,
        "depth pin: {rounds} rounds must leave >= 11 segments"
    );

    // every written key + absent keys: byte-exact vs the oracle
    for (k, v) in &oracle {
        assert_eq!(
            db.get(k).unwrap().as_ref(),
            Some(v),
            "get diverged at {k:?}"
        );
    }
    for absent in [b"k99999".as_slice(), b"nope".as_slice()] {
        assert_eq!(db.get(absent).unwrap(), None, "absent diverged");
    }
    let scan: Vec<(Vec<u8>, Vec<u8>)> = db.scan(b"k").unwrap();
    let expect: Vec<(Vec<u8>, Vec<u8>)> = oracle.into_iter().collect();
    assert_eq!(scan, expect, "scan diverged at tier depth");

    // count-only twin: same ops, ratio 0 — identical answers (the tier
    // never changes an answer, it only changes when the merge fires)
    let d2 = dir("tiered-read-twin");
    let mut cfg = Config::new(d2.clone());
    cfg.memtable_bytes = 512;
    cfg.l0_tier_ratio = 0;
    let twin = Db::open(cfg).unwrap();
    for r in 1..=rounds {
        round_put(&twin, r);
    }
    for (k, v) in &expect {
        assert_eq!(
            twin.get(k).unwrap().as_ref(),
            Some(v),
            "twin diverged at {k:?}"
        );
    }
    assert_eq!(twin.scan(b"k").unwrap(), expect, "twin scan diverged");
    assert!(l0_count(&d2) < 10, "count-only must not pile L0");
}

#[test]
fn tier_depth_fanout_absorbed() {
    // Adversarial shape: every segment's range spans the target, so the
    // range skip cannot prune — the walk must fall through to the blooms.
    // The pin: for an absent key at >= 10 L0 + L1, ZERO segments are
    // range-skipped, the bloom/index identity holds exactly, and the
    // bloom absorbs the fan-out — only a handful of false-positive
    // segments reach the index search, and block I/O stays bounded by
    // that handful, never the depth.
    let d = dir("tiered-read-fanout");
    let mut cfg = Config::new(d.clone());
    cfg.memtable_bytes = 512;
    let db = Db::open(cfg).unwrap();
    let rounds = drive_to_depth(&db, &d, 10, band_put);
    let target = b"k005999".to_vec(); // inside every segment's band, written by nobody
    let s0 = db.read_path_stats();
    assert_eq!(db.get(&target).unwrap(), None, "band target must miss");
    let s = db.read_path_stats();
    let considered = s.segments_considered - s0.segments_considered;
    let range_skipped = s.segments_range_skipped - s0.segments_range_skipped;
    let bloom_skipped = s.segments_bloom_skipped - s0.segments_bloom_skipped;
    let index_searched = s.segments_index_searched - s0.segments_index_searched;
    assert!(
        considered >= 11,
        "depth pin: {rounds} rounds must leave >= 11 segments"
    );
    assert_eq!(
        range_skipped, 0,
        "overlapping bands: no segment may be range-skipped"
    );
    assert_eq!(
        bloom_skipped,
        considered - index_searched,
        "every non-searched segment must be bloom-skipped"
    );
    assert!(
        index_searched <= 3,
        "bloom must absorb the fan-out: {index_searched} false positives over {considered} segments"
    );
    // A bloom false positive legitimately reads its candidate block — the
    // honest pin is that the fan-out costs at most the false-positive
    // fraction's I/O, never one block per segment.
    assert!(
        s.blocks_read - s0.blocks_read <= index_searched,
        "block reads must stay bounded by the false positives, not the depth"
    );
}
