//! SE2-M40 — the directory checkpoint + bounded recovery (review P0-1 /
//! P0-2 / P0-3, milestones M5-M7 and the M12 growth gate): the format
//! golden and damage matrix, checkpoint-vs-full-replay equivalence, the
//! corrupt-checkpoint fail-closed policy, the five crash windows of the
//! publication protocol, pruning and budget-resume pins, the placement
//! generation-allocator hardening (Challenge C), the randomized restart
//! oracle, and — strict opt-in (`SE2M40_NIGHTLY=1`) — the directory growth
//! probe that writes `artifacts/storage-engine-v2/directory-checkpoint.md`
//! (recovery ∝ checkpoint + recent deltas, never the full history).

mod common;

use aikoql_storage_v2::checkpoint::{checkpoint_generation, checkpoint_path, DirectoryCheckpoint};
use aikoql_storage_v2::db::{Config, Db, DurabilityMode};
use aikoql_storage_v2::format::{Current, FormatError, FORMAT_VERSION};
use aikoql_storage_v2::identity::directory::{IdentityResolver, LocalIdentityDirectory};
use aikoql_storage_v2::identity::topology::{LocalReplicaDirectory, ReplicaDirectory};
use aikoql_storage_v2::identity::{LogicalId, ObjectId, ReplicaId};
use aikoql_storage_v2::placement::directory::{
    load_placement_logs, orphan_placement_logs, orphan_placement_max_generation,
    LocalPlacementResolver, PhysicalLocation, Placement, PlacementResolver,
};
use aikoql_storage_v2::placement::{BlockId, SegmentId};
use common::{dir, hex, run_date};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn oid(byte: u8) -> ObjectId {
    ObjectId([byte; 16])
}

fn obj_id(i: usize) -> ObjectId {
    let mut raw = [0u8; 16];
    raw[..8].copy_from_slice(&(i as u64).to_le_bytes());
    ObjectId(raw)
}

fn rid_of(db: &Db, a: ObjectId) -> ReplicaId {
    let lid = LocalIdentityDirectory::new(db).resolve(a).unwrap().unwrap();
    LocalReplicaDirectory::new(db)
        .resolve_local(lid)
        .unwrap()
        .unwrap()
}

fn placement_of(db: &Db, rid: ReplicaId) -> Option<Placement> {
    LocalPlacementResolver::new(db).resolve(rid).unwrap()
}

/// A `STEM-{gen:06}.log` name → generation. The crate's parsers are
/// pub(crate); the tests re-parse names (never file contents).
fn stem_gen(name: &str, stem: &str) -> Option<u64> {
    name.strip_prefix(stem)
        .and_then(|s| s.strip_suffix(".log"))
        .and_then(|g| g.parse::<u64>().ok())
}

fn stem_gens(d: &Path, stem: &str) -> Vec<u64> {
    let mut gens: Vec<u64> = std::fs::read_dir(d)
        .unwrap()
        .flatten()
        .filter_map(|e| stem_gen(&e.file_name().to_string_lossy(), stem))
        .collect();
    gens.sort_unstable();
    gens
}

fn checkpoint_gens(d: &Path) -> Vec<u64> {
    let mut gens: Vec<u64> = std::fs::read_dir(d)
        .unwrap()
        .flatten()
        .filter_map(|e| checkpoint_generation(&e.file_name().to_string_lossy()))
        .collect();
    gens.sort_unstable();
    gens
}

/// The delta-log file count across all three families.
fn log_file_count(d: &Path) -> usize {
    ["IDENTITY-", "REPLICA-", "PLACEMENT-"]
        .iter()
        .map(|stem| stem_gens(d, stem).len())
        .sum()
}

// ---------------------------------------------------------------------------
// ckp001 — the format golden and the damage matrix (decode order: magic →
// version → structure → checksum)
// ---------------------------------------------------------------------------

#[test]
fn ckp001_format_golden_and_damage() {
    // One record of each placement variant; from_state sorts by key, so
    // the fixture is byte-exact whatever the map iteration order was.
    let mut identity = HashMap::new();
    identity.insert(ObjectId([0x11; 16]), LogicalId(1));
    identity.insert(ObjectId([0x22; 16]), LogicalId(2));
    let mut replicas = HashMap::new();
    replicas.insert(LogicalId(1), ReplicaId(10));
    replicas.insert(LogicalId(2), ReplicaId(20));
    let mut placements = HashMap::new();
    placements.insert(
        ReplicaId(10),
        Placement::Segment(PhysicalLocation {
            segment_id: SegmentId(5),
            block_id: BlockId(3),
            entry_offset: 7,
            generation: 9,
        }),
    );
    placements.insert(ReplicaId(20), Placement::Retired { generation: 11 });
    placements.insert(ReplicaId(30), Placement::Memtable { generation: 4 });
    let checkpoint = DirectoryCheckpoint::from_state(7, &identity, &replicas, &placements);
    let encoded = checkpoint.encode();
    assert_eq!(DirectoryCheckpoint::decode(&encoded).unwrap(), checkpoint);

    // The frozen golden — the only format-drift surface left to eyeballs.
    assert_eq!(
        hex(&encoded),
        "414b434b010007000000000000000200000011111111111111111111111111111111\
         0100000000000000222222222222222222222222222222220200000000000000\
         02000000010000000000000001000000000000000a00000000000000\
         020000000000000001000000000000001400000000000000030000000a0000000000000002\
         050000000000000003000000070000000900000000000000140000000000000003\
         000000000000000000000000000000000b000000000000001e0000000000000001\
         000000000000000000000000000000000400000000000000f948258a71a12d46"
    );

    let mut bad_magic = encoded.clone();
    bad_magic[0] ^= 0xFF;
    assert!(matches!(
        DirectoryCheckpoint::decode(&bad_magic),
        Err(FormatError::Corrupt(_))
    ));

    assert!(matches!(
        DirectoryCheckpoint::decode(&encoded[..encoded.len() - 8]),
        Err(FormatError::Corrupt(_))
    ));

    let mut trailing = encoded.clone();
    trailing.push(0);
    assert!(matches!(
        DirectoryCheckpoint::decode(&trailing),
        Err(FormatError::Corrupt(_))
    ));

    let mut flipped = encoded.clone();
    flipped[30] ^= 0xFF; // inside a record — the checksum must catch it
    assert!(matches!(
        DirectoryCheckpoint::decode(&flipped),
        Err(FormatError::Corrupt(_))
    ));

    // A clean unknown version is Unsupported (a newer-format file is not
    // damaged) — checked before the checksum.
    let mut newer = encoded.clone();
    newer[4..6].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
    assert!(matches!(
        DirectoryCheckpoint::decode(&newer),
        Err(FormatError::Unsupported(_))
    ));
}

// ---------------------------------------------------------------------------
// ckp002 — a checkpoint-pruned history reopens to byte-exact state equality
// with the full-replay arm (the P0-1 claim)
// ---------------------------------------------------------------------------

fn equivalence_workload(db: &Db) {
    for b in 0x01u8..=0x14 {
        let a = oid(b);
        db.put_object(a, b"a", &[b, 0xAA]).unwrap();
        db.put_object(a, b"b", &[b, 0xBB]).unwrap();
        db.put_object(a, b"c", &[b, 0xCC]).unwrap();
        if b % 5 == 0 {
            db.flush().unwrap();
        }
    }
}

#[test]
fn ckp002_checkpoint_equals_full_replay() {
    let da = dir("ckp002-a");
    let dbp = dir("ckp002-b");
    let mut full_cfg = Config::new(da.clone());
    full_cfg.checkpoint_bytes = 0;
    let mut ckp_cfg = Config::new(dbp.clone());
    ckp_cfg.checkpoint_bytes = 256; // every flush crosses it
    {
        let db = Db::open(full_cfg.clone()).unwrap();
        equivalence_workload(&db);
    }
    {
        let db = Db::open(ckp_cfg.clone()).unwrap();
        equivalence_workload(&db);
    }

    // Each checkpointing flush pruned the history it subsumed — including
    // the very logs that triggered it — and dropped the older checkpoint.
    // (A fresh db bootstraps an empty manifest at gen 1, so the four
    // flushes publish gens 2..=5.)
    assert_eq!(checkpoint_gens(&dbp), vec![5], "one checkpoint, the newest");
    assert_eq!(log_file_count(&dbp), 0, "all subsumed delta logs pruned");
    assert!(
        log_file_count(&da) > log_file_count(&dbp),
        "the full-replay arm kept the history the checkpoint arm pruned"
    );

    let full = Db::open(full_cfg).unwrap();
    let ckp = Db::open(ckp_cfg).unwrap();
    for b in 0x01u8..=0x14 {
        let a = oid(b);
        for key in ["a", "b", "c"] {
            assert_eq!(
                full.get_object(a, key.as_bytes()).unwrap(),
                ckp.get_object(a, key.as_bytes()).unwrap()
            );
        }
        let full_lid = full.resolve_object(a);
        assert_eq!(full_lid, ckp.resolve_object(a));
        let frid = LocalReplicaDirectory::new(&full)
            .resolve_local(full_lid.unwrap())
            .unwrap()
            .unwrap();
        let crid = LocalReplicaDirectory::new(&ckp)
            .resolve_local(full_lid.unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(frid, crid);
        assert_eq!(
            placement_of(&full, frid),
            placement_of(&ckp, crid),
            "byte-exact placement state (same pgens, segments, anchors)"
        );
    }
    // Both arms continue identically after the reopen.
    full.put_object(oid(0x21), b"z", b"vz").unwrap();
    ckp.put_object(oid(0x21), b"z", b"vz").unwrap();
    full.flush().unwrap();
    ckp.flush().unwrap();
    assert_eq!(
        full.get_object(oid(0x21), b"z").unwrap(),
        Some(b"vz".to_vec())
    );
    assert_eq!(
        ckp.get_object(oid(0x21), b"z").unwrap(),
        Some(b"vz".to_vec())
    );
}

// ---------------------------------------------------------------------------
// ckp003 — a damaged checkpoint fails closed: no fallback to the deltas
// (unsound after a partial prune), ignored orphans, name/internal mismatch
// ---------------------------------------------------------------------------

fn ckp003_workload(db: &Db) {
    for b in 0x01u8..=0x0A {
        let a = oid(b);
        db.put_object(a, b"k1", &[b, 1]).unwrap();
        db.put_object(a, b"k2", &[b, 2]).unwrap();
    }
    db.flush().unwrap(); // 10 oids ≥ the 256 B trigger — the flush checkpoints
}

#[test]
fn ckp003_corrupt_checkpoint_fails_closed() {
    let d = dir("ckp003-corrupt");
    let mut cfg = Config::new(d.clone());
    cfg.checkpoint_bytes = 256;
    {
        let db = Db::open(cfg).unwrap();
        ckp003_workload(&db);
    }
    let gens = checkpoint_gens(&d);
    assert_eq!(gens.len(), 1);
    let path = checkpoint_path(&d, gens[0]);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[40] ^= 0xFF; // bit rot in the records
    std::fs::write(&path, &bytes).unwrap();

    let err = Db::open(Config::new(d.clone()))
        .err()
        .expect("the corrupt checkpoint must fail the open");
    assert!(
        matches!(err, FormatError::Corrupt(_)),
        "no delta fallback after the prune: {err:?}"
    );
}

#[test]
fn ckp003_orphan_checkpoint_ignored_and_recovered() {
    let d = dir("ckp003-orphan");
    let mut cfg = Config::new(d.clone());
    cfg.checkpoint_bytes = 256;
    {
        let db = Db::open(cfg.clone()).unwrap();
        ckp003_workload(&db);
    }
    let gens = checkpoint_gens(&d);
    assert_eq!(gens.len(), 1);
    // A §24 state-C-shaped orphan ABOVE CURRENT (valid content, the wrong
    // generation): recovery must ignore it, never trust it.
    let ckp = DirectoryCheckpoint::read(&checkpoint_path(&d, gens[0])).unwrap();
    let mut orphan = ckp.clone();
    orphan.generation += 1;
    std::fs::write(checkpoint_path(&d, gens[0] + 1), orphan.encode()).unwrap();

    let db = Db::open(cfg.clone()).unwrap();
    for b in 0x01u8..=0x0A {
        let a = oid(b);
        assert_eq!(db.get_object(a, b"k1").unwrap(), Some(vec![b, 1]));
        assert_eq!(db.get_object(a, b"k2").unwrap(), Some(vec![b, 2]));
    }
    drop(db);
    // The next checkpointing flush re-publishes that generation for real
    // and prunes the older history.
    let db = Db::open(cfg.clone()).unwrap();
    for b in 0x21u8..=0x28 {
        db.put_object(oid(b), b"k", &[b]).unwrap();
    }
    db.flush().unwrap();
    drop(db);
    let gens = checkpoint_gens(&d);
    assert_eq!(gens.len(), 1, "exactly the newest checkpoint remains");
}

#[test]
fn ckp003_name_internal_generation_mismatch() {
    let d = dir("ckp003-mismatch");
    let mut cfg = Config::new(d.clone());
    cfg.checkpoint_bytes = 256;
    {
        let db = Db::open(cfg).unwrap();
        ckp003_workload(&db);
    }
    let gens = checkpoint_gens(&d);
    assert_eq!(gens.len(), 1);
    // Re-encode with a VALID checksum but the wrong internal generation —
    // a publication anomaly, never picked.
    let ckp = DirectoryCheckpoint::read(&checkpoint_path(&d, gens[0])).unwrap();
    let mut lied = ckp.clone();
    lied.generation += 7;
    std::fs::write(checkpoint_path(&d, gens[0]), lied.encode()).unwrap();

    let err = Db::open(Config::new(d.clone()))
        .err()
        .expect("the mismatched checkpoint must fail the open");
    assert!(
        matches!(err, FormatError::Corrupt(_)),
        "filename/internal generation disagreement fails closed: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// ckp004 — the five crash windows of the publication protocol (review P0-2
// steps 5-9): publish-temp write/fsync, verified checkpoint, first prune,
// full prune. Every window reopens to the complete state.
// ---------------------------------------------------------------------------

const CHILD_ENV: &str = "AIKOQL_V2_KILL_CHILD";
const DIR_ENV: &str = "AIKOQL_V2_KILL_DIR";
const PLACE_ENV: &str = "AIKOQL_V2_PLACE_PARK";
const CKP_ENV: &str = "AIKOQL_V2_CKP_PARK";
const COMPACT_ENV: &str = "AIKOQL_V2_COMPACT_PARK";

fn child_dir() -> PathBuf {
    PathBuf::from(std::env::var(DIR_ENV).expect("child dir env"))
}

fn spawn_ckp_child(test_name: &str, d: &Path, park_env: &str, park_value: &str) -> Child {
    Command::new(std::env::current_exe().expect("current exe"))
        .arg("--exact")
        .arg(test_name)
        .env(CHILD_ENV, "1")
        .env(DIR_ENV, d)
        .env(park_env, park_value)
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

/// The child's config: a tiny checkpoint trigger, so the FIRST flush parks
/// inside its checkpoint publication window.
fn ckp_child_cfg() -> Config {
    let mut cfg = Config::new(child_dir());
    cfg.checkpoint_bytes = 256;
    cfg
}

/// 30 objects × 2 keys, one flush — 30×81 B crosses the trigger.
fn ckp_workload(db: &Db) {
    for b in 0x01u8..=0x1E {
        let a = oid(b);
        db.put_object(a, b"k1", &[b, 1]).unwrap();
        db.put_object(a, b"k2", &[b, 2]).unwrap();
    }
}

fn verify_ckp_workload(db: &Db) {
    for b in 0x01u8..=0x1E {
        let a = oid(b);
        assert_eq!(db.get_object(a, b"k1").unwrap(), Some(vec![b, 1]));
        assert_eq!(db.get_object(a, b"k2").unwrap(), Some(vec![b, 2]));
        assert!(
            placement_of(db, rid_of(db, a)).is_some(),
            "every replica stays placed through every window"
        );
    }
}

#[test]
fn ckp004_window_checkpoint_temp_write() {
    if std::env::var_os(CHILD_ENV).is_some() {
        let db = Db::open(ckp_child_cfg()).unwrap();
        ckp_workload(&db);
        db.flush().unwrap(); // parks inside the checkpoint temp's write
        unreachable!("the parent kills the parked child");
    }
    let d = dir("ckp004-write");
    let mut child = spawn_ckp_child(
        "ckp004_window_checkpoint_temp_write",
        &d,
        PLACE_ENV,
        "FAIL_AFTER_CHECKPOINT_WRITE",
    );
    wait_for(
        &d.join("FAIL_AFTER_CHECKPOINT_WRITE"),
        Duration::from_secs(60),
    );
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    assert!(
        checkpoint_gens(&d).is_empty(),
        "the checkpoint was never renamed into existence"
    );
    let db = Db::open(Config::new(d.clone())).unwrap();
    verify_ckp_workload(&db); // the full delta history covers it
}

#[test]
fn ckp004_window_checkpoint_temp_fsync() {
    if std::env::var_os(CHILD_ENV).is_some() {
        let db = Db::open(ckp_child_cfg()).unwrap();
        ckp_workload(&db);
        db.flush().unwrap(); // parks after the checkpoint temp's fsync
        unreachable!("the parent kills the parked child");
    }
    let d = dir("ckp004-fsync");
    let mut child = spawn_ckp_child(
        "ckp004_window_checkpoint_temp_fsync",
        &d,
        PLACE_ENV,
        "FAIL_AFTER_CHECKPOINT_FSYNC",
    );
    wait_for(
        &d.join("FAIL_AFTER_CHECKPOINT_FSYNC"),
        Duration::from_secs(60),
    );
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    assert!(
        checkpoint_gens(&d).is_empty(),
        "a torn temp is never a checkpoint"
    );
    let db = Db::open(Config::new(d.clone())).unwrap();
    verify_ckp_workload(&db);
}

#[test]
fn ckp004_window_after_checkpoint() {
    if std::env::var_os(CHILD_ENV).is_some() {
        let db = Db::open(ckp_child_cfg()).unwrap();
        ckp_workload(&db);
        db.flush().unwrap(); // parks after the verified checkpoint, before pruning
        unreachable!("the parent kills the parked child");
    }
    let d = dir("ckp004-after-ckp");
    let mut child = spawn_ckp_child(
        "ckp004_window_after_checkpoint",
        &d,
        CKP_ENV,
        "after_checkpoint",
    );
    wait_for(&d.join("after_checkpoint"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    assert_eq!(checkpoint_gens(&d), vec![2], "the checkpoint is durable");
    assert!(
        !stem_gens(&d, "IDENTITY-").is_empty(),
        "the delta history was not pruned yet — both sources present"
    );
    let db = Db::open(Config::new(d.clone())).unwrap();
    verify_ckp_workload(&db); // checkpoint + leftover logs converge
}

#[test]
fn ckp004_window_after_first_prune() {
    if std::env::var_os(CHILD_ENV).is_some() {
        let db = Db::open(ckp_child_cfg()).unwrap();
        ckp_workload(&db);
        db.flush().unwrap(); // parks after the first pruned file
        unreachable!("the parent kills the parked child");
    }
    let d = dir("ckp004-first-prune");
    let mut child = spawn_ckp_child(
        "ckp004_window_after_first_prune",
        &d,
        CKP_ENV,
        "after_first_prune",
    );
    wait_for(&d.join("after_first_prune"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    assert_eq!(checkpoint_gens(&d), vec![2], "the checkpoint is durable");
    let db = Db::open(Config::new(d.clone())).unwrap();
    // A partial prune is why fallback would be unsound — but recovery
    // never needs it: the checkpoint covers gen 2 either way.
    verify_ckp_workload(&db);
}

#[test]
fn ckp004_window_after_prune() {
    if std::env::var_os(CHILD_ENV).is_some() {
        let db = Db::open(ckp_child_cfg()).unwrap();
        ckp_workload(&db);
        db.flush().unwrap(); // parks after the prune completed
        unreachable!("the parent kills the parked child");
    }
    let d = dir("ckp004-after-prune");
    let mut child = spawn_ckp_child("ckp004_window_after_prune", &d, CKP_ENV, "after_prune");
    wait_for(&d.join("after_prune"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    assert_eq!(checkpoint_gens(&d), vec![2], "the checkpoint is durable");
    assert_eq!(
        log_file_count(&d),
        0,
        "the checkpoint alone now covers every pruned generation"
    );
    let db = Db::open(Config::new(d.clone())).unwrap();
    verify_ckp_workload(&db);
}

// ---------------------------------------------------------------------------
// ckp005 — pruning growth pins and the budget resume across reopen
// ---------------------------------------------------------------------------

fn ckp005_workload(db: &Db) {
    for round in 0..4 {
        for i in 0..5u8 {
            let b = round * 5 + i + 1;
            db.put_object(oid(b), b"k", &[b]).unwrap();
        }
        db.flush().unwrap();
    }
}

#[test]
fn ckp005_pruning_and_budget_resume() {
    // Arm A — checkpoint_bytes=0: the pre-M40 pinned regime (full replay
    // forever, 12 delta logs after 4 flushes).
    let da = dir("ckp005-a");
    let mut cfg_a = Config::new(da.clone());
    cfg_a.checkpoint_bytes = 0;
    cfg_a.l0_compact_trigger = 0; // the write path's auto-compact would fire its own checkpoint
    {
        let db = Db::open(cfg_a.clone()).unwrap();
        ckp005_workload(&db);
    }
    assert!(checkpoint_gens(&da).is_empty(), "0 disables checkpoints");
    assert_eq!(log_file_count(&da), 12, "every flush left its 3 delta logs");

    // Arm B — the same workload with a 256 B trigger: every flush fires a
    // checkpoint that prunes the very logs that triggered it.
    let dbp = dir("ckp005-b");
    let mut cfg_b = Config::new(dbp.clone());
    cfg_b.checkpoint_bytes = 256;
    cfg_b.l0_compact_trigger = 0;
    {
        let db = Db::open(cfg_b.clone()).unwrap();
        ckp005_workload(&db);
    }
    // The four flushes publish gens 2..=5 (gen 1 = the bootstrap manifest).
    assert_eq!(
        checkpoint_gens(&dbp),
        vec![5],
        "one checkpoint — the newest"
    );
    assert_eq!(log_file_count(&dbp), 0, "the subsumed history is pruned");

    // Reopen: the budget resumes from the logs after the checkpoint (0) —
    // a small flush stays under the trigger.
    let db = Db::open(cfg_b.clone()).unwrap();
    for b in 0x01u8..=0x14 {
        assert_eq!(db.get_object(oid(b), b"k").unwrap(), Some(vec![b]));
    }
    db.put_object(oid(0x21), b"k", b"v").unwrap();
    db.flush().unwrap();
    assert_eq!(
        checkpoint_gens(&dbp),
        vec![5],
        "one small flush (below the trigger) does not checkpoint"
    );
    assert_eq!(stem_gens(&dbp, "IDENTITY-"), vec![6]);

    // A bigger flush crosses the resumed budget — a new checkpoint
    // replaces the old and re-prunes.
    for b in 0x22u8..=0x29 {
        db.put_object(oid(b), b"k", b"v").unwrap();
    }
    db.flush().unwrap();
    assert_eq!(
        checkpoint_gens(&dbp),
        vec![7],
        "the new checkpoint replaced the old one"
    );
    assert_eq!(
        log_file_count(&dbp),
        0,
        "re-pruned after the new checkpoint"
    );
    drop(db);

    let db = Db::open(cfg_b).unwrap();
    for b in 0x01u8..=0x14 {
        assert_eq!(db.get_object(oid(b), b"k").unwrap(), Some(vec![b]));
    }
    for b in 0x21u8..=0x29 {
        assert_eq!(db.get_object(oid(b), b"k").unwrap(), Some(b"v".to_vec()));
    }
}

// ---------------------------------------------------------------------------
// ckp006 — the generation allocator never reuses orphaned generations
// (review INV-05, Challenge C: the state-C compaction window's relocation
// records do NOT ride the WAL, so the recovered map never sees them)
// ---------------------------------------------------------------------------

#[test]
fn ckp006_orphan_pgen_allocator_never_reuses() {
    if std::env::var_os(CHILD_ENV).is_some() {
        let mut cfg = Config::new(child_dir());
        cfg.checkpoint_bytes = 0;
        cfg.l0_compact_trigger = 0;
        let db = Db::open(cfg).unwrap();
        for b in 0x01u8..=0x0A {
            let a = oid(b);
            db.put_object(a, b"k1", &[b, 1]).unwrap();
            db.put_object(a, b"k2", &[b, 2]).unwrap();
            if b % 5 == 0 {
                db.flush().unwrap();
            }
        }
        db.compact().unwrap(); // parks after the relocation log's publish
        unreachable!("the parent kills the parked child");
    }
    let d = dir("ckp006");
    let mut child = spawn_ckp_child(
        "ckp006_orphan_pgen_allocator_never_reuses",
        &d,
        COMPACT_ENV,
        "after_location",
    );
    wait_for(&d.join("after_location"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    // The §24 state-C window: the relocation log is an orphan (published
    // past CURRENT) whose durably-published generations the recovered map
    // never sees.
    let current = Current::read(&d.join("CURRENT")).unwrap();
    let orphans = orphan_placement_logs(&d, current.manifest_generation);
    assert_eq!(orphans.len(), 1, "exactly the relocation log is orphaned");
    let orphan_max = orphan_placement_max_generation(&d, current.manifest_generation);
    assert!(orphan_max > 0, "the orphan carries generations");

    let mut cfg = Config::new(d.clone());
    cfg.l0_compact_trigger = 0;
    let db = Db::open(cfg.clone()).unwrap();
    let map_max = (0x01u8..=0x0A)
        .map(|b| placement_of(&db, rid_of(&db, oid(b))).unwrap().generation())
        .max()
        .unwrap();
    assert!(
        orphan_max > map_max,
        "the orphan holds generations the recovered map never saw"
    );
    for b in 0x01u8..=0x0A {
        let a = oid(b);
        assert_eq!(db.get_object(a, b"k1").unwrap(), Some(vec![b, 1]));
        assert_eq!(db.get_object(a, b"k2").unwrap(), Some(vec![b, 2]));
        assert!(placement_of(&db, rid_of(&db, a)).is_some());
    }

    // The second compaction relocates again — every fresh generation must
    // sit ABOVE everything the orphan durably published (INV-05).
    db.compact().unwrap();
    drop(db);
    let current = Current::read(&d.join("CURRENT")).unwrap();
    let logs = load_placement_logs(&d, current.manifest_generation).unwrap();
    let relocations = logs
        .iter()
        .find(|l| l.generation == current.manifest_generation)
        .expect("the relocation log rides the new manifest generation");
    assert!(!relocations.records.is_empty());
    for rec in &relocations.records {
        assert!(
            rec.placement.generation() > orphan_max,
            "new placement generation {} reuses generation space the orphan published",
            rec.placement.generation()
        );
    }
}

// ---------------------------------------------------------------------------
// ckp007 — the randomized restart oracle: puts/deletes/flushes/compactions/
// restarts against a live oracle, with a checkpoint trigger small enough to
// fire constantly
// ---------------------------------------------------------------------------

struct Xs(u64);

impl Xs {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn verify_oracle(
    db: &Db,
    oracle: &HashMap<(u8, Vec<u8>), Vec<u8>>,
    touched: &HashSet<(u8, Vec<u8>)>,
    created: &HashSet<u8>,
) {
    for &o in created {
        assert!(
            db.resolve_object(oid(o)).is_some(),
            "created objects resolve"
        );
    }
    for ((o, k), v) in oracle {
        assert_eq!(db.get_object(oid(*o), k).unwrap(), Some(v.clone()));
    }
    for (o, k) in touched {
        if !oracle.contains_key(&(*o, k.clone())) {
            assert_eq!(
                db.get_object(oid(*o), k).unwrap(),
                None,
                "deleted keys stay deleted through restart"
            );
        }
    }
}

#[test]
fn ckp007_randomized_restart_oracle() {
    let mut cfg = Config::new(dir("ckp007"));
    cfg.checkpoint_bytes = 512; // fires constantly — every flush is ≥ 40 ops
    cfg.l0_compact_trigger = 0;
    let mut xs = Xs(0x9E37_79B9_7F4A_7C15);
    let mut oracle: HashMap<(u8, Vec<u8>), Vec<u8>> = HashMap::new();
    let mut touched: HashSet<(u8, Vec<u8>)> = HashSet::new();
    let mut created: HashSet<u8> = HashSet::new();
    let mut db = Db::open(cfg.clone()).unwrap();

    for op in 0..800u64 {
        let o = (xs.next() % 40) as u8 + 1;
        let key = format!("k{}", xs.next() % 5);
        let k = key.as_bytes().to_vec();
        match xs.next() % 10 {
            0..=6 => {
                let v: Vec<u8> = (0..12).map(|_| xs.next() as u8).collect();
                db.put_object(oid(o), &k, &v).unwrap();
                oracle.insert((o, k.clone()), v);
                touched.insert((o, k));
                created.insert(o);
            }
            7 | 8 => {
                if created.contains(&o) {
                    db.delete_object(oid(o), &k).unwrap();
                    oracle.remove(&(o, k.clone()));
                    touched.insert((o, k));
                }
            }
            _ => {
                let _ = db.get_object(oid(o), &k).unwrap();
            }
        }
        if op % 40 == 39 {
            db.flush().unwrap();
        }
        if op % 200 == 199 {
            db.compact().unwrap();
        }
        if op % 150 == 149 {
            drop(db);
            db = Db::open(cfg.clone()).unwrap();
            verify_oracle(&db, &oracle, &touched, &created);
        }
    }
    drop(db);
    let db = Db::open(cfg).unwrap();
    verify_oracle(&db, &oracle, &touched, &created);
}

// ---------------------------------------------------------------------------
// ckp008 — the M12 growth gate, strict opt-in: `SE2M40_NIGHTLY=1` (any
// other value fails; unset skips). Two arms over the same workload —
// checkpoint_bytes=0 (the pre-M40 regime) vs 2 MiB — at 100K/300K/600K
// directory updates, reporting the on-disk metadata bytes and the warm
// reopen wall. Writes artifacts/storage-engine-v2/directory-checkpoint.md.
// ---------------------------------------------------------------------------

fn directory_footprint(d: &Path) -> (u64, u64, usize, usize) {
    let mut meta: u64 = 0;
    let mut ckp_bytes: u64 = 0;
    let mut logs = 0usize;
    let mut ckps = 0usize;
    for e in std::fs::read_dir(d).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let len = e.metadata().map(|m| m.len()).unwrap_or(0);
        if ["IDENTITY-", "REPLICA-", "PLACEMENT-"]
            .iter()
            .any(|stem| stem_gen(&name, stem).is_some())
        {
            meta += len;
            logs += 1;
        } else if checkpoint_generation(&name).is_some() {
            meta += len;
            ckp_bytes += len;
            ckps += 1;
        }
    }
    (meta, ckp_bytes, logs, ckps)
}

#[test]
fn ckp008_growth_probe() {
    let Some(nightly) = std::env::var_os("SE2M40_NIGHTLY") else {
        return;
    };
    assert_eq!(
        nightly, "1",
        "SE2M40_NIGHTLY strict opt-in: unset (skip) or exactly \"1\", got {nightly:?}"
    );

    const SCALES: [usize; 3] = [100_000, 300_000, 600_000];
    const OBJECTS: usize = 10_000;
    const FLUSH_EVERY: usize = 2_000;
    // (updates, arm label, build s, meta bytes, ckp bytes, log count, ckp count, warm open ms)
    type Row = (usize, &'static str, f64, u64, u64, usize, usize, f64);
    let mut rows: Vec<Row> = Vec::new();

    for &updates in &SCALES {
        for (label, ckp_bytes) in [("off", 0usize), ("2 MiB", 2 * 1024 * 1024)] {
            let d = dir(&format!("ckp008-{}-{label}", updates));
            let mut cfg = Config::new(d.clone());
            cfg.checkpoint_bytes = ckp_bytes;
            cfg.durability = DurabilityMode::Async; // durability is not under test
            cfg.l0_compact_trigger = 0; // the probe measures the DIRECTORY, not compaction
            let samples: Vec<usize> = (0..updates).step_by(10_000).collect();

            let t0 = Instant::now();
            {
                let db = Db::open(cfg.clone()).unwrap();
                for i in 0..OBJECTS {
                    db.put_object(obj_id(i), b"seed", &(i as u64).to_le_bytes())
                        .unwrap();
                }
                db.flush().unwrap();
                for n in 0..updates {
                    let a = obj_id(n % OBJECTS);
                    let key = format!("k{n:06}");
                    db.put_object(a, key.as_bytes(), &n.to_le_bytes()).unwrap();
                    if n % FLUSH_EVERY == FLUSH_EVERY - 1 {
                        db.flush().unwrap();
                    }
                }
                db.flush().unwrap();
            }
            let build_s = t0.elapsed().as_secs_f64();

            let (meta, ckp_bytes_sum, logs, ckps) = directory_footprint(&d);

            // Warm reopen ×3 (page cache warm; both arms open the same
            // segment set, so the delta is the directory decode).
            let mut opens: Vec<Duration> = Vec::new();
            for _ in 0..3 {
                let t = Instant::now();
                let db = Db::open(cfg.clone()).unwrap();
                opens.push(t.elapsed());
                for i in 0..OBJECTS {
                    assert!(db.resolve_object(obj_id(i)).is_some());
                }
                for &n in &samples {
                    let key = format!("k{n:06}");
                    assert_eq!(
                        db.get_object(obj_id(n % OBJECTS), key.as_bytes()).unwrap(),
                        Some(n.to_le_bytes().to_vec())
                    );
                }
                drop(db);
            }
            let min_open_ms = opens
                .iter()
                .map(|d| d.as_secs_f64() * 1000.0)
                .fold(f64::INFINITY, f64::min);

            rows.push((
                updates,
                label,
                build_s,
                meta,
                ckp_bytes_sum,
                logs,
                ckps,
                min_open_ms,
            ));
        }
    }

    // The M12 gate: recovery must be ∝ checkpoint + recent deltas, not the
    // full metadata history. The off-arm's open reads every placement log
    // ever published; the checkpoint arm's open reads the live state.
    let mut out = String::new();
    out.push_str("# Directory growth certification (SE2-M40)\n\n");
    out.push_str(&format!("- date: {}\n", run_date()));
    out.push_str("- harness: `ckp008_growth_probe` with `SE2M40_NIGHTLY=1`\n");
    out.push_str("- workload per scale: 10,000 objects seeded (one key each), then\n");
    out.push_str("  N updates (a new key per update, round-robin over the objects),\n");
    out.push_str("  flush every 2,000 updates, Async durability; `checkpoint_bytes`\n");
    out.push_str("  = 0 (the pre-M40 regime) vs 2 MiB\n");
    out.push_str("- measured per arm: build wall, directory metadata bytes on disk\n");
    out.push_str("  (delta logs + checkpoints; segments/manifest/WAL excluded — identical\n");
    out.push_str("  across arms), and warm reopen wall (min of 3, page cache warm)\n\n");
    out.push_str("| updates | checkpoint | build s | metadata bytes | checkpoint bytes | log files | warm open ms |\n");
    out.push_str("|---|---|---|---|---|---|---|\n");
    for (updates, label, build_s, meta, ckp_bytes_sum, logs, ckps, min_open_ms) in &rows {
        out.push_str(&format!(
            "| {updates} | {label} | {build_s:.1} | {meta} | {ckp_bytes_sum} | {logs} | {min_open_ms:.1} |\n",
        ));
        let _ = ckps;
    }
    out.push_str("\n## The M12 gate\n\n");
    out.push_str("Recovery must be proportional to the checkpoint plus the deltas AFTER\n");
    out.push_str("it, never to the full metadata history. The rows above show the\n");
    out.push_str("off-arm's open climbing with the update count (every placement log\n");
    out.push_str("ever published is decoded) while the checkpoint arm's open stays\n");
    out.push_str("flat at the live-state size (~10K identity/replica records + the\n");
    out.push_str("trigger window of placement records).\n");

    let artifacts =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../artifacts/storage-engine-v2");
    std::fs::create_dir_all(&artifacts).unwrap();
    std::fs::write(artifacts.join("directory-checkpoint.md"), out).unwrap();
}
