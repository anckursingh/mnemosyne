//! SE2-M3 — bounded recovery (TESTING-PLAN-V2 row V2-M3): open reads only
//! the manifest + active WAL (historical segment data untouched), missing
//! segment fails closed, orphan segments are reported not fatal, and the
//! KSE-082B classifier holds at the Db boundary.

mod common;

use aikoql_storage_v2::db::{manifest_path, orphan_segments, segment_path, Config, Db, WAL_FILE};
use aikoql_storage_v2::format::{Current, FormatError, Manifest};
use aikoql_storage_v2::wal::{encode_frame, Op};
use common::dir;
use std::io::Write;

#[test]
fn reopen_touches_only_manifest_and_active_wal() {
    let d = dir("rec-touch");
    let db = Db::open(Config::new(d.clone())).unwrap();
    db.put(b"k1", b"v1").unwrap();
    db.put(b"k2", b"v2").unwrap();
    db.put(b"k3", b"v3").unwrap();
    db.flush().unwrap();
    db.put(b"k4", b"v4").unwrap();
    drop(db);

    // crash mid-append: garbage behind the last complete frame
    let mut wal = std::fs::OpenOptions::new()
        .append(true)
        .open(d.join(WAL_FILE))
        .unwrap();
    wal.write_all(&[0xde, 0xad, 0xbe]).unwrap();
    drop(wal);
    let seg_before = std::fs::read(segment_path(&d, 1)).unwrap();
    let manifest_before = std::fs::read(manifest_path(&d, 2)).unwrap();

    let db = Db::open(Config::new(d.clone())).unwrap();
    assert_eq!(db.get(b"k1").unwrap(), Some(b"v1".to_vec()));
    assert_eq!(db.get(b"k4").unwrap(), Some(b"v4".to_vec()));
    assert_eq!(
        std::fs::read(segment_path(&d, 1)).unwrap(),
        seg_before,
        "historical segment bytes must be untouched by reopen"
    );
    assert_eq!(
        std::fs::read(manifest_path(&d, 2)).unwrap(),
        manifest_before,
        "the manifest must be read, never rewritten, by reopen"
    );
    // the torn tail was truncated: the WAL holds exactly the one unflushed frame
    let frame = encode_frame(4, &[Op::Put(b"k4".to_vec(), b"v4".to_vec())]).unwrap();
    assert_eq!(
        std::fs::metadata(d.join(WAL_FILE)).unwrap().len() as usize,
        frame.len()
    );
}

#[test]
fn reopen_replays_only_the_active_wal() {
    let d = dir("rec-active");
    let db = Db::open(Config::new(d.clone())).unwrap();
    for i in 1..=3u64 {
        db.put(format!("k{i}").as_bytes(), format!("v{i}").as_bytes())
            .unwrap();
    }
    db.flush().unwrap();
    assert_eq!(std::fs::metadata(d.join(WAL_FILE)).unwrap().len(), 0);
    db.put(b"k4", b"v4").unwrap();
    // the WAL holds ONLY the unflushed batch — flushed data is not replayed
    let frame = encode_frame(4, &[Op::Put(b"k4".to_vec(), b"v4".to_vec())]).unwrap();
    assert_eq!(
        std::fs::metadata(d.join(WAL_FILE)).unwrap().len() as usize,
        frame.len()
    );
    drop(db);

    let db = Db::open(Config::new(d.clone())).unwrap();
    for i in 1..=4u64 {
        assert_eq!(
            db.get(format!("k{i}").as_bytes()).unwrap(),
            Some(format!("v{i}").into_bytes())
        );
    }
    assert_eq!(
        db.put(b"k5", b"v5").unwrap(),
        5,
        "replay must not consume a sequence"
    );
}

#[test]
fn missing_segment_fails_closed() {
    let d = dir("rec-missing");
    let db = Db::open(Config::new(d.clone())).unwrap();
    db.put(b"k1", b"v1").unwrap();
    db.flush().unwrap();
    drop(db);
    std::fs::remove_file(segment_path(&d, 1)).unwrap();
    let err = match Db::open(Config::new(d.clone())) {
        Err(e) => e,
        Ok(_) => panic!("reopen with a missing segment must fail closed"),
    };
    assert!(
        matches!(err, FormatError::Io(_)),
        "missing segment must fail closed: {err:?}"
    );
}

#[test]
fn orphan_segment_reported_not_fatal() {
    let d = dir("rec-orphan");
    let db = Db::open(Config::new(d.clone())).unwrap();
    db.put(b"k1", b"v1").unwrap();
    db.flush().unwrap();
    drop(db);
    // a crash between segment publication and manifest/CURRENT leaves an
    // unreferenced segment — reported, ignored, safe to overwrite later
    std::fs::copy(segment_path(&d, 1), segment_path(&d, 999)).unwrap();

    let current = Current::read(&d.join("CURRENT")).unwrap();
    let manifest = Manifest::read(&manifest_path(&d, current.manifest_generation)).unwrap();
    assert_eq!(orphan_segments(&d, &manifest), vec![999]);

    let db = Db::open(Config::new(d.clone())).unwrap();
    assert_eq!(db.get(b"k1").unwrap(), Some(b"v1".to_vec()));
    assert_eq!(
        db.put(b"k2", b"v2").unwrap(),
        2,
        "an orphan must not consume ids or state"
    );
    db.flush().unwrap();
    assert_eq!(db.get(b"k2").unwrap(), Some(b"v2".to_vec()));
    drop(db);
    let db = Db::open(Config::new(d.clone())).unwrap();
    assert_eq!(db.get(b"k1").unwrap(), Some(b"v1".to_vec()));
    assert_eq!(db.get(b"k2").unwrap(), Some(b"v2".to_vec()));
}

#[test]
fn corrupt_manifest_fails_closed() {
    let d = dir("rec-manifest");
    let db = Db::open(Config::new(d.clone())).unwrap();
    db.put(b"k1", b"v1").unwrap();
    db.flush().unwrap();
    drop(db);
    let mut bytes = std::fs::read(manifest_path(&d, 2)).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x01;
    std::fs::write(manifest_path(&d, 2), &bytes).unwrap();
    let err = match Db::open(Config::new(d.clone())) {
        Err(e) => e,
        Ok(_) => panic!("reopen with a corrupt manifest must fail closed"),
    };
    assert!(
        matches!(err, FormatError::Corrupt(_)),
        "corrupt manifest must fail closed: {err:?}"
    );
}

#[test]
fn corrupt_wal_with_valid_tail_fails_closed() {
    // KSE-082B verbatim at the Db boundary: damage followed by a valid
    // frame is middle corruption, not a crash tail to truncate.
    let d = dir("rec-082b");
    let db = Db::open(Config::new(d.clone())).unwrap();
    db.put(b"k1", b"v1").unwrap();
    db.put(b"k2", b"v2").unwrap();
    drop(db);
    let mut bytes = std::fs::read(d.join(WAL_FILE)).unwrap();
    bytes[23] ^= 0x01; // first frame's op byte (19-byte header + u32 entry count)
    std::fs::write(d.join(WAL_FILE), &bytes).unwrap();
    let err = match Db::open(Config::new(d.clone())) {
        Err(e) => e,
        Ok(_) => panic!("middle damage + a valid tail must fail closed"),
    };
    assert!(
        matches!(err, FormatError::Corrupt(_)),
        "middle damage + valid tail must fail closed: {err:?}"
    );
}
