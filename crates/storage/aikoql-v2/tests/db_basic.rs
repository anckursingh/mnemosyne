//! SE2-M2 — Db basics: put/get/delete, per-batch sequences, WAL replay on
//! reopen (TESTING-PLAN-V2 row V2-M2).

mod common;

use aikoql_storage_v2::db::{Config, Db};
use aikoql_storage_v2::wal::Op;
use common::dir;
use std::path::PathBuf;

fn open(tag: &str) -> (Db, PathBuf) {
    let d = dir(tag);
    let db = Db::open(Config::new(d.clone())).unwrap();
    (db, d)
}

#[test]
fn put_get_visible() {
    let (db, _d) = open("basic-put");
    assert_eq!(db.put(b"k1", b"v1").unwrap(), 1);
    assert_eq!(db.get(b"k1").unwrap(), Some(b"v1".to_vec()));
    assert_eq!(db.get(b"missing").unwrap(), None);
}

#[test]
fn overwrite_returns_head() {
    let (db, _d) = open("basic-overwrite");
    db.put(b"k1", b"v1").unwrap();
    assert_eq!(db.put(b"k1", b"v2").unwrap(), 2);
    assert_eq!(db.get(b"k1").unwrap(), Some(b"v2".to_vec()));
}

#[test]
fn delete_hides_key() {
    let (db, _d) = open("basic-delete");
    db.put(b"k1", b"v1").unwrap();
    assert_eq!(db.delete(b"k1").unwrap(), 2);
    assert_eq!(db.get(b"k1").unwrap(), None);
    // deleting a key that never existed is a valid tombstone
    assert_eq!(db.delete(b"ghost").unwrap(), 3);
    assert_eq!(db.get(b"ghost").unwrap(), None);
}

#[test]
fn seqs_are_per_batch() {
    let (db, _d) = open("basic-seq");
    assert_eq!(db.put(b"a", b"1").unwrap(), 1);
    assert_eq!(
        db.write(&[
            Op::Put(b"b".to_vec(), b"2".to_vec()),
            Op::Delete(b"c".to_vec()),
        ])
        .unwrap(),
        2,
        "one sequence per batch, not per op"
    );
    assert_eq!(db.put(b"d", b"3").unwrap(), 3);
    assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn reopen_recovers_unflushed_from_wal() {
    let d = dir("basic-reopen");
    {
        let db = Db::open(Config::new(d.clone())).unwrap();
        db.put(b"k1", b"v1").unwrap();
        db.put(b"k2", b"v2").unwrap();
        db.delete(b"k1").unwrap(); // tombstone must survive reopen
    } // Drop does NOT flush — recovery is the WAL's job
    let db = Db::open(Config::new(d.clone())).unwrap();
    assert_eq!(db.get(b"k1").unwrap(), None);
    assert_eq!(db.get(b"k2").unwrap(), Some(b"v2".to_vec()));
    assert_eq!(
        db.put(b"k3", b"v3").unwrap(),
        4,
        "next seq must follow the replayed batches"
    );
}
