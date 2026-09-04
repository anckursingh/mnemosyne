//! SE2-M16 — size-tiered compaction trigger (rss-root-cause.md option 5):
//! the L0 count trigger (SE2-M10) is the floor, but a full KeepAll merge
//! rewrites the whole accumulated dataset — the size gate skips it while
//! L0 is not yet a material fraction of L1 (`l0_tier_ratio`, default 1:
//! merge only when L0 bytes >= L1 bytes / ratio; L1 empty always merges;
//! 0 restores the M10 count-only behavior). The quadratic bulk-seed wall
//! is the measured motivation; the pins here are the policy and the
//! read path through an unmerged L0 pile.

mod common;

use aikoql_storage_v2::db::{manifest_path, Config, Db};
use aikoql_storage_v2::format::{Current, Manifest};
use common::dir;

/// One flush per round: 8 puts x ~64 B (memtable accounting) crosses a
/// 512-byte memtable from empty, and the flush check runs once per put —
/// exactly one L0 segment per 8-put round. All keys distinct across
/// rounds, so every flush is ~the same size F and the trigger arithmetic
/// is exact. (Measured geometry: flush file ~690 B, a merged file is
/// ~0.77x the sum of its sources — per-file header/index/bloom overhead
/// collapses at merge.)
fn round_put(db: &Db, r: usize) {
    for i in 0..8 {
        let k = format!("k{r:03}{i:02}").into_bytes();
        let v = format!("v{r:03}{i:02}{}", "y".repeat(34)).into_bytes();
        db.put(&k, &v).unwrap();
    }
}

fn seg_count(d: &std::path::Path) -> usize {
    std::fs::read_dir(d)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("SEGMENT-"))
        .count()
}

fn levels(d: &std::path::Path) -> Vec<u8> {
    let current = Current::read(&d.join("CURRENT")).unwrap();
    let manifest = Manifest::read(&manifest_path(d, current.manifest_generation)).unwrap();
    let mut levels: Vec<u8> = manifest.segments.iter().map(|r| r.level).collect();
    levels.sort_unstable();
    levels
}

#[test]
fn tiered_trigger_skips_small_l0() {
    // Trigger 4, ratio 1, one uniform ~F flush per round. Measured trace
    // (probe, this fixture): a flush file is ~690 B but a merged file is
    // ~0.77x the sum of its sources (per-file header/index/bloom overhead
    // collapses at merge), so the gate's boundary moves:
    //   round 4:  L0 4F vs L1 empty -> merge, L1 = 4F (~2126 B)
    //   round 8:  L0 4F (~2761) >= L1 (~2126) -> merge, L1 = 8F (~4047)
    //   round 12: L0 4F < L1 8F     -> SKIP, L0 grows past the trigger
    //   round 14: L0 6F (~4140) >= L1 (~4047) -> merge
    //   round 20: L0 6F vs L1 14F   -> SKIP (six L0 + one L1)
    // The M10 count-only policy merged at every 4th flush — the tiered
    // gate holds the L0 pile past the trigger until it is a material
    // fraction of L1 (pinned against the measured sizes).
    let d = dir("tiered-skip");
    let mut cfg = Config::new(d.clone());
    cfg.memtable_bytes = 512;
    let db = Db::open(cfg).unwrap();
    for r in 1..=8 {
        round_put(&db, r);
    }
    assert_eq!(seg_count(&d), 1, "round 8: 4F vs 4F must merge");
    for r in 9..=12 {
        round_put(&db, r);
    }
    assert_eq!(seg_count(&d), 5, "round 12: 4F vs 8F must be skipped");
    assert_eq!(levels(&d), vec![0, 0, 0, 0, 1], "one L1 + four L0");
    round_put(&db, 13);
    assert_eq!(seg_count(&d), 6, "round 13: still below L1 size");
    round_put(&db, 14);
    assert_eq!(seg_count(&d), 1, "round 14: 6F clears L1");
    for r in 15..=20 {
        round_put(&db, r);
    }
    assert_eq!(seg_count(&d), 7, "round 20: 6F vs 14F must be skipped");
}

#[test]
fn tiered_skip_reads_walk_unmerged_l0() {
    // At the round-12 state (L1 + four unmerged L0s), a get must walk the
    // L0 pile newest-first, a scan must layer-merge it newest-wins — and
    // the state must survive a reopen (the manifest already lists every
    // segment). The extra put (round 10, put 9) lands in flush 11, inside
    // the unmerged L0 pile; its v1 went to L1 in the round-4 merge, so
    // k00100 spans the levels and must resolve to the L0 head.
    let d = dir("tiered-reads");
    let mut cfg = Config::new(d.clone());
    cfg.memtable_bytes = 512;
    let db = Db::open(cfg).unwrap();
    for r in 1..=12 {
        round_put(&db, r);
        if r == 10 {
            let v2 = format!("v2{}", "z".repeat(40)).into_bytes();
            db.put(b"k00100", &v2).unwrap();
        }
    }
    assert_eq!(seg_count(&d), 5, "precondition: skip state");
    assert_eq!(
        db.get(b"k00100").unwrap(),
        Some(b"v2zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".to_vec())
    );
    // 12 rounds x 8 keys = 96 distinct keys, k00100 written twice —
    // every version collapsed to its head across L1 + L0 + memtable.
    let rows = db.scan(b"k").unwrap();
    assert_eq!(rows.len(), 96);
    assert_eq!(
        rows.iter()
            .find(|(k, _)| k == b"k00100")
            .map(|(_, v)| v.as_slice()),
        Some(b"v2zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".as_slice()),
        "the L0 head must win over the L1 version"
    );
    // A key that only ever lived in L1 is still served.
    assert!(db.get(b"k00101").unwrap().is_some());
    drop(db);
    let db = Db::open(Config::new(d.clone())).unwrap();
    assert_eq!(
        db.get(b"k00100").unwrap(),
        Some(b"v2zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".to_vec())
    );
    assert_eq!(seg_count(&d), 5, "reopen keeps the L0 pile");
}

#[test]
fn tiered_ratio_zero_is_count_only() {
    // ratio 0 = the M10 count-only policy: every 4th flush merges, so the
    // segment count stays 1 through the same rounds that ratio 1 skips.
    let d = dir("tiered-count-only");
    let mut cfg = Config::new(d.clone());
    cfg.memtable_bytes = 512;
    cfg.l0_tier_ratio = 0;
    let db = Db::open(cfg).unwrap();
    for r in 1..=20 {
        round_put(&db, r);
        if r % 4 == 0 {
            assert_eq!(seg_count(&d), 1, "round {r}: count-only must merge");
        }
    }
    // ratio 2 = L0 >= L1/2: the looser gate still diverges from
    // count-only once the merged L1 outgrows the trigger — measured
    // trace: merges at 4, 8, 12, then 4F (~2761 B) < L1/2 (~2900 B) at
    // round 16 (SKIP, five segments), and the 5F pile (~3451 B) clears
    // the half-tier at round 17 (MERGE). Pins the integer division at
    // the real boundary.
    let d = dir("tiered-ratio2");
    let mut cfg = Config::new(d.clone());
    cfg.memtable_bytes = 512;
    cfg.l0_tier_ratio = 2;
    let db = Db::open(cfg).unwrap();
    for r in 1..=12 {
        round_put(&db, r);
    }
    assert_eq!(seg_count(&d), 1, "round 12: 4F >= L1/2 must merge");
    for r in 13..=16 {
        round_put(&db, r);
    }
    assert_eq!(seg_count(&d), 5, "round 16: 4F < L1/2 must be skipped");
    round_put(&db, 17);
    assert_eq!(seg_count(&d), 1, "round 17: 5F clears L1/2");
}
