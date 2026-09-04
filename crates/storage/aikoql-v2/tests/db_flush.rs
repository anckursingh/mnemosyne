//! SE2-M2 — flush: visibility during rotation, segment/manifest publication,
//! threshold trigger, crash-window idempotency (TESTING-PLAN-V2 row V2-M2).

mod common;

use aikoql_storage_v2::db::{manifest_path, segment_path, Config, Db, WAL_FILE};
use aikoql_storage_v2::format::{Current, Manifest};
use aikoql_storage_v2::wal::{encode_frame, Op};
use common::dir;
use std::io::Write;

#[test]
fn writes_visible_during_flush() {
    let d = dir("flush-visible");
    let db = Db::open(Config::new(d.clone())).unwrap();
    db.put(b"k1", b"v1").unwrap();
    db.rotate(); // flush's first half: active → immutable
    db.put(b"k2", b"v2").unwrap(); // lands in the fresh active
                                   // during flush: both the rotated and the new write are visible
    assert_eq!(db.get(b"k1").unwrap(), Some(b"v1".to_vec()));
    assert_eq!(db.get(b"k2").unwrap(), Some(b"v2".to_vec()));
    db.flush().unwrap();
    assert_eq!(db.get(b"k1").unwrap(), Some(b"v1".to_vec()));
    assert_eq!(db.get(b"k2").unwrap(), Some(b"v2".to_vec()));
    drop(db);
    let db = Db::open(Config::new(d.clone())).unwrap();
    assert_eq!(db.get(b"k1").unwrap(), Some(b"v1".to_vec()));
    assert_eq!(db.get(b"k2").unwrap(), Some(b"v2".to_vec()));
}

#[test]
fn flush_publishes_segment_manifest_and_truncates_wal() {
    let d = dir("flush-publish");
    let db = Db::open(Config::new(d.clone())).unwrap();
    db.put(b"k1", b"v1").unwrap();
    db.put(b"k2", b"v2").unwrap();
    db.put(b"k3", b"v3").unwrap();
    db.flush().unwrap();

    let current = Current::read(&d.join("CURRENT")).unwrap();
    assert_eq!(
        current.manifest_generation, 2,
        "flush must bump the generation"
    );
    let manifest = Manifest::read(&manifest_path(&d, 2)).unwrap();
    assert_eq!(manifest.segments.len(), 1);
    let rec = &manifest.segments[0];
    assert_eq!(rec.segment_id, 1);
    assert_eq!(rec.level, 0);
    assert_eq!(rec.key_min, b"k1");
    assert_eq!(rec.key_max, b"k3");
    assert_eq!(rec.seq_lo, 1);
    assert_eq!(rec.seq_hi, 3);
    assert_eq!(rec.record_count, 3);
    assert!(segment_path(&d, 1).exists());
    assert_eq!(
        std::fs::metadata(d.join(WAL_FILE)).unwrap().len(),
        0,
        "WAL truncated at publication"
    );
}

#[test]
fn threshold_triggers_flush() {
    let d = dir("flush-threshold");
    let mut cfg = Config::new(d.clone());
    cfg.memtable_bytes = 64;
    let db = Db::open(cfg).unwrap();
    for i in 0..5u64 {
        db.put(format!("k{i}").as_bytes(), format!("v{i}").as_bytes())
            .unwrap();
    }
    let current = Current::read(&d.join("CURRENT")).unwrap();
    let manifest = Manifest::read(&manifest_path(&d, current.manifest_generation)).unwrap();
    assert!(
        !manifest.segments.is_empty(),
        "threshold must flush without an explicit call"
    );
    for i in 0..5u64 {
        assert_eq!(
            db.get(format!("k{i}").as_bytes()).unwrap(),
            Some(format!("v{i}").into_bytes())
        );
    }
    drop(db);
    let db = Db::open(Config::new(d.clone())).unwrap();
    for i in 0..5u64 {
        assert_eq!(
            db.get(format!("k{i}").as_bytes()).unwrap(),
            Some(format!("v{i}").into_bytes())
        );
    }
}

#[test]
fn crash_window_replay_is_idempotent() {
    // The window between manifest/CURRENT publication and WAL truncate:
    // the WAL still holds the flushed batches. Replay must not duplicate
    // state — same (key, seq) → same value, no sequence reuse.
    let d = dir("flush-window");
    let db = Db::open(Config::new(d.clone())).unwrap();
    assert_eq!(db.put(b"k1", b"v1").unwrap(), 1);
    db.flush().unwrap();
    assert_eq!(std::fs::metadata(d.join(WAL_FILE)).unwrap().len(), 0);
    // simulate the crash window: re-append the flushed batch to the WAL
    let frame = encode_frame(1, &[Op::Put(b"k1".to_vec(), b"v1".to_vec())]).unwrap();
    let mut wal = std::fs::OpenOptions::new()
        .append(true)
        .open(d.join(WAL_FILE))
        .unwrap();
    wal.write_all(&frame).unwrap();
    wal.sync_all().unwrap();
    drop(wal);
    drop(db);

    let db = Db::open(Config::new(d.clone())).unwrap();
    assert_eq!(db.get(b"k1").unwrap(), Some(b"v1".to_vec()));
    assert_eq!(
        db.put(b"k2", b"v2").unwrap(),
        2,
        "the replayed batch must not consume a sequence"
    );
    assert_eq!(db.get(b"k2").unwrap(), Some(b"v2".to_vec()));
}

#[test]
fn newer_memtable_shadows_flushed_segment() {
    let d = dir("flush-shadow");
    let db = Db::open(Config::new(d.clone())).unwrap();
    db.put(b"k1", b"v1").unwrap();
    db.flush().unwrap();
    assert_eq!(db.put(b"k1", b"v2").unwrap(), 2);
    assert_eq!(db.get(b"k1").unwrap(), Some(b"v2".to_vec()));
    drop(db);
    let db = Db::open(Config::new(d.clone())).unwrap();
    assert_eq!(db.get(b"k1").unwrap(), Some(b"v2".to_vec()));
}

#[test]
fn equal_key_versions_survive_flush() {
    // SE2-M14 — the kernel's RMW restatement rewrites the same head key
    // between flushes; the memtable keeps every (key, seq) version, so one
    // flush segment holds a 20-entry equal-key run (publish sorts seq
    // desc, and get's first match is the head). The v2 writer's restart
    // points must skip equal keys, or the first read of the flushed
    // segment fails closed (restart keys strictly increasing).
    let d = dir("flush-equal-keys");
    let mut cfg = Config::new(d.clone());
    cfg.memtable_bytes = 1 << 30; // no auto-flush — versions accumulate
    let db = Db::open(cfg).unwrap();
    for i in 0..20u64 {
        db.put(b"head/x", format!("v{i:02}").as_bytes()).unwrap();
    }
    db.flush().unwrap();
    assert_eq!(
        db.get(b"head/x").unwrap().as_deref(),
        Some(&b"v19"[..]),
        "the newest version is the head of the run"
    );
    drop(db);
    let db = Db::open(Config::new(d)).unwrap();
    assert_eq!(
        db.get(b"head/x").unwrap().as_deref(),
        Some(&b"v19"[..]),
        "cold reopen must survive the flushed segment's equal-key run"
    );
}
