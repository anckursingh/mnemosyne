//! V2-Adopt — `Db::scan`: the prefix scan the kernel's `StorageEngine`
//! contract needs (sorted ascending, prefix-restricted, one entry per key
//! = the newest layer's head; tombstones shadow — a deleted key does not
//! appear). Same layer order as `Db::get`: active → immutables → segments
//! (newest first).
//!
//! The kernel keyspace is prefix-schemed (`ko/<koid 16B>`, `head/`, …) and
//! koid bytes can be any byte value, so the scan must never rely on an
//! ASCII-style end bound like b"~" — the prefix bound is computed by byte
//! successor, and a prefix that overflows (all 0xFF) iterates unbounded.

mod common;

use aikoql_storage_v2::db::{Config, Db};
use common::dir;
use std::collections::BTreeMap;

fn expected(rows: &[(&str, &str)]) -> BTreeMap<Vec<u8>, Vec<u8>> {
    rows.iter()
        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect()
}

#[test]
fn scan_merges_layers_newest_wins() {
    let d = dir("scan-layers");
    let db = Db::open(Config::new(d.clone())).unwrap();

    // layer 1 (flushed): k1, k2, k3
    db.put(b"k1", b"v1").unwrap();
    db.put(b"k2", b"v2").unwrap();
    db.put(b"k3", b"v3").unwrap();
    db.flush().unwrap();
    // layer 2 (flushed): overwrite k1, delete k2, new k4
    db.put(b"k1", b"v1b").unwrap();
    db.delete(b"k2").unwrap();
    db.put(b"k4", b"v4").unwrap();
    db.flush().unwrap();
    // layer 3 (active): overwrite k1 again, tombstone k3, new k5
    db.put(b"k1", b"v1c").unwrap();
    db.delete(b"k3").unwrap();
    db.put(b"k5", b"v5").unwrap();

    let got: BTreeMap<Vec<u8>, Vec<u8>> = db.scan(b"").unwrap().into_iter().collect();
    assert_eq!(got, expected(&[("k1", "v1c"), ("k4", "v4"), ("k5", "v5")]));
    // every scanned answer agrees with get — no drift between the paths
    for (k, v) in &got {
        assert_eq!(db.get(k).unwrap(), Some(v.clone()), "get disagrees");
    }
    for k in [&b"k2"[..], &b"k3"[..]] {
        assert_eq!(db.get(k).unwrap(), None, "tombstone must shadow");
    }
}

#[test]
fn scan_prefix_bounds_and_sorted() {
    let d = dir("scan-prefix");
    let db = Db::open(Config::new(d.clone())).unwrap();
    db.put(b"a/1", b"1").unwrap();
    db.put(b"a/3", b"3").unwrap();
    db.put(b"a/2", b"2").unwrap();
    db.put(b"b/1", b"1").unwrap();
    // a key whose bytes exceed any ASCII bound — the no-successor shape
    // (koid bytes are arbitrary; a high-byte key must never fall out of a
    // full scan)
    let mut hi = b"ko/".to_vec();
    hi.extend_from_slice(&[0xFF; 16]);
    db.put(&hi, b"hi").unwrap();
    db.flush().unwrap();

    let a: Vec<Vec<u8>> = db
        .scan(b"a/")
        .unwrap()
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        a,
        vec![b"a/1".to_vec(), b"a/2".to_vec(), b"a/3".to_vec()],
        "prefix scan must be sorted ascending and prefix-restricted"
    );
    assert_eq!(db.scan(b"a/2").unwrap().len(), 1);
    assert!(db.scan(b"nope").unwrap().is_empty());
    // full scan covers the high-byte key too
    let all: Vec<Vec<u8>> = db.scan(b"").unwrap().into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        all,
        vec![
            b"a/1".to_vec(),
            b"a/2".to_vec(),
            b"a/3".to_vec(),
            b"b/1".to_vec(),
            hi.clone()
        ]
    );
}

#[test]
fn scan_collapses_versions_to_head_across_segments() {
    let d = dir("scan-versions");
    let db = Db::open(Config::new(d.clone())).unwrap();
    for i in 0..5 {
        db.put(b"hot", format!("v{i}").as_bytes()).unwrap();
        db.flush().unwrap(); // five versions across five segments
    }
    let got = db.scan(b"hot").unwrap();
    assert_eq!(got.len(), 1, "one entry per key — the head");
    assert_eq!(got[0].0, b"hot");
    assert_eq!(got[0].1, b"v4");
    // same head after reopen — segments + WAL replay agree
    drop(db);
    let db = Db::open(Config::new(d.clone())).unwrap();
    let got = db.scan(b"hot").unwrap();
    assert_eq!(got, vec![(b"hot".to_vec(), b"v4".to_vec())]);
}

#[test]
fn scan_survives_compact() {
    let d = dir("scan-compact");
    let db = Db::open(Config::new(d.clone())).unwrap();
    for i in 0..20 {
        db.put(format!("k{i:02}").as_bytes(), format!("v{i}").as_bytes())
            .unwrap();
        if i % 5 == 4 {
            db.flush().unwrap();
        }
    }
    db.delete(b"k07").unwrap();
    db.flush().unwrap();
    let before: BTreeMap<Vec<u8>, Vec<u8>> = db.scan(b"").unwrap().into_iter().collect();
    assert_eq!(before.len(), 19);
    db.compact().unwrap();
    let after: BTreeMap<Vec<u8>, Vec<u8>> = db.scan(b"").unwrap().into_iter().collect();
    assert_eq!(before, after, "compaction must not change scan answers");
}
