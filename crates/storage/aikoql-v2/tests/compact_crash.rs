//! SE2-M4 — §25 crash matrix at the compaction publication stages
//! (child-kill harness, KSE-15 pattern): kill after the new segment lands
//! but before the manifest, after the manifest but before CURRENT, and
//! after CURRENT but before obsolete-file deletion. Compaction is
//! state-preserving, so every window must recover the SAME logical state —
//! no acked write lost, no phantom commit, no duplicate state.

mod common;

use aikoql_storage_v2::db::{manifest_path, Config, Db};
use aikoql_storage_v2::format::{Current, Manifest};
use common::dir;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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

/// xorshift64 — the child and the parent must derive the same workload.
fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut s = seed;
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    }
}

/// The deterministic workload: 120 single-op batches over 40 hot keys.
fn ops() -> Vec<(bool, Vec<u8>, Vec<u8>)> {
    let mut next = rng(7);
    (0..120)
        .map(|i| {
            let k = format!("k{:03}", next() % 40).into_bytes();
            match next() % 10 {
                0..=6 => (true, k, format!("v{i:03}").into_bytes()),
                _ => (false, k, Vec::new()),
            }
        })
        .collect()
}

fn run_workload_then_compact(db: &Db) {
    for (put, k, v) in ops() {
        if put {
            db.put(&k, &v).unwrap();
        } else {
            db.delete(&k).unwrap();
        }
    }
    db.flush().unwrap();
    db.compact().unwrap(); // parks at the env-named stage
}

fn expected() -> HashMap<Vec<u8>, Option<Vec<u8>>> {
    let mut m = HashMap::new();
    for (put, k, v) in ops() {
        m.insert(k, if put { Some(v) } else { None });
    }
    m
}

fn segment_file_count(d: &Path) -> usize {
    std::fs::read_dir(d)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("SEGMENT-"))
        .count()
}

/// Reopen after the kill: every key holds its expected value, nothing was
/// acked that is lost, nothing phantom — and the sequence resumes at 121.
fn verify(d: &Path) {
    let db = Db::open(Config::new(d.to_path_buf())).unwrap();
    for (k, want) in &expected() {
        assert_eq!(
            db.get(k).unwrap(),
            *want,
            "key {:?} diverged",
            String::from_utf8_lossy(k)
        );
    }
    for probe in 0..30u64 {
        let k = format!("z{probe:03}").into_bytes();
        assert_eq!(db.get(&k).unwrap(), None, "phantom key {probe}");
    }
    assert_eq!(
        db.put(b"next", b"x").unwrap(),
        121,
        "no acked write lost, no phantom commit"
    );
}

#[test]
fn compact_crash_after_segment_before_manifest() {
    // dir() AFTER the child branch: the child would otherwise create its own
    // pid-namespaced dir at this line and, being hard-killed, never sweep it.
    if std::env::var_os(CHILD_ENV).is_some() {
        let mut cfg = Config::new(child_dir());
        cfg.memtable_bytes = 512;
        // SE2-M10: these children pin the EXPLICIT compact's windows — an
        // auto-triggered compact would park at the env stage mid-workload.
        cfg.l0_compact_trigger = 0;
        let db = Db::open(cfg).unwrap();
        run_workload_then_compact(&db);
        unreachable!("the parent kills the parked child");
    }
    let d = dir("compact-crash-seg");
    let mut child = spawn_child(
        "compact_crash_after_segment_before_manifest",
        &d,
        "after_segment",
    );
    wait_for(&d.join("after_segment"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    // The old manifest still governs; the parked L1 output is the one
    // orphan segment on disk.
    let current = Current::read(&d.join("CURRENT")).unwrap();
    let manifest = Manifest::read(&manifest_path(&d, current.manifest_generation)).unwrap();
    assert!(
        !manifest.segments.is_empty(),
        "pre-compact manifest still governs"
    );
    assert!(
        manifest.segments.iter().all(|s| s.level == 0),
        "no L1 record published yet"
    );
    assert_eq!(
        segment_file_count(&d),
        manifest.segments.len() + 1,
        "the parked L1 output is the one orphan"
    );
    verify(&d);
}

#[test]
fn compact_crash_after_manifest_before_current() {
    // dir() AFTER the child branch (see ..._after_segment_before_manifest).
    if std::env::var_os(CHILD_ENV).is_some() {
        let mut cfg = Config::new(child_dir());
        cfg.memtable_bytes = 512;
        // SE2-M10: these children pin the EXPLICIT compact's windows — an
        // auto-triggered compact would park at the env stage mid-workload.
        cfg.l0_compact_trigger = 0;
        let db = Db::open(cfg).unwrap();
        run_workload_then_compact(&db);
        unreachable!("the parent kills the parked child");
    }
    let d = dir("compact-crash-manifest");
    let mut child = spawn_child(
        "compact_crash_after_manifest_before_current",
        &d,
        "after_manifest",
    );
    wait_for(&d.join("after_manifest"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    // CURRENT still points at the old generation; the new manifest file
    // sits unreferenced on disk — reopen must follow CURRENT, never pick.
    let current = Current::read(&d.join("CURRENT")).unwrap();
    let manifest = Manifest::read(&manifest_path(&d, current.manifest_generation)).unwrap();
    assert!(
        manifest.segments.iter().all(|s| s.level == 0),
        "the current manifest is the pre-compaction one"
    );
    assert!(
        manifest_path(&d, current.manifest_generation + 1).exists(),
        "the new manifest is published but not yet current"
    );
    verify(&d);
}

#[test]
fn compact_crash_after_current_before_deletion() {
    // dir() AFTER the child branch (see ..._after_segment_before_manifest).
    if std::env::var_os(CHILD_ENV).is_some() {
        let mut cfg = Config::new(child_dir());
        cfg.memtable_bytes = 512;
        // SE2-M10: these children pin the EXPLICIT compact's windows — an
        // auto-triggered compact would park at the env stage mid-workload.
        cfg.l0_compact_trigger = 0;
        let db = Db::open(cfg).unwrap();
        run_workload_then_compact(&db);
        unreachable!("the parent kills the parked child");
    }
    let d = dir("compact-crash-current");
    let mut child = spawn_child(
        "compact_crash_after_current_before_deletion",
        &d,
        "after_current",
    );
    wait_for(&d.join("after_current"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    // The new state governs: a single L1 segment holding the live keys;
    // the obsolete L0 files are still on disk (kill beat the deletion) and
    // reopen must report-and-ignore them.
    let current = Current::read(&d.join("CURRENT")).unwrap();
    let manifest = Manifest::read(&manifest_path(&d, current.manifest_generation)).unwrap();
    assert_eq!(
        manifest.segments.len(),
        1,
        "compaction output is the single L1 segment"
    );
    assert_eq!(manifest.segments[0].level, 1);
    let live = expected().values().filter(|v| v.is_some()).count() as u64;
    assert_eq!(manifest.segments[0].record_count, live);
    assert!(
        segment_file_count(&d) >= 2,
        "obsolete L0 files not yet deleted"
    );
    verify(&d);
}
