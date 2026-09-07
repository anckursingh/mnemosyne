//! SE2-M37 — recovery (§39 milestone 9): RC-001..005. One end-to-end
//! chain — create objects, flush, compact, clean restart — and the whole
//! mapping trio comes back identical: ObjectId→LogicalId (RC-001),
//! LogicalId→ReplicaId (RC-002), ReplicaId→Placement (RC-003), every
//! committed object still readable (RC-004, including the memtable-only
//! object the WAL replays). RC-005 pins the §24 state-C orphan across a
//! restart: the uncommitted relocation never becomes visible, and the
//! database compacts again cleanly after it.

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

fn lid_of(db: &Db, a: ObjectId) -> LogicalId {
    LocalIdentityDirectory::new(db).resolve(a).unwrap().unwrap()
}

fn rid_of(db: &Db, a: ObjectId) -> ReplicaId {
    LocalReplicaDirectory::new(db)
        .resolve_local(lid_of(db, a))
        .unwrap()
        .unwrap()
}

fn placement_of(db: &Db, rid: ReplicaId) -> Option<Placement> {
    LocalPlacementResolver::new(db).resolve(rid).unwrap()
}

/// The deterministic two-flush workload (the cp shape): fixed creation
/// order, so rids/segment ids/anchors/generations reproduce exactly.
fn workload(db: &Db) {
    let a = oid(0xA1);
    let b = oid(0xB2);
    let c = oid(0xC3);
    db.put_object(a, b"a1", b"va1").unwrap();
    db.put_object(a, b"a2", b"va2").unwrap();
    db.put_object(b, b"b1", b"vb1").unwrap();
    db.flush().unwrap();
    db.put_object(a, b"a3", b"va3").unwrap();
    db.put_object(c, b"c1", b"vc1").unwrap();
    db.flush().unwrap();
}

/// (ObjectId, LogicalId, ReplicaId, Placement) for the workload's three
/// objects — the §39 chain's full snapshot.
fn trio_of(db: &Db) -> Vec<(ObjectId, LogicalId, ReplicaId, Placement)> {
    [oid(0xA1), oid(0xB2), oid(0xC3)]
        .into_iter()
        .map(|a| {
            let lid = lid_of(db, a);
            let rid = rid_of(db, a);
            let placement = placement_of(db, rid).expect("placement exists");
            (a, lid, rid, placement)
        })
        .collect()
}

/// §38 — verify all objects: every key the workload wrote still answers
/// the right value after the restart.
fn verify_all_objects(db: &Db) {
    let a = oid(0xA1);
    assert_eq!(db.get_object(a, b"a1").unwrap(), Some(b"va1".to_vec()));
    assert_eq!(db.get_object(a, b"a2").unwrap(), Some(b"va2".to_vec()));
    assert_eq!(db.get_object(a, b"a3").unwrap(), Some(b"va3".to_vec()));
    assert_eq!(
        db.get_object(oid(0xB2), b"b1").unwrap(),
        Some(b"vb1".to_vec())
    );
    assert_eq!(
        db.get_object(oid(0xC3), b"c1").unwrap(),
        Some(b"vc1".to_vec())
    );
}

/// §38 — verify no invalid locations: every Segment placement names a
/// real entry in a manifest segment carrying the replica's id.
fn verify_no_invalid_locations(db: &Db, dir: &Path) {
    let rids = vec![
        rid_of(db, oid(0xA1)),
        rid_of(db, oid(0xB2)),
        rid_of(db, oid(0xC3)),
    ];
    for rid in rids {
        let Some(placement) = placement_of(db, rid) else {
            panic!("replica {rid:?} has no placement after the restart");
        };
        if let Placement::Segment(loc) = placement {
            let entry = SegmentReader::open(&segment_path(dir, loc.segment_id.0))
                .unwrap()
                .entry_at(loc.block_id, loc.entry_offset)
                .unwrap()
                .expect("the placement names an existing entry");
            assert_eq!(entry.replica_id, rid, "the entry carries the replica's id");
        }
    }
}

// ---------------------------------------------------------------------------
// RC-001..004 — the clean restart chain (drop → reopen, no crash)
// ---------------------------------------------------------------------------

#[test]
fn rc001_restart_restores_identity_mappings() {
    let d = dir("rc001");
    let before = {
        let db = Db::open(Config::new(d.clone())).unwrap();
        workload(&db);
        trio_of(&db)
    }; // the Db drops here — a clean shutdown
    let db = Db::open(Config::new(d.clone())).unwrap();
    let after = trio_of(&db);
    for ((a, want_lid, ..), (_, got_lid, ..)) in before.iter().zip(&after) {
        assert_eq!(
            want_lid, got_lid,
            "RC-001: {a:?} → LogicalId survives restart"
        );
    }
}

#[test]
fn rc002_restart_restores_replica_mappings() {
    let d = dir("rc002");
    let before = {
        let db = Db::open(Config::new(d.clone())).unwrap();
        workload(&db);
        trio_of(&db)
    };
    let db = Db::open(Config::new(d.clone())).unwrap();
    let after = trio_of(&db);
    for ((_, _, want_rid, _), (_, _, got_rid, _)) in before.iter().zip(&after) {
        assert_eq!(want_rid, got_rid, "RC-002: ReplicaId survives restart");
    }
}

#[test]
fn rc003_restart_restores_placement_mappings() {
    let d = dir("rc003");
    let before = {
        let mut cfg = Config::new(d.clone());
        cfg.l0_compact_trigger = 0;
        let db = Db::open(cfg).unwrap();
        workload(&db);
        db.compact().unwrap(); // the relocated placements are the pin
        trio_of(&db)
    };
    let db = Db::open(Config::new(d.clone())).unwrap();
    let after = trio_of(&db);
    for ((_, _, rid, want), (_, _, _, got)) in before.iter().zip(&after) {
        assert_eq!(
            want, got,
            "RC-003: {rid:?} → relocated Placement survives restart"
        );
    }
}

#[test]
fn rc004_all_committed_objects_readable() {
    let d = dir("rc004");
    {
        let db = Db::open(Config::new(d.clone())).unwrap();
        workload(&db);
        // A committed object that never flushed — only the WAL knows it.
        db.put_object(oid(0xD4), b"d1", b"vd1").unwrap();
    }
    let db = Db::open(Config::new(d.clone())).unwrap();
    verify_all_objects(&db);
    assert_eq!(
        db.get_object(oid(0xD4), b"d1").unwrap(),
        Some(b"vd1".to_vec()),
        "RC-004: the memtable-only committed object replays from the WAL"
    );
}

// ---------------------------------------------------------------------------
// RC-005 — the state-C orphan: kill between the relocation log and the
// manifest, restart, and the uncommitted relocation never becomes visible
// — then compact again cleanly on top of the orphan.
// ---------------------------------------------------------------------------

const CHILD_ENV: &str = "AIKOQL_V2_KILL_CHILD";
const DIR_ENV: &str = "AIKOQL_V2_KILL_DIR";
const COMPACT_ENV: &str = "AIKOQL_V2_COMPACT_PARK";

fn child_dir() -> PathBuf {
    PathBuf::from(std::env::var(DIR_ENV).expect("child dir env"))
}

fn spawn_child(test_name: &str, dir: &Path, stage: &str) -> Child {
    Command::new(std::env::current_exe().expect("current exe"))
        .arg("--exact")
        .arg(test_name)
        .env(CHILD_ENV, "1")
        .env(DIR_ENV, dir)
        .env(COMPACT_ENV, stage)
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

/// The scratch replay: the workload's trio WITHOUT the compaction — the
/// exact pre-compact mappings the state-C restart must show.
fn scratch_trio(name: &str) -> Vec<(ObjectId, LogicalId, ReplicaId, Placement)> {
    // One scratch dir PER test — cargo runs tests in parallel and two
    // tests sweeping the same name would race each other's replay.
    let d = dir(name);
    let ps = {
        let mut cfg = Config::new(d.clone());
        cfg.l0_compact_trigger = 0;
        let db = Db::open(cfg).unwrap();
        workload(&db);
        trio_of(&db)
    }; // the Db drops here — no handle held when the dir is swept
    std::fs::remove_dir_all(&d).ok();
    ps
}

#[test]
fn rc005_no_uncommitted_relocation_visible() {
    if std::env::var_os(CHILD_ENV).is_some() {
        let db = Db::open(child_cfg()).unwrap();
        workload(&db);
        db.compact().unwrap(); // parks at after_location (state-C)
        unreachable!("the parent kills the parked child");
    }
    let d = dir("rc005");
    let pre = scratch_trio("rc005-scratch");
    let mut child = spawn_child(
        "rc005_no_uncommitted_relocation_visible",
        &d,
        "after_location",
    );
    wait_for(&d.join("after_location"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    // The §24 state-C restart: the relocation log sits past CURRENT (an
    // orphan), and the pre-compact mappings — the whole trio — govern.
    let current = Current::read(&d.join("CURRENT")).unwrap();
    assert!(
        placement_log_path(&d, current.manifest_generation + 1).exists(),
        "the relocation log was renamed before the kill"
    );
    let db = Db::open(Config::new(d.clone())).unwrap();
    let after = trio_of(&db);
    for ((a, _, rid, want), (_, _, _, got)) in pre.iter().zip(&after) {
        assert_eq!(
            want, got,
            "RC-005: {a:?}/{rid:?} keeps the old placement — the uncommitted relocation is invisible"
        );
    }
    verify_all_objects(&db);
    verify_no_invalid_locations(&db, &d);

    // And the database compacts again cleanly on top of the orphan: the
    // fresh compaction allocates strictly newer placement generations and
    // its own log/manifest/CURRENT generation, and a final restart keeps
    // them.
    let before_gens: Vec<u64> = after.iter().map(|(_, _, _, p)| p.generation()).collect();
    db.compact().unwrap();
    let relocated = trio_of(&db);
    for (((_, _, rid, _), (_, _, _, p)), gen) in pre.iter().zip(&relocated).zip(&before_gens) {
        assert!(
            p.generation() > *gen,
            "RC-005: the second compact gives {rid:?} a fresh generation {} > {gen}",
            p.generation()
        );
    }
    verify_all_objects(&db);
    verify_no_invalid_locations(&db, &d);
    drop(db);
    let db = Db::open(Config::new(d.clone())).unwrap();
    let final_ = trio_of(&db);
    for ((_, _, _rid, want), (_, _, _, got)) in relocated.iter().zip(&final_) {
        assert_eq!(
            want, got,
            "RC-005: the second compaction's placements survive a final restart"
        );
    }
    verify_all_objects(&db);
    verify_no_invalid_locations(&db, &d);
}
