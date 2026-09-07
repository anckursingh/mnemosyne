//! SE2-M39 §41/§42 — randomized oracle certification (spec milestone 11).
//!
//! Random PUT/UPDATE/DELETE/FLUSH/COMPACT/RESTART/CRASH/RECOVER operations
//! against an independent `BTreeMap<ObjectId, Expected>` oracle. The oracle
//! builds its expectations from the operation stream alone — it never reads
//! engine state to form an expectation (§42: no AIKOQL internal abstraction
//! validates AIKOQL). The one observation it records is each object's
//! LogicalId at creation, via the public `resolve_object`; it then asserts
//! that identity is stable forever. Value, version (newest-wins, durability,
//! tombstone shadowing) and existence are decided by the oracle's own state.
//!
//! Always-on: 600 ops, restarts at fixed points, no crash. Nightly
//! (`SE2M39_NIGHTLY=1`): 20k ops, restarts at fixed points, 3 crash windows
//! — a child process replays a deterministic prefix on a fresh dir, acks a
//! marker, parks; the parent hard-kills it, reopens, and verifies the oracle
//! through the acked op (the SE2 kill harness, KSE-15).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use aikoql_storage_v2::db::{Config, Db};
use aikoql_storage_v2::identity::topology::{LocalReplicaDirectory, ReplicaDirectory};
use aikoql_storage_v2::identity::{LogicalId, ObjectId};
use aikoql_storage_v2::placement::directory::{LocalPlacementResolver, PlacementResolver};

mod common;
use common::dir;

const NIGHTLY: &str = "SE2M39_NIGHTLY";
const CHILD: &str = "AIKOQL_V2_ORACLE_CHILD";
const CHILD_DIR: &str = "AIKOQL_V2_ORACLE_DIR";
const CHILD_SEED: &str = "AIKOQL_V2_ORACLE_SEED";
const CHILD_OPS: &str = "AIKOQL_V2_ORACLE_OPS";

/// xorshift64 — deterministic and self-contained. The crash children must
/// generate the identical operation stream, so every draw is a pure
/// function of (seed, step index) — never of engine output.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// The oracle's expectation for one object — decided solely by the stream.
#[derive(Default)]
struct Expected {
    /// One row per key this object has written (None = tombstoned there).
    rows: BTreeMap<Vec<u8>, (Option<Vec<u8>>, u64)>,
    /// The key of the object's newest write — the §16 delete target.
    last_key: Vec<u8>,
    /// Oracle-side write counter — the observable "version" contract:
    /// newest-wins on read, no resurrection across restart/compact, and a
    /// tombstone keeps shadowing older rows.
    version: u64,
    /// Observed once at creation (public API); asserted stable forever.
    logical: Option<LogicalId>,
}

#[derive(Default)]
struct Oracle {
    map: BTreeMap<ObjectId, Expected>,
}

/// One planned operation. `plan` is pure — engine calls happen only in
/// `execute`, so the crash-window verifier can rebuild the oracle's
/// expected state without touching an engine.
enum Plan {
    Put(ObjectId, Vec<u8>, Vec<u8>),
    Delete(ObjectId, Vec<u8>),
    Flush,
    Compact,
    Restart,
    Sample,
}

/// The pure half: evolve the oracle for one step and produce the concrete
/// engine operation. Restarts ride at fixed points (every `ops / 4` steps)
/// so the smoke and nightly arms both cross restart boundaries.
fn plan(oracle: &mut Oracle, seed: u64, i: usize, ops: usize) -> Plan {
    if i % (ops / 4).max(1) == (ops / 4).max(1) - 1 {
        return Plan::Restart;
    }
    let mut r = Rng(seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    match r.below(100) {
        0..=47 => {
            // PUT: mostly fresh oids and hot keys; 1/5 hit an existing oid
            // (newest-wins), 1/16 write a unique key.
            let oid = if r.below(5) != 0 || oracle.map.is_empty() {
                let mut b = [0u8; 16];
                b[..8].copy_from_slice(&r.next().to_le_bytes());
                b[8..].copy_from_slice(&r.next().to_le_bytes());
                ObjectId(b)
            } else {
                *oracle
                    .map
                    .keys()
                    .nth(r.below(oracle.map.len() as u64) as usize)
                    .unwrap()
            };
            let key = if r.below(16) == 0 {
                format!("u{}-{}", i, r.next()).into_bytes()
            } else {
                format!("k{}", r.below(8)).into_bytes()
            };
            let len = r.below(65) as usize;
            let mut value = Vec::with_capacity(len);
            for _ in 0..len {
                value.push(r.below(256) as u8);
            }
            let entry = oracle.map.entry(oid).or_default();
            entry
                .rows
                .insert(key.clone(), (Some(value.clone()), entry.version + 1));
            entry.version += 1;
            entry.last_key = key.clone();
            Plan::Put(oid, key, value)
        }
        48..=72 => {
            // UPDATE: a known oid, hot or unique key, fresh value.
            if oracle.map.is_empty() {
                return Plan::Sample;
            }
            let oid = *oracle
                .map
                .keys()
                .nth(r.below(oracle.map.len() as u64) as usize)
                .unwrap();
            let key = if r.below(16) == 0 {
                format!("u{}-{}", i, r.next()).into_bytes()
            } else {
                format!("k{}", r.below(8)).into_bytes()
            };
            let len = r.below(65) as usize;
            let mut value = Vec::with_capacity(len);
            for _ in 0..len {
                value.push(r.below(256) as u8);
            }
            let entry = oracle.map.get_mut(&oid).unwrap();
            entry
                .rows
                .insert(key.clone(), (Some(value.clone()), entry.version + 1));
            entry.version += 1;
            entry.last_key = key.clone();
            Plan::Put(oid, key, value)
        }
        73..=82 => {
            // DELETE: tombstone the object's newest row.
            if oracle.map.is_empty() {
                return Plan::Sample;
            }
            let oid = *oracle
                .map
                .keys()
                .nth(r.below(oracle.map.len() as u64) as usize)
                .unwrap();
            let key = oracle.map.get(&oid).unwrap().last_key.clone();
            let entry = oracle.map.get_mut(&oid).unwrap();
            entry.rows.insert(key.clone(), (None, entry.version + 1));
            entry.version += 1;
            Plan::Delete(oid, key)
        }
        83..=90 => Plan::Flush,
        91..=95 => Plan::Compact,
        _ => Plan::Sample,
    }
}

fn open(d: &Path) -> Db {
    Db::open(Config::new(d.to_path_buf())).unwrap()
}

fn execute(db: &Db, step: &Plan, oracle: &mut Oracle) {
    match step {
        Plan::Put(oid, key, value) => {
            db.put_object(*oid, key, value).unwrap();
            // Record the observed identity at creation (§42: observation,
            // not expectation — the stability assert is the validation).
            if oracle.map[oid].logical.is_none() {
                let lid = db.resolve_object(*oid).expect("created object resolves");
                oracle.map.get_mut(oid).unwrap().logical = Some(lid);
            }
        }
        Plan::Delete(oid, key) => {
            db.delete_object(*oid, key).unwrap();
        }
        Plan::Flush => db.flush().unwrap(),
        Plan::Compact => {
            db.compact().unwrap();
        }
        Plan::Restart | Plan::Sample => {}
    }
}

/// One random row verified in place — catches divergence without a restart
/// boundary.
fn sample_verify(db: &Db, oracle: &Oracle, seed: u64, i: usize) {
    if oracle.map.is_empty() {
        return;
    }
    let mut r = Rng(seed ^ (i as u64).wrapping_add(0x51_7C_C1_B7_27_22_0A));
    let (oid, exp) = oracle
        .map
        .iter()
        .nth(r.below(oracle.map.len() as u64) as usize)
        .unwrap();
    let (key, (value, _)) = exp
        .rows
        .iter()
        .nth(r.below(exp.rows.len() as u64) as usize)
        .unwrap();
    assert_eq!(
        db.get_object(*oid, key).unwrap().as_deref(),
        value.as_deref(),
        "in-place sample diverged at step {i}"
    );
}

/// Full validation of §41's four axes. Existence negatives: random oids
/// outside the stream answer None.
fn verify(db: &Db, oracle: &Oracle, seed: u64, at: &str) {
    let mut observed: Vec<LogicalId> = Vec::new();
    for (oid, exp) in &oracle.map {
        for (key, (value, _)) in &exp.rows {
            let got = db.get_object(*oid, key).unwrap();
            if got.as_deref() != value.as_deref() {
                let lid = db.resolve_object(*oid);
                let rid =
                    lid.and_then(|l| LocalReplicaDirectory::new(db).resolve_local(l).unwrap());
                let placement =
                    rid.and_then(|r| LocalPlacementResolver::new(db).resolve(r).unwrap());
                panic!(
                    "value/version divergence at {at}: {oid:?} key {key:?}\n  \
                     expected {value:?}\n  got {got:?}\n  \
                     lid {lid:?} rid {rid:?} placement {placement:?}\n  \
                     oracle rows: {:?}",
                    exp.rows
                        .iter()
                        .map(|(k, (v, ver))| {
                            (String::from_utf8_lossy(k), v.as_ref().map(|v| v.len()), ver)
                        })
                        .collect::<Vec<_>>()
                );
            }
        }
        match exp.logical {
            // Observed at creation in this process: identity is stable.
            Some(lid) => {
                assert_eq!(
                    db.resolve_object(*oid),
                    Some(lid),
                    "identity diverged for {oid:?}"
                );
                observed.push(lid);
            }
            // Crash-window rebuild: the observation died with the child —
            // existence + uniqueness still pin the directory.
            None => {
                assert!(
                    db.resolve_object(*oid).is_some(),
                    "created {oid:?} no longer resolves"
                );
            }
        }
    }
    observed.sort();
    for w in observed.windows(2) {
        assert_ne!(w[0], w[1], "logical id reused across objects");
    }
    let mut r = Rng(seed ^ 0xDEAD_BEEF);
    for _ in 0..25 {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&r.next().to_le_bytes());
        b[8..].copy_from_slice(&r.next().to_le_bytes());
        let oid = ObjectId(b);
        if oracle.map.contains_key(&oid) {
            continue;
        }
        assert_eq!(db.get_object(oid, b"k0").unwrap(), None);
        assert_eq!(db.resolve_object(oid), None);
    }
}

fn run_stream(d: &Path, seed: u64, ops: usize) {
    let mut db = open(d);
    let mut oracle = Oracle::default();
    for i in 0..ops {
        let step = plan(&mut oracle, seed, i, ops);
        execute(&db, &step, &mut oracle);
        match step {
            Plan::Restart => {
                drop(db);
                db = open(d);
                verify(&db, &oracle, seed, &format!("restart at {i}"));
            }
            Plan::Sample => sample_verify(&db, &oracle, seed, i),
            _ => {}
        }
    }
    verify(&db, &oracle, seed, "final");
}

fn kill_child(child: &mut Child) {
    // Std kill(): TerminateProcess on Windows, SIGKILL on Unix.
    child.kill().unwrap();
}

/// CRASH/RECOVER (nightly): a child replays the deterministic prefix of the
/// stream on a fresh dir and parks after acking op `ops`; the parent
/// hard-kills it, reopens, and verifies the oracle through the acked op.
/// The marker lives next to (not inside) the engine dir.
fn crash_window(d: &Path, seed: u64, ops: usize) {
    let marker = PathBuf::from(format!("{}.acked-{ops}", d.display()));
    let _ = std::fs::remove_file(&marker);
    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(exe)
        .arg("--exact")
        .arg("oracle_child")
        .arg("--nocapture")
        .env(CHILD, "1")
        .env(CHILD_DIR, d)
        .env(CHILD_SEED, seed.to_string())
        .env(CHILD_OPS, ops.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(600);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "child never acked op {ops}");
        std::thread::sleep(Duration::from_millis(100));
    }
    kill_child(&mut child);
    let _ = child.wait();
    let db = open(d);
    // Rebuild the oracle's expectation purely (plan only, no engine) and
    // verify the recovered engine through the acked prefix — twice: once on
    // the recovered state, once across a clean reopen (identity stability
    // across the crash boundary AND restart).
    let mut oracle = Oracle::default();
    for i in 0..ops {
        let _ = plan(&mut oracle, seed, i, ops);
    }
    verify(&db, &oracle, seed, "crash recover");
    drop(db);
    let db = open(d);
    verify(&db, &oracle, seed, "crash reopen");
}

#[test]
fn oracle_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let d = PathBuf::from(std::env::var(CHILD_DIR).unwrap());
    let seed: u64 = std::env::var(CHILD_SEED).unwrap().parse().unwrap();
    let ops: usize = std::env::var(CHILD_OPS).unwrap().parse().unwrap();
    let db = open(&d);
    let mut oracle = Oracle::default();
    for i in 0..ops {
        let step = plan(&mut oracle, seed, i, ops);
        execute(&db, &step, &mut oracle);
    }
    // Ack marker: the engine's Sync ack for op `ops` IS durable, so the
    // marker appearing after it means the parent may kill us — then park
    // until the kill (with a generous self-exit against a leaked child).
    let marker =
        std::fs::File::create(PathBuf::from(format!("{}.acked-{ops}", d.display()))).unwrap();
    marker.sync_all().unwrap();
    let deadline = Instant::now() + Duration::from_secs(1200);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(60));
    }
}

#[test]
fn or001_randomized_oracle() {
    let seed: u64 = 0x3A6B_2C9D_E4F1_0875;
    let ops = if std::env::var_os(NIGHTLY).is_some() {
        20_000
    } else {
        600
    };
    let d = dir("oracle");
    run_stream(&d, seed, ops);
    if std::env::var_os(NIGHTLY).is_some() {
        for (i, k) in [2_000usize, 8_000, 15_000].iter().enumerate() {
            let d = dir(&format!("oracle-crash-{i}"));
            crash_window(&d, seed, *k);
        }
    }
    println!("or001 PASS — {ops} ops, oracle rows verified value/identity/version/existence");
}
