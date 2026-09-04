//! SE2-M3 — legacy v1 WAL migration (design §23, TESTING-PLAN-V2 V2-M3):
//! decode → build v2 segments → manifest → publish CURRENT → reopen →
//! verify → only then done. The source WAL is never modified or deleted
//! (the operator's retention policy decides; the migrator only reports).

mod common;

use aikoql_kernel::storage::store::{StorageEngine, WriteBatch};
use aikoql_storage::AikoqlStorageEngine;
use aikoql_storage_v2::db::{Config, Db};
use aikoql_storage_v2::format::FormatError;
use aikoql_storage_v2::migration::migrate_v1_wal;
use common::{dir, tmp};
use std::collections::BTreeMap;
use std::path::Path;

/// Write real v1 batches through the certified engine (the v1 WAL format
/// as produced in production, not a hand-rolled fixture).
fn write_v1(path: &Path, batches: &[WriteBatch]) {
    let engine = AikoqlStorageEngine::open(path).unwrap();
    for batch in batches {
        engine.write_batch(batch).unwrap();
    }
}

/// v1 apply semantics (puts before dels — the shared KSE-006 order).
fn expected(batches: &[WriteBatch]) -> BTreeMap<Vec<u8>, Option<Vec<u8>>> {
    let mut map = BTreeMap::new();
    for batch in batches {
        for (k, v) in &batch.puts {
            map.insert(k.clone(), Some(v.clone()));
        }
        for k in &batch.dels {
            map.insert(k.clone(), None);
        }
    }
    map
}

#[test]
fn migrate_v1_wal_moves_state_and_never_deletes_source() {
    let src = tmp("migrate-src");
    let mut b1 = WriteBatch::new();
    b1.put(b"k1".to_vec(), b"v1".to_vec());
    b1.put(b"k2".to_vec(), b"v2-old".to_vec());
    let mut b2 = WriteBatch::new();
    b2.del(b"k1".to_vec());
    b2.put(b"k2".to_vec(), b"v2-new".to_vec());
    b2.put(b"k3".to_vec(), b"v3".to_vec());
    let mut b3 = WriteBatch::new();
    b3.put(b"k4".to_vec(), b"v4".to_vec());
    let batches = [b1, b2, b3];
    write_v1(&src, &batches);
    let src_bytes = std::fs::read(&src).unwrap();

    // the source's own engine agrees on the final state (fixture cross-check)
    let v1 = AikoqlStorageEngine::open(&src).unwrap();
    let want = expected(&batches);
    for (k, v) in &want {
        assert_eq!(v1.get(k).unwrap(), v.clone(), "v1 fixture diverged");
    }

    let dest = dir("migrate-dest");
    let report = migrate_v1_wal(&src, Config::new(dest.clone())).unwrap();
    assert_eq!(report.batches, 3);
    assert_eq!(report.puts, 5);
    assert_eq!(report.deletes, 1);
    assert_eq!(report.keys, 4);
    assert!(!report.torn_tail_dropped);

    // never delete (or modify) the source — not even before verification
    assert_eq!(std::fs::read(&src).unwrap(), src_bytes);

    let db = Db::open(Config::new(dest.clone())).unwrap();
    for (k, v) in &want {
        assert_eq!(db.get(k).unwrap(), v.clone(), "migrated key diverged");
    }
    drop(db);
    let db = Db::open(Config::new(dest)).unwrap(); // state survives reopen
    for (k, v) in &want {
        assert_eq!(db.get(k).unwrap(), v.clone(), "reopened key diverged");
    }
}

#[test]
fn migrate_rejects_corrupt_source() {
    let src = tmp("migrate-corrupt");
    let mut b1 = WriteBatch::new();
    b1.put(b"k1".to_vec(), b"v1".to_vec());
    let mut b2 = WriteBatch::new();
    b2.put(b"k2".to_vec(), b"v2".to_vec());
    write_v1(&src, &[b1, b2]);
    let mut bytes = std::fs::read(&src).unwrap();
    bytes[15] ^= 0x01; // first record's payload (11-byte header + 4) → checksum mismatch
    std::fs::write(&src, &bytes).unwrap();

    let dest = dir("migrate-corrupt-dest");
    let err = match migrate_v1_wal(&src, Config::new(dest)) {
        Err(e) => e,
        Ok(_) => panic!("a corrupt source must fail closed"),
    };
    assert!(
        matches!(err, FormatError::Corrupt(_)),
        "corrupt source must fail closed: {err:?}"
    );
}

#[test]
fn migrate_stops_at_torn_tail() {
    let src = tmp("migrate-torn");
    let mut b1 = WriteBatch::new();
    b1.put(b"k1".to_vec(), b"v1".to_vec());
    let mut b2 = WriteBatch::new();
    b2.put(b"k2".to_vec(), b"v2".to_vec());
    write_v1(&src, &[b1, b2]);
    let bytes = std::fs::read(&src).unwrap();
    std::fs::write(&src, &bytes[..bytes.len() - 5]).unwrap(); // crash mid-append
    let src_after = std::fs::read(&src).unwrap();

    let dest = dir("migrate-torn-dest");
    let report = migrate_v1_wal(&src, Config::new(dest.clone())).unwrap();
    assert_eq!(report.batches, 1);
    assert!(report.torn_tail_dropped);
    // unlike v1's own open (which truncates the torn tail in place), the
    // migrator must leave the source byte-for-byte intact
    assert_eq!(std::fs::read(&src).unwrap(), src_after);

    let db = Db::open(Config::new(dest)).unwrap();
    assert_eq!(db.get(b"k1").unwrap(), Some(b"v1".to_vec()));
    assert_eq!(
        db.get(b"k2").unwrap(),
        None,
        "the torn batch was never acked"
    );
}

#[test]
fn migrate_empty_wal_creates_fresh_v2() {
    let src = tmp("migrate-empty");
    AikoqlStorageEngine::open(&src).unwrap(); // v1 open creates the (empty) WAL file

    let dest = dir("migrate-empty-dest");
    let report = migrate_v1_wal(&src, Config::new(dest.clone())).unwrap();
    assert_eq!(report.batches, 0);
    assert_eq!(report.keys, 0);

    let db = Db::open(Config::new(dest)).unwrap();
    assert_eq!(db.put(b"k1", b"v1").unwrap(), 1);
    assert_eq!(db.get(b"k1").unwrap(), Some(b"v1".to_vec()));
}
