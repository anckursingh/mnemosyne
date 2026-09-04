//! SE2-M13 — real group commit evidence (QA M7). The M6 matrix's 8-writer
//! arm mislabeled 200-per-writer code as ×25 batches and hid the
//! coalescing; the committer itself was always correct — these tests pin
//! that: a deterministic 20-batch group, 100 000 commit schedules, and
//! the corrected effectiveness matrix (`SE2M6_NIGHTLY=1` strict opt-in,
//! cells named `batches_submitted`). The M6 suite and the group_crash
//! windows are the regression.

mod common;

use aikoql_storage_v2::db::{Config, Db, DurabilityMode};
use aikoql_storage_v2::wal::Op;
use common::dir;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

fn gc_config(dir: std::path::PathBuf) -> Config {
    let mut c = Config::new(dir);
    c.durability = DurabilityMode::GroupCommit; // explicit opt-in, never silent
    c
}

/// QA TC-PERF-0701 — 20 concurrent single-op batches released together
/// behind a barrier: the 200 ms window takes every batch into ONE group.
/// `fsync_count < 20` is the loose pin, ≤ 5 the strong one (ideal: 1).
#[test]
fn deterministic_batch() {
    const BATCHES: usize = 20;
    let d = dir("gc-deterministic");
    let mut cfg = gc_config(d.clone());
    cfg.max_wait_duration = Duration::from_millis(200);
    let db = Db::open(cfg).unwrap();
    let writer = db.writer().unwrap();
    let barrier = Arc::new(Barrier::new(BATCHES + 1));
    let threads: Vec<_> = (0..BATCHES)
        .map(|i| {
            let writer = writer.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait(); // every thread released together
                writer
                    .write(&[Op::Put(
                        format!("k{i:02}").into_bytes(),
                        format!("v{i:02}").into_bytes(),
                    )])
                    .unwrap()
            })
        })
        .collect();
    barrier.wait();
    for t in threads {
        t.join().unwrap();
    }
    let fsyncs = db.fsync_count();
    assert!(
        fsyncs < BATCHES as u64,
        "no coalescing: {fsyncs} fsyncs for {BATCHES} simultaneous batches"
    );
    assert!(
        fsyncs <= 5,
        "the strong pin: 20 simultaneous batches should group near 1 fsync, got {fsyncs}"
    );
    drop(writer);
    drop(db);
    let db = Db::open(gc_config(d)).unwrap();
    for i in 0..BATCHES {
        assert_eq!(
            db.get(&format!("k{i:02}").into_bytes()).unwrap(),
            Some(format!("v{i:02}").into_bytes()),
            "batch {i} lost"
        );
    }
}

/// QA TC-PERF-0703 — 100 000 commit schedules: 8 persistent writers ×
/// 12 500 batches each, wait=0 (drain-at-wake). Pins: per writer the seqs
/// strictly increase in submission order (a blocking ack makes ack order =
/// submission order); every seq globally unique (never assigned twice);
/// the seqs are exactly 1..=100 000 — nothing lost, no phantom.
#[test]
fn commit_ordering() {
    const WRITERS: usize = 8;
    const EACH: usize = 12_500;
    let d = dir("gc-ordering");
    let mut cfg = gc_config(d.clone());
    cfg.max_wait_duration = Duration::ZERO;
    let db = Db::open(cfg).unwrap();
    let seen: Arc<Mutex<HashSet<u64>>> =
        Arc::new(Mutex::new(HashSet::with_capacity(WRITERS * EACH)));
    let threads: Vec<_> = (0..WRITERS)
        .map(|w| {
            let writer = db.writer().unwrap();
            let seen = Arc::clone(&seen);
            std::thread::spawn(move || {
                let mut last = 0u64;
                for i in 0..EACH {
                    let seq = writer
                        .write(&[Op::Put(
                            format!("w{w}-{i:05}").into_bytes(),
                            format!("v{w}-{i:05}").into_bytes(),
                        )])
                        .unwrap();
                    assert!(seq > last, "writer {w}: seq {seq} not after {last}");
                    last = seq;
                    let mut s = seen.lock().unwrap();
                    assert!(s.insert(seq), "seq {seq} assigned twice");
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    // 100 000 distinct seqs whose min is 1 and max is 100 000 IS exactly
    // 1..=100 000: no gap, no phantom.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), WRITERS * EACH, "seq count");
    assert_eq!(*seen.iter().min().unwrap(), 1);
    assert_eq!(*seen.iter().max().unwrap(), (WRITERS * EACH) as u64);
    // A sample of the 100 000 acked batches survives recovery.
    drop(db);
    let db = Db::open(gc_config(d)).unwrap();
    for (w, i) in [(0usize, 0usize), (3, 5_000), (7, EACH - 1)] {
        let k = format!("w{w}-{i:05}").into_bytes();
        let v = format!("v{w}-{i:05}").into_bytes();
        assert_eq!(db.get(&k).unwrap(), Some(v), "acked batch w{w}-{i:05} lost");
    }
}

// ---------------------------------------------------------------------------
// SE2M6_NIGHTLY — the corrected effectiveness matrix (SE2-M13). Strict
// opt-in: unset skips, any value other than "1" panics. Perf numbers are
// report cells, never asserts — the report regenerates only with the env set.
// ---------------------------------------------------------------------------

const GATE: &str = "SE2M6_NIGHTLY";

fn nightly_on() -> bool {
    match std::env::var(GATE) {
        Err(_) => false,
        Ok(v) if v == "1" => true,
        Ok(v) => panic!("{GATE} must be unset or \"1\", got {v:?} (strict opt-in)"),
    }
}

/// QA TC-PERF-0704 — the corrected matrix: 200 batches TOTAL (8 writers ×
/// 25 DISTINCT each — the M6 matrix's 8-writer arm ran 200 per writer and
/// labeled it ×25), wait=0 default (the window is dead time under the
/// blocking ack). The effectiveness pin: the 8-writer arm's fsyncs < 200 —
/// coalescing at wait=0 is real and comes from concurrent submitters.
/// Per-batch latencies are report cells.
#[test]
fn group_commit_effectiveness() {
    if !nightly_on() {
        eprintln!("SKIPPED (set SE2M6_NIGHTLY=1 to run the effectiveness matrix)");
        return;
    }
    const BATCHES: usize = 200;
    const WRITERS: usize = 8;
    const EACH: usize = BATCHES / WRITERS; // 25 — the honest per-writer count
    let val = vec![b'x'; 128];
    let mut report = String::new();

    // (a) Sync, 1 writer, batches_submitted=200 — the baseline.
    let d = dir("gc-eff-sync");
    let t0 = std::time::Instant::now();
    let sync_fsyncs = {
        let db = Db::open(Config::new(d.clone())).unwrap();
        for i in 0..BATCHES {
            db.write(&[Op::Put(format!("key-{i:04}").into_bytes(), val.clone())])
                .unwrap();
        }
        db.fsync_count()
    };
    let sync_ms = t0.elapsed().as_millis();
    report.push_str(&format!(
        "- Sync, 1 writer, batches_submitted={BATCHES}: {sync_ms} ms, {sync_fsyncs} fsyncs, {:.2} ms/batch\n",
        sync_ms as f64 / BATCHES as f64
    ));

    // (b) GC, 1 writer, wait=0, batches_submitted=200 — the blocking ack
    // caps in-flight at 1 per writer: nothing can coalesce by construction.
    let d = dir("gc-eff-gc1");
    let t0 = std::time::Instant::now();
    let gc1_fsyncs = {
        let mut cfg = gc_config(d.clone());
        cfg.max_wait_duration = Duration::ZERO;
        let db = Db::open(cfg).unwrap();
        let writer = db.writer().unwrap();
        for i in 0..BATCHES {
            writer
                .write(&[Op::Put(format!("key-{i:04}").into_bytes(), val.clone())])
                .unwrap();
        }
        let n = db.fsync_count();
        drop(writer);
        n
    };
    let gc1_ms = t0.elapsed().as_millis();
    assert_eq!(
        gc1_fsyncs, BATCHES as u64,
        "a single blocking writer cannot coalesce — the ceiling is in-flight = 1"
    );
    report.push_str(&format!(
        "- GroupCommit, 1 writer, wait=0, batches_submitted={BATCHES}: {gc1_ms} ms, {gc1_fsyncs} fsyncs, {:.2} ms/batch\n",
        gc1_ms as f64 / BATCHES as f64
    ));

    // (c) GC, 8 writers × 25 DISTINCT batches = 200 submitted, wait=0.
    let d = dir("gc-eff-gc8");
    let t0 = std::time::Instant::now();
    let gc8_fsyncs = {
        let mut cfg = gc_config(d.clone());
        cfg.max_wait_duration = Duration::ZERO;
        let db = Db::open(cfg).unwrap();
        let writers: Vec<_> = (0..WRITERS).map(|_| db.writer().unwrap()).collect();
        let mut threads = Vec::new();
        for (w, writer) in writers.iter().enumerate() {
            let writer = writer.clone();
            let val = val.clone();
            threads.push(std::thread::spawn(move || {
                for i in 0..EACH {
                    let k = format!("key-{:04}", w * EACH + i).into_bytes();
                    writer.write(&[Op::Put(k, val.clone())]).unwrap();
                }
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
        let n = db.fsync_count();
        drop(writers);
        n
    };
    let gc8_ms = t0.elapsed().as_millis();
    assert!(
        gc8_fsyncs < BATCHES as u64,
        "8 concurrent writers at wait=0 must coalesce: {gc8_fsyncs} fsyncs for {BATCHES} batches"
    );
    report.push_str(&format!(
        "- GroupCommit, {WRITERS} writers × {EACH}, wait=0, batches_submitted={BATCHES}: {gc8_ms} ms, {gc8_fsyncs} fsyncs, {:.2} ms/batch, avg group {:.1}\n",
        gc8_ms as f64 / BATCHES as f64,
        BATCHES as f64 / gc8_fsyncs as f64,
    ));
    // Correctness under load: every one of the 200 acked batches is there.
    let db = Db::open(gc_config(d)).unwrap();
    for i in 0..BATCHES {
        assert_eq!(
            db.get(&format!("key-{i:04}").into_bytes()).unwrap(),
            Some(val.clone()),
            "acked batch {i} lost"
        );
    }

    let machine = format!(
        "{}/{}; {} logical cores; {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "processor NOT_REPORTED".into()),
    );
    let report = format!(
        "# Group Commit Effectiveness — SE2-M13\n\n\
         Generated only when `SE2M6_NIGHTLY=1` (strict opt-in). Perf numbers are\n\
         report cells, never asserts — the report regenerates only with the env set.\n\n\
         - Test: `group_commit_effectiveness`\n\
         - Build mode: {build}\n\
         - Machine: {machine}\n\
         - Workload: 200 single-op batches TOTAL, 128-byte values, 1 MiB+ memtable\n\
           (no flush during the run); arm (c) = 8 writers × 25 DISTINCT batches —\n\
           the M6 matrix's 8-writer arm ran 200 per writer and labeled it ×25,\n\
           hiding the coalescing; cells name `batches_submitted` so a row cannot\n\
           lie again.\n\n\
         {report}\n\
         - Pipelining ceiling (SE2-M13, documented): in-flight = 1 per writer by\n\
           design (the blocking ack); coalescing = concurrent-submitter count, not\n\
           the wait window — under the blocking API the window is dead time (the M6\n\
           wait=5ms arm — 1600 batches, mislabeled ×25 — measured 3131 ms wall with\n\
           200 groups: ~5 ms window tax per group for zero extra coalescing,\n\
           2026-09-02), so the default stays ZERO. Upgrade path, if a workload ever\n\
           needs window-filling: a non-blocking submit API.\n",
        build = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
    );
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("artifacts")
        .join("storage-engine-v2");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("group-commit.md"), report).unwrap();
}
