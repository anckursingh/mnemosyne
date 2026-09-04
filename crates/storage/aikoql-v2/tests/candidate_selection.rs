//! SE2-M10 — candidate selection at the engine level (docs/IMPLEMENTATION-PLAN-V2.md
//! SE2-M10, docs/TESTING-PLAN-V2.md row SE2-M10): Arc segments so a get
//! never holds the state lock across a disk read (the W8 write-stall), the
//! one-alloc memtable probe, and the L0 compaction trigger that keeps the
//! steady state at one L1 + the active L0 (segments_considered ≤ 2).

mod common;

use aikoql_kernel::knowledge::kom::sha256;
use aikoql_storage_v2::db::{manifest_path, CommitWriter, Config, Db, DurabilityMode};
use aikoql_storage_v2::format::{Current, Manifest};
use aikoql_storage_v2::wal::Op;
use common::{dir, percentiles};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The M1 bloom spec (m = 10·n, 4 probes, double hashing over sha256) —
/// same independent re-implementation as the M8 suite: the test's
/// expectation must not be the engine's code.
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

/// An ABSENT key between "b-a" and "b-z" whose four probes all land on bits
/// the two filler keys set (m = 20, two entries per segment): it provably
/// passes every filler segment's bloom, so a get of it can never be skipped
/// — every block is read. The search is bounded and deterministic.
fn passing_key() -> String {
    let mut set = [false; 20];
    for k in [b"b-a".as_slice(), b"b-z".as_slice()] {
        for p in probes(k, 20) {
            set[p as usize] = true;
        }
    }
    for i in 0..10_000u32 {
        let k = format!("b-m{i}");
        if probes(k.as_bytes(), 20).iter().all(|p| set[*p as usize]) {
            return k;
        }
    }
    panic!("no bloom-passing key found");
}

#[test]
fn get_does_not_stall_writers_during_disk_read() {
    // One writer (GroupCommit — one fsync per batch) measures ack latency
    // while a getter thread hammers a get that reads SIX 16 MiB blocks: the
    // target is absent but provably inside every segment's range and bloom,
    // so neither skip fires and all six blocks are read per get. With Arc
    // segments the state lock is held only while the arcs are cloned — the
    // writer's p50 ack latency stays in the fsync class. Without the fix the
    // getter holds the lock across ~96 MiB of reads and the p50 inflates by
    // the whole get duration (the W8 write-stall).
    const SEGS: usize = 6;
    const FILL: usize = 8 << 20; // two values per segment → one 16 MiB block
    let mut cfg = Config::new(dir("m10-stall"));
    cfg.durability = DurabilityMode::GroupCommit;
    cfg.cache_bytes = 0;
    cfg.block_target = 1 << 30; // one block per segment
    cfg.memtable_bytes = usize::MAX;
    let db = Db::open(cfg).unwrap();
    let target = passing_key();
    for i in 0..SEGS {
        // ponytail: constant-fill values — compression is none (segment.rs),
        // so blocks are 16 MiB regardless; switch to noise if compression lands.
        db.put(format!("b-a{i}").as_bytes(), &vec![i as u8; FILL][..])
            .unwrap();
        db.put(format!("b-z{i}").as_bytes(), &vec![(i + 1) as u8; FILL][..])
            .unwrap();
        db.flush().unwrap();
    }

    let w = db.writer().unwrap();
    let submit = |w: &CommitWriter| -> u128 {
        let t = Instant::now();
        w.write(&[Op::Put(b"a".to_vec(), vec![1])]).unwrap();
        t.elapsed().as_micros()
    };
    let control: Vec<u128> = (0..20).map(|_| submit(&w)).collect();

    let stop = Arc::new(AtomicBool::new(false));
    let (contention, gets, ok) = std::thread::scope(|s| {
        let h = s.spawn(|| {
            let mut ok = true;
            let mut gets = 0u64;
            while !stop.load(Ordering::Relaxed) {
                ok &= db.get(target.as_bytes()).unwrap().is_none();
                gets += 1;
            }
            (ok, gets)
        });
        std::thread::sleep(Duration::from_millis(300)); // getter at speed
        let contention: Vec<u128> = (0..20).map(|_| submit(&w)).collect();
        stop.store(true, Ordering::Relaxed);
        let (ok, gets) = h.join().unwrap();
        (contention, gets, ok)
    });

    let (ctrl_p50, _, _) = percentiles(control);
    let (cont_p50, _, _) = percentiles(contention);
    assert!(ok, "the getter diverged under concurrent writes");
    assert!(
        gets >= 10,
        "the getter made {gets} gets — the stall pin needs real reads"
    );
    assert!(
        cont_p50 < ctrl_p50 + 3_000,
        "writer ack p50 inflates by the get duration: {cont_p50}µs under \
         contention vs {ctrl_p50}µs control — a get must not hold the state \
         lock across the disk read"
    );
}

#[test]
fn compaction_trigger_bounds_candidates() {
    let d = dir("m10-trigger");
    let mut cfg = Config::new(d.clone());
    cfg.memtable_bytes = 16; // every put flushes its own L0 segment
    cfg.cache_bytes = 0;
    let db = Db::open(cfg).unwrap();
    for i in 0..4 {
        db.put(format!("k{i}").as_bytes(), b"v").unwrap();
    }

    // The 4th write's flush hit the trigger: four L0 → one L1 (KeepAll).
    let current = Current::read(&d.join("CURRENT")).unwrap();
    let manifest = Manifest::read(&manifest_path(&d, current.manifest_generation)).unwrap();
    assert_eq!(
        manifest.segments.len(),
        1,
        "the trigger must compact 4 L0 into one L1"
    );
    assert_eq!(manifest.segments[0].level, 1);
    assert_eq!(manifest.segments[0].record_count, 4);

    // Steady state: one L1 + the active L0 — a get considers ≤ 2 segments
    // (the QA M1 bound, achieved by compaction not a catalog), and the
    // probe order is L0 newest-first, then L1 (pinned: the L0 range-skip
    // fires, the L1 search wins).
    db.put(b"k9", b"v").unwrap(); // one fresh L0 — no trigger at 1
    assert_eq!(db.get(b"k0").unwrap(), Some(b"v".to_vec()));
    let s = db.read_path_stats();
    assert_eq!(s.lookups, 1);
    assert_eq!(s.segments_considered, 2, "one L0 + one L1");
    assert_eq!(s.segments_range_skipped, 1, "L0 rejected by range");
    assert_eq!(s.segments_index_searched, 1, "L1 searched");

    // Manifest shape: L1 first (older), then L0.
    let current = Current::read(&d.join("CURRENT")).unwrap();
    let manifest = Manifest::read(&manifest_path(&d, current.manifest_generation)).unwrap();
    assert_eq!(manifest.segments.len(), 2);
    assert_eq!(manifest.segments[0].level, 1);
    assert_eq!(manifest.segments[1].level, 0);

    // The trigger runs KeepAll — the dataset survives byte-exact.
    let expect: Vec<(Vec<u8>, Vec<u8>)> = (0..4)
        .map(|i| (format!("k{i}").into_bytes(), b"v".to_vec()))
        .chain(std::iter::once((b"k9".to_vec(), b"v".to_vec())))
        .collect();
    assert_eq!(db.scan(b"").unwrap(), expect);
}

#[test]
fn explicit_flush_never_triggers_compaction() {
    // The trigger lives on the write path only: an explicit flush is the
    // caller's checkpoint and must never auto-compact (the flush/crash
    // suites pin their layouts on that).
    let d = dir("m10-flush-no-trigger");
    let mut cfg = Config::new(d.clone());
    cfg.memtable_bytes = usize::MAX;
    let db = Db::open(cfg).unwrap();
    for i in 0..4 {
        db.put(format!("k{i}").as_bytes(), b"v").unwrap();
        db.flush().unwrap();
    }
    let current = Current::read(&d.join("CURRENT")).unwrap();
    let manifest = Manifest::read(&manifest_path(&d, current.manifest_generation)).unwrap();
    assert_eq!(
        manifest.segments.len(),
        4,
        "four explicit flushes → four L0"
    );
    assert!(manifest.segments.iter().all(|r| r.level == 0));
}
