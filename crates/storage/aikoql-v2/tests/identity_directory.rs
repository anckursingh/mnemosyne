//! SE2-M30 — identity directory (spec §32 milestone 2): the persistent
//! ObjectId → LogicalId mapping. ID-010..015: allocation, resolution,
//! restart persistence, uniqueness, no-reuse, and the crash window between
//! identity-log publish and CURRENT (child-kill, the KSE-15 harness).

mod common;

use aikoql_storage_v2::db::{Config, Db, DurabilityMode};
use aikoql_storage_v2::format::Current;
use aikoql_storage_v2::identity::ObjectId;
use common::dir;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CHILD_ENV: &str = "AIKOQL_V2_KILL_CHILD";
const DIR_ENV: &str = "AIKOQL_V2_KILL_DIR";
const STAGE_ENV: &str = "AIKOQL_V2_FLUSH_PARK";

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

/// ID-010 — a new ObjectId receives a LogicalId (and, invisibly for M30, a
/// ReplicaId): create_object allocates, resolve_object answers, and two
/// creates get distinct object AND logical ids. The GroupCommit arm pins
/// the same pipeline through the committer.
#[test]
fn id010_create_object_allocates_and_resolves() {
    let d = dir("m30-id010");
    let db = Db::open(Config::new(d)).unwrap();
    let oid1 = db.create_object().unwrap();
    let lid1 = db
        .resolve_object(oid1)
        .expect("a new object has a logical id");
    let oid2 = db.create_object().unwrap();
    assert_ne!(oid1, oid2, "object ids are distinct");
    assert_ne!(
        lid1,
        db.resolve_object(oid2).expect("second object resolves"),
        "logical ids are distinct"
    );
    drop(db);

    let mut cfg = Config::new(dir("m30-id010-gc"));
    cfg.durability = DurabilityMode::GroupCommit;
    let db = Db::open(cfg).unwrap();
    let oid = db.create_object().unwrap();
    assert!(
        db.resolve_object(oid).is_some(),
        "group-commit create resolves"
    );
}

/// ID-011 — resolving the same ObjectId returns the same LogicalId; an
/// unknown ObjectId resolves to None.
#[test]
fn id011_same_object_resolves_to_same_logical_id() {
    let db = Db::open(Config::new(dir("m30-id011"))).unwrap();
    let oid = db.create_object().unwrap();
    let first = db.resolve_object(oid).expect("resolves");
    for _ in 0..10 {
        assert_eq!(db.resolve_object(oid), Some(first), "resolution is stable");
    }
    let unknown = ObjectId::from_bytes([0xEE; 16]);
    assert_eq!(db.resolve_object(unknown), None, "unknown object id");
}

/// ID-012 — restart preserves ObjectId → LogicalId across BOTH recovery
/// paths: the flushed half rides the published identity log, the unflushed
/// half rides the active WAL.
#[test]
fn id012_restart_preserves_object_to_logical_mapping() {
    let d = dir("m30-id012");
    let mut expected: Vec<(ObjectId, u64)> = Vec::new();
    {
        let db = Db::open(Config::new(d.clone())).unwrap();
        for _ in 0..5 {
            let oid = db.create_object().unwrap();
            let lid = db.resolve_object(oid).unwrap().0;
            expected.push((oid, lid));
        }
        db.put(b"k", b"v").unwrap();
        db.flush().unwrap(); // exports the 5 pending records, truncates the WAL
        for _ in 0..5 {
            let oid = db.create_object().unwrap();
            let lid = db.resolve_object(oid).unwrap().0;
            expected.push((oid, lid));
        }
        // drop without flushing — the second five survive only in the WAL
    }
    let db = Db::open(Config::new(d)).unwrap();
    for (oid, lid) in &expected {
        assert_eq!(
            db.resolve_object(*oid).map(|l| l.0),
            Some(*lid),
            "mapping survived restart"
        );
    }
}

/// ID-013 — two ObjectIds never receive the same LogicalId.
#[test]
fn id013_two_object_ids_never_share_a_logical_id() {
    let db = Db::open(Config::new(dir("m30-id013"))).unwrap();
    let mut lids = HashSet::new();
    for _ in 0..500 {
        let oid = db.create_object().unwrap();
        let lid = db.resolve_object(oid).expect("resolves").0;
        assert!(lids.insert(lid), "logical id {lid} allocated twice");
    }
}

/// ID-014 — LogicalIds are never reused after restart: the allocator
/// recovers past every id that ever existed and only allocates above it.
#[test]
fn id014_logical_ids_never_reused_after_restart() {
    let d = dir("m30-id014");
    let mut max = 0u64;
    {
        let db = Db::open(Config::new(d.clone())).unwrap();
        for _ in 0..10 {
            let oid = db.create_object().unwrap();
            max = max.max(db.resolve_object(oid).unwrap().0);
        }
        // drop with the creates still in the WAL — recovery must still
        // place the allocator above them
    }
    let db = Db::open(Config::new(d)).unwrap();
    for _ in 0..10 {
        let oid = db.create_object().unwrap();
        let lid = db.resolve_object(oid).unwrap().0;
        assert!(lid > max, "logical id {lid} reused after restart");
        max = lid;
    }
}

/// ID-015 — a kill between identity-log publish and CURRENT (the §24
/// state-C window) recovers exactly ONE authoritative mapping: the orphan
/// log (generation past CURRENT) is reported and ignored, the WAL rebuilds
/// the mapping, and no duplicate or ambiguity survives.
#[test]
fn id015_crash_between_identity_log_and_current_recovers_one_mapping() {
    // dir() AFTER the child branch: the child would otherwise create its own
    // pid-namespaced dir at this line and, being hard-killed, never sweep it.
    if std::env::var_os(CHILD_ENV).is_some() {
        let mut cfg = Config::new(child_dir());
        cfg.l0_compact_trigger = 0; // pin the explicit flush's windows
        let db = Db::open(cfg).unwrap();
        let oid = db.create_object().unwrap();
        let lid = db.resolve_object(oid).unwrap();
        // The parent cannot compute the ids — write what it must assert.
        std::fs::write(
            child_dir().join("EXPECTED-IDENTITY"),
            format!("{} {}", oid, lid.0),
        )
        .unwrap();
        db.put(b"data", b"x").unwrap();
        db.flush().unwrap(); // parks at after_identity, mid-publication
        unreachable!("the parent kills the parked child");
    }
    let d = dir("m30-id015");
    let mut child = spawn_child(
        "id015_crash_between_identity_log_and_current_recovers_one_mapping",
        &d,
        "after_identity",
    );
    wait_for(&d.join("after_identity"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    // The state-C fingerprint: the generation-2 delta logs are published
    // while CURRENT still names generation 1.
    let current = Current::read(&d.join("CURRENT")).unwrap();
    assert_eq!(
        current.manifest_generation, 1,
        "CURRENT not yet advanced past the park"
    );
    assert!(
        d.join("IDENTITY-000002.log").exists(),
        "identity delta published before the park"
    );
    assert!(
        d.join("REPLICA-000002.log").exists(),
        "replica delta published before the park"
    );

    // Reopen: the orphan logs are ignored (reported), the WAL rebuilds the
    // one authoritative mapping — no duplicate, no ambiguity.
    let db = Db::open(Config::new(d.clone())).unwrap();
    let exp = std::fs::read_to_string(d.join("EXPECTED-IDENTITY")).unwrap();
    let (hex, lid_str) = exp.split_once(' ').unwrap();
    let mut bytes = [0u8; 16];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
    }
    let oid = ObjectId::from_bytes(bytes);
    let lid: u64 = lid_str.parse().unwrap();
    assert_eq!(
        db.resolve_object(oid).map(|l| l.0),
        Some(lid),
        "the WAL rebuilds exactly the child's mapping"
    );
    let next = db.create_object().unwrap();
    assert_eq!(
        db.resolve_object(next).map(|l| l.0),
        Some(2),
        "the allocator continues above the recovered mapping — no reuse"
    );
    assert!(
        d.join("IDENTITY-000002.log").exists(),
        "the orphan log stays on disk (reported and ignored, not deleted)"
    );
}
