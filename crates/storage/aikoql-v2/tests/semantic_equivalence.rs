//! SE2-M5 — the §25 Compaction Semantic Equivalence gate plus the
//! retention policy interface (KEEP/DROP/ARCHIVE per key class).
//!
//! Keys mirror the kernel's real row layout
//! (crates/kernel/src/storage/repository.rs): version rows
//! `ko/<koid 16B><commit_ts u64 BE>` are DISTINCT keys per (koid, ts),
//! heads are same-key overwrites, relationships are mirrored
//! relo/reli index rows. Newest-per-key compaction therefore preserves
//! history by construction — this milestone proves it against a
//! kernel-shaped workload, and adds the policy that lets a caller mark
//! genuinely-obsolete rows (the policy is an input, never an engine
//! feature — the engine stays key-space-generic).

mod common;

use aikoql_storage_v2::compaction::{KeepAll, Retention, RetentionPolicy};
use aikoql_storage_v2::db::{manifest_path, Config, Db};
use aikoql_storage_v2::format::{Current, Manifest};
use aikoql_storage_v2::segment::SegmentReader;
use aikoql_storage_v2::wal::Op;
use common::dir;
use std::collections::HashMap;
use std::path::Path;

const KOID_A: [u8; 16] = [0xAA; 16];
const KOID_B: [u8; 16] = [0xBB; 16];
const UPDATES: u64 = 100;

fn obj_key(koid: &[u8; 16], ts: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(3 + 16 + 8);
    v.extend_from_slice(b"ko/");
    v.extend_from_slice(koid);
    v.extend_from_slice(&ts.to_be_bytes());
    v
}

fn head_key(koid: &[u8; 16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + 16);
    v.extend_from_slice(b"head/");
    v.extend_from_slice(koid);
    v
}

fn type_key(name: &str, koid: &[u8; 16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + name.len() + 1 + 16);
    v.extend_from_slice(b"type/");
    v.extend_from_slice(name.as_bytes());
    v.push(b'/');
    v.extend_from_slice(koid);
    v
}

fn rel_out_key(src: &[u8; 16], rel: &str, dst: &[u8; 16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + 16 + 1 + rel.len() + 1 + 16);
    v.extend_from_slice(b"relo/");
    v.extend_from_slice(src);
    v.push(b'/');
    v.extend_from_slice(rel.as_bytes());
    v.push(b'/');
    v.extend_from_slice(dst);
    v
}

fn rel_in_key(dst: &[u8; 16], rel: &str, src: &[u8; 16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + 16 + 1 + rel.len() + 1 + 16);
    v.extend_from_slice(b"reli/");
    v.extend_from_slice(dst);
    v.push(b'/');
    v.extend_from_slice(rel.as_bytes());
    v.push(b'/');
    v.extend_from_slice(src);
    v
}

fn version_value(i: u64) -> Vec<u8> {
    format!("a-version-{i:03}").into_bytes()
}

fn head_value(i: u64) -> Vec<u8> {
    format!("a-head-{i:03}").into_bytes()
}

/// The oracle: every key ever written → its current expected value.
struct Oracle {
    live: HashMap<Vec<u8>, Vec<u8>>,
    dead: Vec<Vec<u8>>,
}

impl Oracle {
    fn put(&mut self, k: Vec<u8>, v: Vec<u8>) {
        self.live.insert(k, v);
    }
    fn delete(&mut self, k: Vec<u8>) {
        self.live.remove(&k);
        self.dead.push(k);
    }
    fn verify(&self, db: &Db) {
        for (k, want) in &self.live {
            assert_eq!(
                db.get(k).unwrap(),
                Some(want.clone()),
                "live key {:?} diverged",
                String::from_utf8_lossy(k)
            );
        }
        for k in &self.dead {
            assert_eq!(
                db.get(k).unwrap(),
                None,
                "deleted key {:?} resurrected",
                String::from_utf8_lossy(k)
            );
        }
    }
}

/// The §25 gate workload: create KO A → update ×100 → create KO B →
/// supersede → relate → unrelate, compacting (KeepAll) at every stage.
fn build_gate(d: &Path) -> (Db, Oracle) {
    let mut cfg = Config::new(d.to_path_buf());
    cfg.memtable_bytes = 1024; // interleaved auto-flushes — real merges
    let db = Db::open(cfg).unwrap();
    let mut oracle = Oracle {
        live: HashMap::new(),
        dead: Vec::new(),
    };

    // Create KO A: version row 1 + head + type index (one atomic batch).
    db.write(&[
        Op::Put(obj_key(&KOID_A, 1), version_value(1)),
        Op::Put(head_key(&KOID_A), head_value(1)),
        Op::Put(type_key("Note", &KOID_A), b"note-a".to_vec()),
    ])
    .unwrap();
    oracle.put(obj_key(&KOID_A, 1), version_value(1));
    oracle.put(head_key(&KOID_A), head_value(1));
    oracle.put(type_key("Note", &KOID_A), b"note-a".to_vec());

    // Update ×99: one new version row per update, head moves to it.
    // Version rows are distinct keys — an old version is never an
    // overwrite, so the newest-per-key merge must keep all 100.
    for i in 2..=UPDATES {
        db.write(&[
            Op::Put(obj_key(&KOID_A, i), version_value(i)),
            Op::Put(head_key(&KOID_A), head_value(i)),
        ])
        .unwrap();
        oracle.put(obj_key(&KOID_A, i), version_value(i));
        oracle.put(head_key(&KOID_A), head_value(i));
        if i % 20 == 0 {
            db.compact_with(&KeepAll).unwrap();
            oracle.verify(&db); // temporal queries after every compaction
        }
    }

    // Create KO B.
    db.write(&[
        Op::Put(obj_key(&KOID_B, 1), b"b-version-001".to_vec()),
        Op::Put(head_key(&KOID_B), b"b-head-001".to_vec()),
        Op::Put(type_key("Note", &KOID_B), b"note-b".to_vec()),
    ])
    .unwrap();
    oracle.put(obj_key(&KOID_B, 1), b"b-version-001".to_vec());
    oracle.put(head_key(&KOID_B), b"b-head-001".to_vec());
    oracle.put(type_key("Note", &KOID_B), b"note-b".to_vec());

    // Supersede: B supersedes A — a mirrored rel pair.
    db.write(&[
        Op::Put(rel_out_key(&KOID_A, "supersedes", &KOID_B), b"1".to_vec()),
        Op::Put(rel_in_key(&KOID_B, "supersedes", &KOID_A), b"1".to_vec()),
    ])
    .unwrap();
    oracle.put(rel_out_key(&KOID_A, "supersedes", &KOID_B), b"1".to_vec());
    oracle.put(rel_in_key(&KOID_B, "supersedes", &KOID_A), b"1".to_vec());
    db.compact_with(&KeepAll).unwrap();
    oracle.verify(&db);

    // Relate, then unrelate: the delete tombstones must hold through
    // compaction — no resurrection of the removed relationship.
    let ro = rel_out_key(&KOID_A, "related", &KOID_B);
    let ri = rel_in_key(&KOID_B, "related", &KOID_A);
    db.write(&[
        Op::Put(ro.clone(), b"1".to_vec()),
        Op::Put(ri.clone(), b"1".to_vec()),
    ])
    .unwrap();
    oracle.put(ro.clone(), b"1".to_vec());
    oracle.put(ri.clone(), b"1".to_vec());
    db.write(&[Op::Delete(ro.clone()), Op::Delete(ri.clone())])
        .unwrap();
    oracle.delete(ro);
    oracle.delete(ri);
    // Canonical end state for the gate: flush the memtable and compact so
    // exactly one L1 segment holds exactly the live keys (the pins the
    // callers make on top).
    db.flush().unwrap();
    db.compact_with(&KeepAll).unwrap();
    oracle.verify(&db);
    (db, oracle)
}

#[test]
fn semantic_equivalence_keep_all() {
    let d = dir("sem-eq-keep");
    let (db, oracle) = build_gate(&d);
    oracle.verify(&db);

    // Head preserved: A's head is the 100th update's head; B's untouched.
    assert_eq!(
        db.get(&head_key(&KOID_A)).unwrap(),
        Some(head_value(UPDATES))
    );
    assert_eq!(
        db.get(&head_key(&KOID_B)).unwrap(),
        Some(b"b-head-001".to_vec())
    );

    // Temporal soundness: every one of the 100 version rows survives.
    for i in 1..=UPDATES {
        assert_eq!(
            db.get(&obj_key(&KOID_A, i)).unwrap(),
            Some(version_value(i)),
            "version row {i} lost"
        );
    }

    // Supersede lineage preserved; unrelate held (mirrored pair absent).
    assert_eq!(
        db.get(&rel_out_key(&KOID_A, "supersedes", &KOID_B))
            .unwrap(),
        Some(b"1".to_vec())
    );
    assert_eq!(
        db.get(&rel_in_key(&KOID_B, "supersedes", &KOID_A)).unwrap(),
        Some(b"1".to_vec())
    );
    assert_eq!(
        db.get(&rel_out_key(&KOID_A, "related", &KOID_B)).unwrap(),
        None
    );

    // Compaction idempotence: a second merge changes nothing.
    let stats = db.compact_with(&KeepAll).unwrap();
    assert_eq!(
        stats.entries_in, stats.entries_out,
        "recompaction must be a fixed point"
    );
    assert_eq!(stats.entries_archived, 0);
    let live = oracle.live.len() as u64;
    assert_eq!(stats.entries_out, live, "L1 holds exactly the live keys");

    // Durability: the compacted state alone serves everything on reopen.
    drop(db);
    let db = Db::open(Config::new(d.clone())).unwrap();
    oracle.verify(&db);
    for i in 1..=UPDATES {
        assert_eq!(
            db.get(&obj_key(&KOID_A, i)).unwrap(),
            Some(version_value(i))
        );
    }
    let current = Current::read(&d.join("CURRENT")).unwrap();
    let manifest = Manifest::read(&manifest_path(&d, current.manifest_generation)).unwrap();
    assert_eq!(manifest.segments.len(), 1);
    assert_eq!(manifest.segments[0].level, 1);
    assert_eq!(manifest.segments[0].record_count, live);
}

/// DROP exactly the marked class: A's version rows — nothing else.
struct DropObjA;

impl RetentionPolicy for DropObjA {
    fn classify(&self, key: &[u8]) -> Retention {
        let mut prefix = b"ko/".to_vec();
        prefix.extend_from_slice(&KOID_A);
        if key.starts_with(&prefix) {
            Retention::Drop
        } else {
            Retention::Keep
        }
    }
}

#[test]
fn retention_drop_removes_only_marked_keys() {
    let d = dir("sem-eq-drop");
    let (db, oracle) = build_gate(&d);
    let stats = db.compact_with(&DropObjA).unwrap();
    assert_eq!(stats.entries_archived, 0);
    assert_eq!(
        stats.entries_in - stats.entries_out,
        UPDATES,
        "the merge dropped exactly A's 100 version rows"
    );

    // A's version rows are gone — temporal queries for them return None
    // (the caller asserted they were genuinely obsolete).
    for i in 1..=UPDATES {
        assert_eq!(db.get(&obj_key(&KOID_A, i)).unwrap(), None);
    }
    // Everything not marked survives: heads, type indexes, B's version,
    // the supersede lineage, the mirror invariants.
    assert_eq!(
        db.get(&head_key(&KOID_A)).unwrap(),
        Some(head_value(UPDATES))
    );
    assert_eq!(
        db.get(&obj_key(&KOID_B, 1)).unwrap(),
        Some(b"b-version-001".to_vec())
    );
    assert_eq!(
        db.get(&rel_out_key(&KOID_A, "supersedes", &KOID_B))
            .unwrap(),
        Some(b"1".to_vec())
    );
    assert_eq!(
        db.get(&type_key("Note", &KOID_A)).unwrap(),
        Some(b"note-a".to_vec())
    );
    assert_eq!(
        db.get(&rel_out_key(&KOID_A, "related", &KOID_B)).unwrap(),
        None,
        "the unrelate tombstone must still hold"
    );

    drop(db);
    let db = Db::open(Config::new(d)).unwrap();
    for i in 1..=UPDATES {
        assert_eq!(db.get(&obj_key(&KOID_A, i)).unwrap(), None);
    }
    assert_eq!(
        db.get(&head_key(&KOID_A)).unwrap(),
        Some(head_value(UPDATES))
    );
    let _ = oracle; // DROP is caller-asserted obsolescence — the kept
                    // subset was verified above; the dropped subset is
                    // verified absent.
}

/// ARCHIVE the marked class: rows leave the live database but remain
/// readable from the archive segment, byte-exact.
struct ArchiveObjA;

impl RetentionPolicy for ArchiveObjA {
    fn classify(&self, key: &[u8]) -> Retention {
        let mut prefix = b"ko/".to_vec();
        prefix.extend_from_slice(&KOID_A);
        if key.starts_with(&prefix) {
            Retention::Archive
        } else {
            Retention::Keep
        }
    }
}

fn archive_segments(d: &Path) -> Vec<std::path::PathBuf> {
    let archive = d.join("archive");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&archive)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("ARCHIVE-"))
        .map(|e| e.path())
        .collect();
    files.sort();
    files
}

#[test]
fn retention_archive_preserves_dropped_rows() {
    let d = dir("sem-eq-archive");
    let (db, _oracle) = build_gate(&d);
    let stats = db.compact_with(&ArchiveObjA).unwrap();
    assert_eq!(stats.entries_archived, UPDATES, "all 100 rows archived");

    // Absent from the live database, exactly like DROP.
    for i in 1..=UPDATES {
        assert_eq!(db.get(&obj_key(&KOID_A, i)).unwrap(), None);
    }
    assert_eq!(
        db.get(&head_key(&KOID_A)).unwrap(),
        Some(head_value(UPDATES))
    );

    // The archive holds every archived row, byte-exact and readable.
    let files = archive_segments(&d);
    assert_eq!(files.len(), 1, "one archive segment per compaction");
    let reader = SegmentReader::open(&files[0]).unwrap();
    assert_eq!(reader.entry_count(), UPDATES);
    for i in 1..=UPDATES {
        let e = reader
            .get(&obj_key(&KOID_A, i))
            .unwrap()
            .expect("archived version row missing");
        assert_eq!(e.value, version_value(i), "archived row {i} diverged");
    }
    // Nothing else was archived.
    assert_eq!(reader.get(&head_key(&KOID_A)).unwrap(), None);

    // Reopen: archive files are never consulted by the live database.
    drop(db);
    let db = Db::open(Config::new(d)).unwrap();
    for i in 1..=UPDATES {
        assert_eq!(db.get(&obj_key(&KOID_A, i)).unwrap(), None);
    }
    assert_eq!(
        db.get(&head_key(&KOID_A)).unwrap(),
        Some(head_value(UPDATES))
    );
}

#[test]
fn tombstone_never_resurrects() {
    let d = dir("sem-eq-tomb");
    let db = Db::open(Config::new(d.clone())).unwrap();
    db.put(b"k", b"v").unwrap();
    db.flush().unwrap();
    db.compact().unwrap(); // L1: k=v
    assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));

    db.delete(b"k").unwrap();
    db.flush().unwrap(); // L0 tombstone shadows L1
    assert_eq!(db.get(b"k").unwrap(), None);

    // The tombstone is retained until safe: it drops exactly together
    // with the older value it shadows — never before, so the old value
    // can never come back.
    let stats = db.compact_with(&KeepAll).unwrap();
    assert_eq!(stats.entries_in, 2, "value + tombstone both merge");
    assert_eq!(stats.entries_out, 0, "nothing remains to shadow");
    assert_eq!(db.get(b"k").unwrap(), None);
    drop(db);
    let db = Db::open(Config::new(d.clone())).unwrap();
    assert_eq!(db.get(b"k").unwrap(), None);
    // The L1 set is empty — the deletion is total, nothing below to
    // resurrect it.
    let current = Current::read(&d.join("CURRENT")).unwrap();
    let manifest = Manifest::read(&manifest_path(&d, current.manifest_generation)).unwrap();
    assert!(manifest.segments.is_empty(), "no L1 remains after the drop");
}
