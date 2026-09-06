//! SE2-M34 — flush identity REDs (spec §36 milestone 6, §17/§18/§23/§24):
//! the flush preserves stable identity end to end. The memtable's entries
//! become a v3 segment whose blocks carry `replica_id u64` per entry
//! (FL-001); each flushed replica resolves to a Segment placement naming
//! its anchor — the max-seq entry's (block, offset) (FL-002); after the
//! publication the old Memtable placement is not authoritative (FL-003);
//! a crash before the flush publication leaves the old state governing —
//! the WAL replays the memtable and its Memtable placements (FL-004, the
//! §24 state-B window, child-kill harness). SE2-M35 lifted the compact
//! guard this file pinned in M34: with Segment placements present,
//! compaction relocates instead of refusing (FL-005).

mod common;

use aikoql_storage_v2::db::{Config, Db};
use aikoql_storage_v2::identity::directory::{IdentityResolver, LocalIdentityDirectory};
use aikoql_storage_v2::identity::topology::{LocalReplicaDirectory, ReplicaDirectory};
use aikoql_storage_v2::identity::{ObjectId, ReplicaId};
use aikoql_storage_v2::placement::directory::{
    LocalPlacementResolver, Placement, PlacementResolver,
};
use aikoql_storage_v2::segment::{segment_path, SegmentReader};
use common::dir;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn oid(byte: u8) -> ObjectId {
    ObjectId([byte; 16])
}

fn rid_of(db: &Db, a: ObjectId) -> ReplicaId {
    let lid = LocalIdentityDirectory::new(db).resolve(a).unwrap().unwrap();
    LocalReplicaDirectory::new(db)
        .resolve_local(lid)
        .unwrap()
        .unwrap()
}

// ---------------------------------------------------------------------------
// Child-kill harness (KSE-15 pattern, the M4 shape)
// ---------------------------------------------------------------------------

const CHILD_ENV: &str = "AIKOQL_V2_KILL_CHILD";
const DIR_ENV: &str = "AIKOQL_V2_KILL_DIR";
const STAGE_ENV: &str = "AIKOQL_V2_FLUSH_PARK";

fn child_dir() -> PathBuf {
    PathBuf::from(std::env::var(DIR_ENV).expect("child dir env"))
}

fn spawn_child(test_name: &str, dir: &std::path::Path, stage: &str) -> Child {
    Command::new(std::env::current_exe().expect("current exe"))
        .arg("--exact")
        .arg(test_name)
        .env(CHILD_ENV, "1")
        .env(DIR_ENV, dir)
        .env(STAGE_ENV, stage)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child")
}

fn wait_for(path: &std::path::Path, timeout: Duration) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < timeout,
            "marker {} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// The FL REDs
// ---------------------------------------------------------------------------

#[test]
fn fl001_memtable_to_segment_preserves_replica_id() {
    let d = dir("fl001");
    let db = Db::open(Config::new(d.clone())).unwrap();
    let a = oid(0xA1);
    let b = oid(0xB2);
    db.put_object(a, b"ka1", b"va1").unwrap();
    db.put_object(a, b"ka2", b"va2").unwrap();
    db.put_object(b, b"kb1", b"vb1").unwrap();
    let rid_a = rid_of(&db, a);
    let rid_b = rid_of(&db, b);
    db.flush().unwrap();

    // §17 — the id relationship survives memtable → immutable → segment.
    // The flushed segment's entries carry their replica ids in the v3
    // block; nothing in the object's data was written as a byte-API row.
    let loc = match LocalPlacementResolver::new(&db).resolve(rid_a).unwrap() {
        Some(Placement::Segment(loc)) => loc,
        other => panic!("expected a Segment placement after flush, got {other:?}"),
    };
    let reader = SegmentReader::open(&segment_path(&d, loc.segment_id.0)).unwrap();
    let entries = reader.scan(b"", b"\xff").unwrap();
    let a_rows: Vec<_> = entries.iter().filter(|e| e.replica_id == rid_a).collect();
    assert_eq!(a_rows.len(), 2, "object a's two keys both carry rid_a");
    for e in &a_rows {
        assert!(
            e.key == b"ka1".to_vec() || e.key == b"ka2".to_vec(),
            "object a owns exactly ka1/ka2"
        );
    }
    let b_rows: Vec<_> = entries.iter().filter(|e| e.replica_id == rid_b).collect();
    assert_eq!(b_rows.len(), 1, "object b's key carries rid_b");
    assert_eq!(b_rows[0].key, b"kb1".to_vec());
    assert!(
        a_rows.iter().all(|e| e.replica_id != b_rows[0].replica_id),
        "distinct objects never share a replica id"
    );
}

#[test]
fn fl002_physical_location_resolvable_after_flush() {
    let d = dir("fl002");
    let db = Db::open(Config::new(d.clone())).unwrap();
    let a = oid(0xA1);
    db.put_object(a, b"k-low", b"v-low").unwrap();
    db.put_object(a, b"k-high", b"v-high").unwrap(); // newer seq → the anchor
    let rid = rid_of(&db, a);
    db.flush().unwrap();

    // The placement resolves to the anchor: the object's max-seq entry
    // location, and reading that entry back yields exactly the object's
    // newest write with its rid attached.
    let loc = match LocalPlacementResolver::new(&db).resolve(rid).unwrap() {
        Some(Placement::Segment(loc)) => loc,
        other => panic!("expected a Segment placement after flush, got {other:?}"),
    };
    let entry = SegmentReader::open(&segment_path(&d, loc.segment_id.0))
        .unwrap()
        .entry_at(loc.block_id, loc.entry_offset)
        .unwrap()
        .expect("the anchor names an existing entry");
    assert_eq!(entry.key, b"k-high".to_vec(), "anchor = the max-seq entry");
    assert_eq!(entry.value, b"v-high".to_vec());
    assert_eq!(entry.replica_id, rid, "the anchor entry carries its rid");
}

#[test]
fn fl003_memtable_placement_not_authoritative_after_flush() {
    let d = dir("fl003");
    let a = oid(0xA1);
    let db = Db::open(Config::new(d.clone())).unwrap();
    db.put_object(a, b"k", b"v1").unwrap();
    let rid = rid_of(&db, a);
    assert!(
        matches!(
            LocalPlacementResolver::new(&db).resolve(rid).unwrap(),
            Some(Placement::Memtable { .. })
        ),
        "§14 — the placement starts Memtable"
    );
    db.flush().unwrap();
    assert!(
        matches!(
            LocalPlacementResolver::new(&db).resolve(rid).unwrap(),
            Some(Placement::Segment(_))
        ),
        "after the flush publication the Memtable placement is not authoritative"
    );
    // PL-003 — the Segment placement survives the restart (it rode the
    // PLACEMENT log in the publication window).
    drop(db);
    let db = Db::open(Config::new(d)).unwrap();
    assert!(
        matches!(
            LocalPlacementResolver::new(&db).resolve(rid).unwrap(),
            Some(Placement::Segment(_))
        ),
        "the Segment placement survives the restart"
    );
}

#[test]
fn fl004_crash_before_flush_publication_preserves_old_state() {
    // dir() AFTER the child branch (the M4 pattern): the child is killed
    // hard at the park and must never sweep the parent's evidence.
    if std::env::var_os(CHILD_ENV).is_some() {
        let db = Db::open(Config::new(child_dir())).unwrap();
        let a = oid(0xA1);
        db.put_object(a, b"k", b"v1").unwrap();
        db.flush().unwrap(); // parks at after_identity: logs out, no manifest
        unreachable!("the parent kills the parked child");
    }
    let d = dir("fl004");
    let mut child = spawn_child(
        "fl004_crash_before_flush_publication_preserves_old_state",
        &d,
        "after_identity",
    );
    wait_for(&d.join("after_identity"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    // §24 state-B: the new segment and the placement log are orphans —
    // the old state governs. The WAL replays the memtable, so the object
    // is readable and its placement is Memtable again, exactly as before
    // the flush.
    let db = Db::open(Config::new(d)).unwrap();
    let a = oid(0xA1);
    assert_eq!(db.get_object(a, b"k").unwrap(), Some(b"v1".to_vec()));
    let rid = rid_of(&db, a);
    assert!(
        matches!(
            LocalPlacementResolver::new(&db).resolve(rid).unwrap(),
            Some(Placement::Memtable { .. })
        ),
        "the unpublished flush must not move the placement"
    );
}

#[test]
fn fl005_compact_relocates_segment_placements() {
    // SE2-M35 — the M34 fail-closed guard is LIFTED: the merge now carries
    // identity, so compacting with Segment placements relocates them — the
    // replica resolves to its surviving max-seq entry in a fresh merged
    // segment, the old location retired, the data readable.
    let d = dir("fl005");
    let db = Db::open(Config::new(d.clone())).unwrap();
    let a = oid(0xA1);
    db.put_object(a, b"k1", b"v1").unwrap();
    db.flush().unwrap(); // Segment placement published
    db.put_object(a, b"k2", b"v2").unwrap();
    db.flush().unwrap(); // two segments — past compact()'s ≤1 pre-check
    let rid = rid_of(&db, a);
    let old = LocalPlacementResolver::new(&db)
        .resolve(rid)
        .unwrap()
        .unwrap();
    let Placement::Segment(old_loc) = old else {
        panic!("pre-compact placement must be Segment, got {old:?}");
    };
    db.compact().unwrap(); // relocates instead of refusing
    let new = LocalPlacementResolver::new(&db)
        .resolve(rid)
        .unwrap()
        .unwrap();
    let Placement::Segment(loc) = new else {
        panic!("the relocated placement must be Segment, got {new:?}");
    };
    assert!(
        loc.segment_id != old_loc.segment_id,
        "the location moves to the merged output"
    );
    // The anchor names the replica's max-seq surviving entry.
    let entry = SegmentReader::open(&segment_path(&d, loc.segment_id.0))
        .unwrap()
        .entry_at(loc.block_id, loc.entry_offset)
        .unwrap()
        .expect("anchor names an entry");
    assert_eq!(entry.key, b"k2".to_vec());
    assert_eq!(entry.value, b"v2".to_vec());
    assert_eq!(entry.replica_id, rid);
    assert_eq!(db.get_object(a, b"k2").unwrap(), Some(b"v2".to_vec()));
    assert!(
        !segment_path(&d, old_loc.segment_id.0).exists(),
        "the old segment is retired"
    );
}
