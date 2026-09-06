//! SE2-M6 — group commit REDs (design §7): a commit queue drains into
//! groups bounded by `max_batch_ops` / `max_batch_bytes` / `max_wait_duration`
//! and commits each group with ONE fsync, then applies and acks (never ack
//! before apply). Sync mode stays the correctness baseline — its WAL bytes
//! are the golden the group-commit path must reproduce exactly.

mod common;

use aikoql_storage_v2::db::{Config, Db, DurabilityMode, WAL_FILE};
use aikoql_storage_v2::wal::{replay_frames, Op};
use common::dir;
use std::path::PathBuf;
use std::time::Duration;

fn gc_config(dir: PathBuf) -> Config {
    let mut c = Config::new(dir);
    c.durability = DurabilityMode::GroupCommit; // explicit opt-in, never silent
    c
}

/// The engine's byte accounting: the sum over ops of key+value bytes
/// (a Delete carries only its key).
fn batch_bytes(ops: &[Op]) -> usize {
    ops.iter()
        .map(|op| match op {
            Op::Put(k, v) => k.len() + v.len(),
            Op::Delete(k) => k.len(),
            Op::CreateObject { .. } => 32, // oid 16 + lid 8 + rid 8
        })
        .sum()
}

#[test]
fn group_commit_coalesces_batches_into_one_fsync() {
    let d = dir("gc-coalesce");
    let mut cfg = gc_config(d.clone());
    cfg.max_wait_duration = Duration::from_millis(200); // long window
    let db = Db::open(cfg).unwrap();
    let writer = db.writer().unwrap();
    // 8 concurrent writers queue within microseconds of each other — the
    // window is 200 ms, so the drain must take every batch into ONE group.
    // (A single synchronous submitter cannot coalesce by construction: its
    // write blocks until the ack, so the queue never holds two batches.)
    let threads: Vec<_> = (0..8u64)
        .map(|i| {
            let writer = writer.clone();
            std::thread::spawn(move || {
                writer
                    .write(&[Op::Put(
                        format!("k{i}").into_bytes(),
                        format!("v{i}").into_bytes(),
                    )])
                    .unwrap()
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(
        db.fsync_count(),
        1,
        "one fsync for the whole group, not one per batch"
    );
    // Ack implies apply: each write returned only after its ack, and every
    // key is already visible.
    for i in 0..8u64 {
        assert_eq!(
            db.get(&format!("k{i}").into_bytes()).unwrap(),
            Some(format!("v{i}").into_bytes())
        );
    }
    drop(writer);
    drop(db);
    let db = Db::open(gc_config(d.clone())).unwrap();
    for i in 0..8u64 {
        assert_eq!(
            db.get(&format!("k{i}").into_bytes()).unwrap(),
            Some(format!("v{i}").into_bytes())
        );
    }
}

#[test]
fn group_commit_respects_max_batch_ops() {
    let d = dir("gc-cap-ops");
    let mut cfg = gc_config(d.clone());
    cfg.max_wait_duration = Duration::from_millis(200);
    cfg.max_batch_ops = 4;
    let db = Db::open(cfg).unwrap();
    let writer = db.writer().unwrap();
    // 10 concurrent one-op batches with cap 4 → exact-fit groups 4/4/2:
    // any packing of 10 into groups of ≤4 needs at least 3 groups, and the
    // 200 ms window never expires mid-burst, so exactly 3. Concurrent
    // submitters are what group commit is FOR — a single synchronous
    // submitter's write blocks until its ack, so nothing can coalesce.
    let threads: Vec<_> = (0..10u64)
        .map(|i| {
            let writer = writer.clone();
            std::thread::spawn(move || {
                writer
                    .write(&[Op::Put(
                        format!("k{i}").into_bytes(),
                        format!("v{i}").into_bytes(),
                    )])
                    .unwrap()
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(db.fsync_count(), 3, "cap 4 over 10 batches → 3 groups");
    drop(writer);
    drop(db);
    let db = Db::open(gc_config(d)).unwrap();
    for i in 0..10u64 {
        assert_eq!(
            db.get(&format!("k{i}").into_bytes()).unwrap(),
            Some(format!("v{i}").into_bytes())
        );
    }
}

#[test]
fn group_commit_respects_max_batch_bytes() {
    let d = dir("gc-cap-bytes");
    let mut cfg = gc_config(d.clone());
    cfg.max_wait_duration = Duration::from_millis(200);
    cfg.max_batch_bytes = 250;
    let db = Db::open(cfg).unwrap();
    let writer = db.writer().unwrap();
    // 100-byte batches with cap 250 → exact-fit groups of 2 (200 ≤ 250,
    // 300 > 250) → 5 groups. Concurrent submitters, same reasoning as the
    // ops-cap test.
    let v = vec![b'x'; 90];
    let threads: Vec<_> = (0..10u64)
        .map(|i| {
            let writer = writer.clone();
            let v = v.clone();
            std::thread::spawn(move || {
                let ops = [Op::Put(format!("key-{i:06}").into_bytes(), v)];
                assert_eq!(batch_bytes(&ops), 100, "the pin assumes 100-byte batches");
                writer.write(&ops).unwrap()
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(
        db.fsync_count(),
        5,
        "cap 250 over ten 100-byte batches → groups of 2"
    );
    drop(writer);
    drop(db);
    let db = Db::open(gc_config(d)).unwrap();
    for i in 0..10u64 {
        assert_eq!(
            db.get(&format!("key-{i:06}").into_bytes()).unwrap(),
            Some(v.clone())
        );
    }
}

#[test]
fn ack_after_apply_and_orders_hold() {
    let d = dir("gc-order");
    let mut cfg = gc_config(d.clone());
    cfg.max_wait_duration = Duration::ZERO; // every batch its own group
    let db = Db::open(cfg).unwrap();
    let writer = db.writer().unwrap();
    let mut last_seq = 0;
    for i in 1..=20u64 {
        let ops = [Op::Put(
            format!("k{i:02}").into_bytes(),
            format!("v{i:02}").into_bytes(),
        )];
        let seq = writer.write(&ops).unwrap();
        // Ack order == apply order == log order: the seq strictly
        // increases in submission order...
        assert!(seq > last_seq, "seq {seq} after {last_seq}");
        assert_eq!(seq, i, "seqs are 1..=20 in submission order");
        last_seq = seq;
        // ...and the ack is only sent AFTER the apply — the just-acked
        // batch is already visible.
        assert_eq!(
            db.get(&format!("k{i:02}").into_bytes()).unwrap(),
            Some(format!("v{i:02}").into_bytes()),
            "batch {i} visible immediately after its ack"
        );
    }
    drop(writer);
    drop(db);

    // Log order: the WAL holds 20 frames with seqs 1..=20 in order.
    let wal_bytes = std::fs::read(d.join(WAL_FILE)).unwrap();
    let (frames, consumed) = replay_frames(&wal_bytes).unwrap();
    assert_eq!(consumed, wal_bytes.len(), "no torn tail after clean ack");
    assert_eq!(frames.len(), 20);
    for (i, frame) in frames.iter().enumerate() {
        let i = i as u64 + 1;
        assert_eq!(frame.seq, i, "WAL frames strictly ordered by seq");
        assert_eq!(
            frame.ops,
            vec![Op::Put(
                format!("k{i:02}").into_bytes(),
                format!("v{i:02}").into_bytes()
            )],
            "frame {i} payload byte-exact"
        );
    }

    let db = Db::open(gc_config(d)).unwrap();
    for i in 1..=20u64 {
        assert_eq!(
            db.get(&format!("k{i:02}").into_bytes()).unwrap(),
            Some(format!("v{i:02}").into_bytes())
        );
    }
}

/// The deterministic 50-op workload both modes must commit identically.
fn workload() -> Vec<Op> {
    let mut ops = Vec::new();
    for i in 0..50u64 {
        ops.push(match i % 5 {
            0 => Op::Put(
                format!("k{:03}", i % 20).into_bytes(),
                format!("v{i:03}").into_bytes(),
            ),
            1 => Op::Put(
                format!("k{:03}", i % 20).into_bytes(),
                format!("w{i:03}").into_bytes(),
            ),
            2 => Op::Delete(format!("k{:03}", (i + 3) % 20).into_bytes()),
            _ => Op::Put(
                format!("k{:03}", i % 20).into_bytes(),
                format!("u{i:03}").into_bytes(),
            ),
        });
    }
    ops
}

#[test]
fn sync_and_group_commit_wals_are_byte_identical() {
    let d_sync = dir("gc-parity-sync");
    {
        let db = Db::open(Config::new(d_sync.clone())).unwrap();
        for op in &workload() {
            db.write(std::slice::from_ref(op)).unwrap();
        }
    }
    let d_gc = dir("gc-parity-gc");
    {
        let mut cfg = gc_config(d_gc.clone());
        cfg.max_wait_duration = Duration::ZERO;
        let db = Db::open(cfg).unwrap();
        let writer = db.writer().unwrap();
        for op in &workload() {
            writer.write(std::slice::from_ref(op)).unwrap();
        }
        drop(writer);
    }

    // The WAL bytes are the durability contract: Sync assigns per-batch
    // seqs and appends frames; group commit must produce the exact same
    // frames — same seqs, same payloads, byte for byte.
    let sync_wal = std::fs::read(d_sync.join(WAL_FILE)).unwrap();
    let gc_wal = std::fs::read(d_gc.join(WAL_FILE)).unwrap();
    assert_eq!(
        sync_wal, gc_wal,
        "group commit must not change a single WAL byte"
    );

    // Same state, too.
    let db_sync = Db::open(Config::new(d_sync)).unwrap();
    let db_gc = Db::open(gc_config(d_gc)).unwrap();
    for i in 0..20u64 {
        let k = format!("k{i:03}").into_bytes();
        assert_eq!(db_sync.get(&k).unwrap(), db_gc.get(&k).unwrap());
    }
}

// The throughput matrix moved to tests/group_commit_evidence.rs (SE2-M13):
// the M6 version's 8-writer arm ran 200 batches per writer but labeled the
// report cell ×25, hiding the coalescing. `group_commit_effectiveness`
// regenerates artifacts/storage-engine-v2/group-commit.md with honest
// `batches_submitted` cells.
