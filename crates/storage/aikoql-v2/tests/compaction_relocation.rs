//! SE2-M35 — compaction relocation REDs (spec §37 milestone 7, §21–25):
//! compaction preserves the identity triple (CP-001..003) while the
//! physical location may change (CP-004); after the merge the replica
//! resolves to its NEW location — the surviving max-seq entry's
//! block/offset in the merged output (CP-005); the old location is never
//! returned after publication (CP-006); a replica whose last live entry
//! died in the merge retires (§16, CP-007); generations stay monotonic
//! across relocations (CP-008, §25); the §24 state-C/D windows pin the
//! §23 publication order with the child-kill harness (CP-009/010) —
//! kill after the placement log but before the manifest: old placements
//! still authoritative (the new log is an orphan); kill after CURRENT:
//! the relocated placements govern.

mod common;

use aikoql_storage_v2::db::{Config, Db};
use aikoql_storage_v2::format::Current;
use aikoql_storage_v2::identity::directory::{IdentityResolver, LocalIdentityDirectory};
use aikoql_storage_v2::identity::topology::{LocalReplicaDirectory, ReplicaDirectory};
use aikoql_storage_v2::identity::{LogicalId, ObjectId, ReplicaId};
use aikoql_storage_v2::placement::directory::{
    placement_log_path, LocalPlacementResolver, Placement, PlacementResolver,
};
use aikoql_storage_v2::segment::{segment_path, SegmentReader};
use common::dir;
use std::path::{Path, PathBuf};
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

fn lid_of(db: &Db, a: ObjectId) -> LogicalId {
    LocalIdentityDirectory::new(db).resolve(a).unwrap().unwrap()
}

fn placement_of(db: &Db, rid: ReplicaId) -> Option<Placement> {
    LocalPlacementResolver::new(db).resolve(rid).unwrap()
}

/// The deterministic two-flush workload shared by CP-001..008 and the
/// crash pins: object creation order is fixed, so rids/segment ids/anchors
/// reproduce exactly across directories and processes.
fn workload(db: &Db) {
    let a = oid(0xA1);
    let b = oid(0xB2);
    let c = oid(0xC3);
    db.put_object(a, b"a1", b"va1").unwrap();
    db.put_object(a, b"a2", b"va2").unwrap();
    db.put_object(b, b"b1", b"vb1").unwrap();
    db.flush().unwrap();
    db.put_object(a, b"a3", b"va3").unwrap(); // a's newest key
    db.put_object(c, b"c1", b"vc1").unwrap();
    db.flush().unwrap();
}

#[test]
fn cp001_compaction_preserves_object_identity() {
    let d = dir("cp001");
    let db = Db::open(Config::new(d.clone())).unwrap();
    workload(&db);
    db.compact().unwrap();
    let a = oid(0xA1);
    let b = oid(0xB2);
    let c = oid(0xC3);
    assert_eq!(db.get_object(a, b"a1").unwrap(), Some(b"va1".to_vec()));
    assert_eq!(db.get_object(a, b"a2").unwrap(), Some(b"va2".to_vec()));
    assert_eq!(db.get_object(a, b"a3").unwrap(), Some(b"va3".to_vec()));
    assert_eq!(db.get_object(b, b"b1").unwrap(), Some(b"vb1".to_vec()));
    assert_eq!(db.get_object(c, b"c1").unwrap(), Some(b"vc1".to_vec()));
    // The directory still resolves every object — the identity mapping
    // survived the merge.
    assert!(LocalIdentityDirectory::new(&db)
        .resolve(a)
        .unwrap()
        .is_some());
    assert!(LocalIdentityDirectory::new(&db)
        .resolve(b)
        .unwrap()
        .is_some());
    assert!(LocalIdentityDirectory::new(&db)
        .resolve(c)
        .unwrap()
        .is_some());
}

#[test]
fn cp002_compaction_preserves_logical_id() {
    let d = dir("cp002");
    let db = Db::open(Config::new(d.clone())).unwrap();
    workload(&db);
    let before = (
        lid_of(&db, oid(0xA1)),
        lid_of(&db, oid(0xB2)),
        lid_of(&db, oid(0xC3)),
    );
    db.compact().unwrap();
    let after = (
        lid_of(&db, oid(0xA1)),
        lid_of(&db, oid(0xB2)),
        lid_of(&db, oid(0xC3)),
    );
    assert_eq!(before, after, "compaction must not move the oid→lid map");
}

#[test]
fn cp003_compaction_preserves_replica_id() {
    let d = dir("cp003");
    let db = Db::open(Config::new(d.clone())).unwrap();
    workload(&db);
    let before = (
        rid_of(&db, oid(0xA1)),
        rid_of(&db, oid(0xB2)),
        rid_of(&db, oid(0xC3)),
    );
    db.compact().unwrap();
    let after = (
        rid_of(&db, oid(0xA1)),
        rid_of(&db, oid(0xB2)),
        rid_of(&db, oid(0xC3)),
    );
    assert_eq!(before, after, "compaction must not re-allocate replicas");
}

#[test]
fn cp004_compaction_may_change_physical_location() {
    let d = dir("cp004");
    let db = Db::open(Config::new(d.clone())).unwrap();
    workload(&db);
    let rids = (
        rid_of(&db, oid(0xA1)),
        rid_of(&db, oid(0xB2)),
        rid_of(&db, oid(0xC3)),
    );
    let old: Vec<Placement> = vec![
        placement_of(&db, rids.0).unwrap(),
        placement_of(&db, rids.1).unwrap(),
        placement_of(&db, rids.2).unwrap(),
    ];
    for p in &old {
        assert!(
            matches!(p, Placement::Segment(_)),
            "pre-compact placements are Segments, got {p:?}"
        );
    }
    let old_ids: Vec<u64> = old
        .iter()
        .map(|p| match p {
            Placement::Segment(loc) => loc.segment_id.0,
            other => panic!("not a segment placement: {other:?}"),
        })
        .collect();
    db.compact().unwrap();
    for (rid, old_id) in [rids.0, rids.1, rids.2].iter().zip(&old_ids) {
        let Placement::Segment(loc) = placement_of(&db, *rid).unwrap() else {
            panic!("placement after compaction must be Segment");
        };
        // The merged output is a FRESH segment — the location necessarily
        // changes, and it must resolve: the anchor names a real entry.
        assert!(
            !old_ids.contains(&loc.segment_id.0),
            "the new placement must not name an input segment"
        );
        let entry = SegmentReader::open(&segment_path(&d, loc.segment_id.0))
            .unwrap()
            .entry_at(loc.block_id, loc.entry_offset)
            .unwrap()
            .expect("the relocated anchor names an entry");
        assert_eq!(entry.replica_id, *rid);
        assert!(*old_id != loc.segment_id.0);
    }
}

#[test]
fn cp005_after_compaction_replica_resolves_to_new_location() {
    let d = dir("cp005");
    let db = Db::open(Config::new(d.clone())).unwrap();
    workload(&db);
    let a = oid(0xA1);
    let rid = rid_of(&db, a);
    db.compact().unwrap();
    // The relocated placement anchors the replica's MAX-SEQ surviving
    // entry: a's newest key, with its value and rid — and the same
    // placement comes back after a restart (the relocation rode the
    // PLACEMENT log in the publication window).
    let loc = match placement_of(&db, rid) {
        Some(Placement::Segment(loc)) => loc,
        other => panic!("expected a Segment placement, got {other:?}"),
    };
    let entry = SegmentReader::open(&segment_path(&d, loc.segment_id.0))
        .unwrap()
        .entry_at(loc.block_id, loc.entry_offset)
        .unwrap()
        .expect("anchor names an entry");
    assert_eq!(entry.key, b"a3".to_vec(), "the anchor is the max-seq entry");
    assert_eq!(entry.value, b"va3".to_vec());
    assert_eq!(entry.replica_id, rid);
    drop(db);
    let db = Db::open(Config::new(d)).unwrap();
    assert_eq!(
        placement_of(&db, rid),
        Some(Placement::Segment(loc)),
        "the relocated placement survives the restart"
    );
}

#[test]
fn cp006_old_location_never_returned_after_publication() {
    let d = dir("cp006");
    let db = Db::open(Config::new(d.clone())).unwrap();
    workload(&db);
    let a = oid(0xA1);
    let rid = rid_of(&db, a);
    let old = placement_of(&db, rid).unwrap();
    let Placement::Segment(old_loc) = old else {
        panic!("pre-compact placement must be Segment");
    };
    db.compact().unwrap();
    // The old location is gone for good: never resolved again, the old
    // segment file retired, the new placement stable across restarts.
    let new = placement_of(&db, rid).unwrap();
    assert_ne!(new, old, "the old location must not be returned");
    assert!(
        matches!(new, Placement::Segment(_)),
        "the new placement is a Segment"
    );
    assert!(
        !segment_path(&d, old_loc.segment_id.0).exists(),
        "the obsolete segment is retired after publication"
    );
    drop(db);
    for _ in 0..2 {
        let db = Db::open(Config::new(d.clone())).unwrap();
        let again = placement_of(&db, rid).unwrap();
        assert_eq!(again, new, "the new placement is stable across restarts");
        assert_ne!(again, old, "the old location never comes back");
        assert_eq!(db.get_object(a, b"a3").unwrap(), Some(b"va3".to_vec()));
        drop(db);
    }
}

#[test]
fn cp007_tombstone_dropped_replica_is_retired() {
    // §16 — when the merge drops a replica's LAST live entry (its
    // tombstone wins), the placement retires: the directories keep the
    // identity mapping, the placement says Retired — historically
    // resolvable, never dangling at a deleted segment.
    let d = dir("cp007");
    let db = Db::open(Config::new(d.clone())).unwrap();
    let a = oid(0xA1);
    db.put_object(a, b"k", b"v1").unwrap();
    db.flush().unwrap();
    db.put_object(a, b"k", b"v2").unwrap();
    db.flush().unwrap();
    db.delete_object(a, b"k").unwrap();
    db.flush().unwrap();
    let rid = rid_of(&db, a);
    assert!(matches!(
        placement_of(&db, rid),
        Some(Placement::Segment(_))
    ));
    db.compact().unwrap();
    assert_eq!(db.get_object(a, b"k").unwrap(), None, "the tombstone wins");
    assert!(
        matches!(placement_of(&db, rid), Some(Placement::Retired { .. })),
        "the dropped replica's placement retires (§16)"
    );
    // Identity survives: the mapping still resolves, historically.
    assert!(LocalIdentityDirectory::new(&db)
        .resolve(a)
        .unwrap()
        .is_some());
    drop(db);
    let db = Db::open(Config::new(d)).unwrap();
    assert!(
        matches!(placement_of(&db, rid), Some(Placement::Retired { .. })),
        "Retired survives the restart"
    );
}

#[test]
fn cp008_generations_monotonic_across_relocations() {
    // §25 — every relocation allocates a FRESH placement generation, so
    // newer relocations always out-rank older ones; a stale record can
    // never resurrect an old location.
    let d = dir("cp008");
    let db = Db::open(Config::new(d.clone())).unwrap();
    let a = oid(0xA1);
    let rid;
    {
        db.put_object(a, b"k1", b"v1").unwrap();
        db.flush().unwrap();
        db.put_object(a, b"k2", b"v2").unwrap();
        db.flush().unwrap();
        rid = rid_of(&db, a);
        db.compact().unwrap();
    }
    let gen1 = placement_of(&db, rid).unwrap().generation();
    {
        db.put_object(a, b"k3", b"v3").unwrap();
        db.flush().unwrap();
        db.put_object(a, b"k4", b"v4").unwrap();
        db.flush().unwrap();
        db.compact().unwrap();
    }
    let gen2 = placement_of(&db, rid).unwrap().generation();
    assert!(gen2 > gen1, "relocation generations must be monotonic");
}

// ---------------------------------------------------------------------------
// §24 state-C/D child-kill pins: the §23 publication order under the axe.
// ---------------------------------------------------------------------------

const CHILD_ENV: &str = "AIKOQL_V2_KILL_CHILD";
const DIR_ENV: &str = "AIKOQL_V2_KILL_DIR";
const STAGE_ENV: &str = "AIKOQL_V2_COMPACT_PARK";

fn child_dir() -> PathBuf {
    PathBuf::from(std::env::var(DIR_ENV).expect("child dir env"))
}

fn spawn_child(test_name: &str, dir: &Path, stage: &str) -> Child {
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

fn wait_for(path: &Path, timeout: Duration) {
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

fn child_cfg() -> Config {
    let mut cfg = Config::new(child_dir());
    cfg.l0_compact_trigger = 0; // SE2-M10 — the explicit compact's windows only
    cfg
}

/// The scratch replay: the parent reproduces the child's exact workload
/// (fixed creation order → identical rids, segment ids, anchors) to learn
/// the placements the child must hold at each window. The scratch dir is
/// swept before returning — it is evidence of the parent's computation,
/// not of the child's crash.
fn scratch_placements(name: &str, pre: bool) -> Vec<(ReplicaId, Placement)> {
    // One scratch dir PER test — cargo runs tests in parallel and two
    // tests sweeping the same name would race each other's replay.
    let d = dir(name);
    let ps = {
        let mut cfg = Config::new(d.clone());
        cfg.l0_compact_trigger = 0;
        let db = Db::open(cfg).unwrap();
        workload(&db);
        let rids = vec![
            rid_of(&db, oid(0xA1)),
            rid_of(&db, oid(0xB2)),
            rid_of(&db, oid(0xC3)),
        ];
        if !pre {
            db.compact().unwrap();
        }
        rids.into_iter()
            .map(|r| (r, placement_of(&db, r).unwrap()))
            .collect()
    }; // the Db drops here — no handle held when the dir is swept
    std::fs::remove_dir_all(&d).ok();
    ps
}

#[test]
fn cp009_kill_after_location_before_manifest_keeps_old_placements() {
    // §24 state-C: the placement log is durable, the manifest is not —
    // the OLD placements stay authoritative (the new log is an orphan
    // past CURRENT's generation, reported and ignored).
    if std::env::var_os(CHILD_ENV).is_some() {
        let db = Db::open(child_cfg()).unwrap();
        workload(&db);
        db.compact().unwrap(); // parks at after_location
        unreachable!("the parent kills the parked child");
    }
    let d = dir("cp009");
    let pre = scratch_placements("cp009-scratch", true);
    let mut child = spawn_child(
        "cp009_kill_after_location_before_manifest_keeps_old_placements",
        &d,
        "after_location",
    );
    wait_for(&d.join("after_location"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    let current = Current::read(&d.join("CURRENT")).unwrap();
    assert!(
        placement_log_path(&d, current.manifest_generation + 1).exists(),
        "the relocation log was written before the kill"
    );
    let db = Db::open(Config::new(d.clone())).unwrap();
    for (rid, want) in &pre {
        assert_eq!(
            placement_of(&db, *rid),
            Some(*want),
            "the old placement stays authoritative until the manifest"
        );
    }
    assert_eq!(
        db.get_object(oid(0xA1), b"a3").unwrap(),
        Some(b"va3".to_vec())
    );
}

#[test]
fn cp010_kill_after_current_publishes_relocated_placements() {
    // §24 state-D: CURRENT names the merged generation — the relocated
    // placements govern on reopen, the old segments are gone or
    // report-and-ignored.
    if std::env::var_os(CHILD_ENV).is_some() {
        let db = Db::open(child_cfg()).unwrap();
        workload(&db);
        db.compact().unwrap(); // parks at after_current
        unreachable!("the parent kills the parked child");
    }
    let d = dir("cp010");
    let post = scratch_placements("cp010-scratch", false);
    let mut child = spawn_child(
        "cp010_kill_after_current_publishes_relocated_placements",
        &d,
        "after_current",
    );
    wait_for(&d.join("after_current"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    let db = Db::open(Config::new(d)).unwrap();
    for (rid, want) in &post {
        assert_eq!(
            placement_of(&db, *rid),
            Some(*want),
            "the relocated placement governs after CURRENT"
        );
    }
    assert_eq!(
        db.get_object(oid(0xA1), b"a3").unwrap(),
        Some(b"va3".to_vec())
    );
}
