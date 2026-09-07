//! SE2-M6 — group-commit crash windows (child-kill harness, KSE-15
//! pattern): kill the committer after the group fsync but before the apply,
//! after the apply but before the acks, and after the acks (writer-side
//! marker — the writer received its acks before the kill). Every window
//! must recover every submitted batch with contiguous seqs — no
//! acknowledged commit lost, no phantom, no gap.

mod common;

use aikoql_storage_v2::db::{Config, Db, DurabilityMode, WAL_FILE};
use aikoql_storage_v2::wal::replay_frames;
use common::dir;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CHILD_ENV: &str = "AIKOQL_V2_GC_CHILD";
const DIR_ENV: &str = "AIKOQL_V2_GC_DIR";
const STAGE_ENV: &str = "AIKOQL_V2_GROUP_PARK";

const BATCHES: u64 = 20;

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

/// Child side: 20 concurrent single-op batches (one thread each, one
/// writer clone) — the 200 ms window groups them ALL into one group, so
/// the park hits the one group commit. Concurrent submitters are required:
/// a synchronous submitter's write blocks on its ack, and in the park
/// stages the acks never come. In the park stages the threads simply block
/// on their acks until the parent kills us; in after_ack every ack is
/// delivered (joins return, `acked` marker written) before the committer
/// parks.
fn run_child() -> ! {
    let mut cfg = Config::new(child_dir());
    cfg.durability = DurabilityMode::GroupCommit;
    cfg.max_wait_duration = Duration::from_millis(200);
    let db = Db::open(cfg).unwrap();
    let writer = db.writer().unwrap();
    let stage = std::env::var(STAGE_ENV).unwrap();
    let threads: Vec<_> = (1..=BATCHES)
        .map(|i| {
            let writer = writer.clone();
            std::thread::spawn(move || {
                writer
                    .write(&[aikoql_storage_v2::wal::Op::Put(
                        format!("k{i:02}").into_bytes(),
                        format!("v{i:02}").into_bytes(),
                    )])
                    .unwrap()
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    if stage == "after_ack" {
        // Every batch was acked — the honest "acknowledged commit"
        // marker for the parent to kill on.
        std::fs::write(child_dir().join("acked"), b"20").unwrap();
    }
    std::thread::park();
    unreachable!("the parent kills the parked child")
}

/// Parent side: reopen after the kill and pin the exact recovered state.
fn verify(d: &Path) {
    let mut cfg = Config::new(d.to_path_buf());
    cfg.durability = DurabilityMode::GroupCommit;
    let db = Db::open(cfg).unwrap();
    for i in 1..=BATCHES {
        assert_eq!(
            db.get(&format!("k{i:02}").into_bytes()).unwrap(),
            Some(format!("v{i:02}").into_bytes()),
            "batch {i} lost"
        );
    }
    // Contiguity: the WAL replays to exactly seqs 1..=20 — a lost group
    // would leave a gap, a phantom commit would extend the range.
    let wal_bytes = std::fs::read(d.join(WAL_FILE)).unwrap();
    let (frames, consumed) = replay_frames(&wal_bytes).unwrap();
    assert_eq!(consumed, wal_bytes.len(), "torn tail survived");
    assert_eq!(frames.len() as u64, BATCHES);
    for (idx, frame) in frames.iter().enumerate() {
        assert_eq!(frame.seq, idx as u64 + 1, "seq gap or phantom");
    }
    // The sequence resumes at 21 — nothing lost, nothing invented.
    drop(db);
    let mut cfg = Config::new(d.to_path_buf());
    cfg.durability = DurabilityMode::GroupCommit;
    let db = Db::open(cfg).unwrap();
    let writer = db.writer().unwrap();
    assert_eq!(
        writer
            .write(&[aikoql_storage_v2::wal::Op::Put(
                b"next".to_vec(),
                b"x".to_vec()
            )])
            .unwrap(),
        BATCHES + 1
    );
}

#[test]
fn group_crash_after_fsync_before_apply() {
    // dir() AFTER the child branch: the child would otherwise create its own
    // pid-namespaced dir at this line and, being hard-killed, never sweep it.
    if std::env::var_os(CHILD_ENV).is_some() {
        run_child();
    }
    let d = dir("gc-crash-fsync");
    let mut child = spawn_child("group_crash_after_fsync_before_apply", &d, "after_fsync");
    wait_for(&d.join("after_fsync"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");
    verify(&d);
}

#[test]
fn group_crash_after_apply_before_ack() {
    // dir() AFTER the child branch (see ..._after_fsync_before_apply).
    if std::env::var_os(CHILD_ENV).is_some() {
        run_child();
    }
    let d = dir("gc-crash-apply");
    let mut child = spawn_child("group_crash_after_apply_before_ack", &d, "after_apply");
    wait_for(&d.join("after_apply"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");
    verify(&d);
}

#[test]
fn group_crash_after_ack_loses_no_acknowledged_commit() {
    // dir() AFTER the child branch (see ..._after_fsync_before_apply).
    if std::env::var_os(CHILD_ENV).is_some() {
        run_child();
    }
    let d = dir("gc-crash-ack");
    let mut child = spawn_child(
        "group_crash_after_ack_loses_no_acknowledged_commit",
        &d,
        "after_ack",
    );
    // The writer-side marker: all 20 acks were DELIVERED before the kill —
    // this window is the "acknowledged commit" one.
    wait_for(&d.join("acked"), Duration::from_secs(60));
    wait_for(&d.join("after_ack"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");
    verify(&d);
}
