//! SE2-M4 — compaction REDs: logical state before == after (byte-exact,
//! seeded workload); tombstone resolution at the bottom level; overwrites
//! collapse to the newest across and within levels; readers continue
//! during compaction; obsolete segments survive while referenced (Windows
//! delete-pending keeps an open reader's data alive).

mod common;

use aikoql_storage_v2::db::{manifest_path, segment_path, Config, Db};
use aikoql_storage_v2::format::{Current, Manifest};
use aikoql_storage_v2::segment::SegmentReader;
use common::dir;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// xorshift64 — deterministic workload, no dev-dependency.
fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut s = seed;
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    }
}

fn live_count(oracle: &HashMap<Vec<u8>, Option<Vec<u8>>>) -> u64 {
    oracle.values().filter(|v| v.is_some()).count() as u64
}

fn verify_oracle(db: &Db, oracle: &HashMap<Vec<u8>, Option<Vec<u8>>>) {
    for (k, want) in oracle {
        assert_eq!(
            db.get(k).unwrap(),
            *want,
            "key {:?} diverged",
            String::from_utf8_lossy(k)
        );
    }
    for probe in 0..50u64 {
        let k = format!("never{probe:03}").into_bytes();
        assert_eq!(db.get(&k).unwrap(), None, "phantom key {probe}");
    }
}

#[test]
fn logical_state_survives_compaction() {
    let d = dir("compact-equiv");
    let mut cfg = Config::new(d.clone());
    cfg.memtable_bytes = 512; // many flushes → several L0 segments
    cfg.l0_compact_trigger = 0; // SE2-M10: this test pins MANUAL compaction
    let db = Db::open(cfg).unwrap();
    let mut next = rng(7);
    let mut oracle: HashMap<Vec<u8>, Option<Vec<u8>>> = HashMap::new();
    for i in 0..400u64 {
        let k = format!("k{:04}", next() % 120).into_bytes();
        match next() % 10 {
            0..=6 => {
                let v = format!("v{i:04}").into_bytes();
                db.put(&k, &v).unwrap();
                oracle.insert(k, Some(v));
            }
            7..=8 => {
                db.delete(&k).unwrap();
                oracle.insert(k, None);
            }
            _ => {
                let want = oracle.get(&k).cloned().flatten();
                assert_eq!(
                    db.get(&k).unwrap(),
                    want,
                    "pre-compaction divergence at op {i}"
                );
            }
        }
    }
    db.flush().unwrap();
    let live = live_count(&oracle);
    let stats = db.compact().unwrap();
    assert_eq!(
        stats.entries_out, live,
        "L1 must hold exactly the live keys"
    );
    assert!(
        stats.entries_in >= stats.entries_out,
        "merge must not invent entries"
    );
    assert!(
        stats.segments_in >= 2,
        "the workload must produce several L0 segments"
    );
    assert_eq!(stats.segments_out, 1);
    verify_oracle(&db, &oracle);

    // Manifest pin: one L1 segment holding exactly the live keys.
    let current = Current::read(&d.join("CURRENT")).unwrap();
    let manifest = Manifest::read(&manifest_path(&d, current.manifest_generation)).unwrap();
    assert_eq!(
        manifest.segments.len(),
        1,
        "compaction output is one segment"
    );
    let rec = &manifest.segments[0];
    assert_eq!(rec.level, 1, "compaction output is L1");
    assert_eq!(rec.record_count, live, "L1 record count == live keys");

    // Durability across reopen: L1 alone serves everything.
    drop(db);
    let db = Db::open(Config::new(d)).unwrap();
    verify_oracle(&db, &oracle);
}

#[test]
fn tombstone_at_bottom_drops_the_key() {
    let d = dir("compact-tomb");
    let db = Db::open(Config::new(d.clone())).unwrap();
    db.put(b"k", b"v1").unwrap();
    db.flush().unwrap();
    db.compact().unwrap(); // L1: k=v1
    assert_eq!(db.get(b"k").unwrap(), Some(b"v1".to_vec()));

    db.put(b"k", b"v2").unwrap();
    db.flush().unwrap(); // L0: k=v2
    db.delete(b"k").unwrap();
    db.flush().unwrap(); // L0: k tombstone (newest)
    assert_eq!(
        db.get(b"k").unwrap(),
        None,
        "the tombstone shadows before compaction"
    );
    let stats = db.compact().unwrap();
    assert_eq!(
        stats.entries_out, 0,
        "the tombstone drops the key at the bottom level"
    );
    assert_eq!(
        stats.segments_out, 0,
        "an all-tombstone merge publishes no segment"
    );
    assert_eq!(db.get(b"k").unwrap(), None);
    drop(db);
    let db = Db::open(Config::new(d)).unwrap();
    assert_eq!(
        db.get(b"k").unwrap(),
        None,
        "a dropped key stays dropped across reopen"
    );
}

#[test]
fn cross_level_overwrites_collapse_to_newest() {
    let d = dir("compact-collapse");
    let db = Db::open(Config::new(d.clone())).unwrap();
    db.put(b"k", b"old").unwrap();
    db.flush().unwrap();
    db.compact().unwrap(); // L1: old
    db.put(b"k", b"new").unwrap();
    db.flush().unwrap(); // L0: new shadows L1
    assert_eq!(db.get(b"k").unwrap(), Some(b"new".to_vec()));
    let stats = db.compact().unwrap();
    assert_eq!(stats.entries_in, 2, "old and new both merge");
    assert_eq!(stats.entries_out, 1, "only the newest survives");
    assert_eq!(db.get(b"k").unwrap(), Some(b"new".to_vec()));
    drop(db);
    let db = Db::open(Config::new(d)).unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(b"new".to_vec()));
}

#[test]
fn same_level_overwrites_collapse_to_newest() {
    let d = dir("compact-collapse-l0");
    let db = Db::open(Config::new(d.clone())).unwrap();
    db.put(b"k", b"v1").unwrap();
    db.flush().unwrap();
    db.put(b"k", b"v2").unwrap();
    db.flush().unwrap();
    db.put(b"k", b"v3").unwrap();
    db.flush().unwrap();
    let stats = db.compact().unwrap();
    assert_eq!(stats.segments_in, 3);
    assert_eq!(stats.entries_in, 3);
    assert_eq!(stats.entries_out, 1);
    assert_eq!(db.get(b"k").unwrap(), Some(b"v3".to_vec()));
    drop(db);
    let db = Db::open(Config::new(d)).unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(b"v3".to_vec()));
}

#[test]
fn compact_is_a_noop_without_segments() {
    let d = dir("compact-noop");
    let db = Db::open(Config::new(d.clone())).unwrap();
    let stats = db.compact().unwrap();
    assert_eq!(
        stats.segments_in + stats.segments_out + stats.entries_in + stats.entries_out,
        0
    );
    db.put(b"k", b"v").unwrap(); // memtable only — not compaction material
    let stats = db.compact().unwrap();
    assert_eq!(
        stats.segments_in + stats.segments_out + stats.entries_in + stats.entries_out,
        0
    );
    assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
}

#[test]
fn readers_continue_and_obsolete_survive_while_referenced() {
    let d = dir("compact-readers");
    let mut cfg = Config::new(d.clone());
    cfg.memtable_bytes = 512;
    cfg.l0_compact_trigger = 0; // SE2-M10: segment 1 must survive to open
    let db = Db::open(cfg).unwrap();
    let mut next = rng(11);
    let mut oracle: HashMap<Vec<u8>, Option<Vec<u8>>> = HashMap::new();
    for i in 0..200u64 {
        let k = format!("k{:03}", next() % 60).into_bytes();
        let v = format!("v{i:03}").into_bytes();
        db.put(&k, &v).unwrap();
        oracle.insert(k, Some(v));
    }
    db.flush().unwrap();
    // Hold a reader on a segment compaction will obsolete: the file is
    // removed from the directory (delete-pending on Windows) but the open
    // handle keeps every entry readable.
    let first = segment_path(&d, 1);
    let old_reader = SegmentReader::open(&first).expect("segment 1 exists pre-compaction");

    // A reader hammering gets while the main thread compacts must only
    // ever observe the logical state — compaction is state-preserving, so
    // every answer equals the oracle (a torn segment swap would break it).
    let shared = Arc::new(RwLock::new(db));
    let reader_thread = {
        let db = Arc::clone(&shared);
        let oracle = oracle.clone();
        std::thread::spawn(move || {
            let mut next = rng(99);
            for _ in 0..20_000 {
                let k = format!("k{:03}", next() % 60).into_bytes();
                let want = oracle.get(&k).cloned().flatten();
                let got = db.read().unwrap().get(&k).unwrap();
                assert_eq!(got, want, "reader saw a torn state");
                std::thread::sleep(Duration::from_micros(50));
            }
        })
    };
    shared.write().unwrap().compact().unwrap();
    reader_thread.join().unwrap();

    assert!(!first.exists(), "obsolete segment file must be removed");
    let k = old_reader.key_min().to_vec();
    let e = old_reader
        .get(&k)
        .unwrap()
        .expect("a referenced reader keeps serving the deleted segment");
    assert!(!e.value.is_empty());
    verify_oracle(&shared.read().unwrap(), &oracle);
}
