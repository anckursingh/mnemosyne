//! SE2-M2 — child-kill recovery (KSE-15 harness pattern): spawn
//! `current_exe --exact <this test>` with env gates; the child branch runs
//! the scenario and parks; the parent kills it hard (TerminateProcess — no
//! Drop, no flush, no close) and asserts recovery from the WAL alone.

mod common;

use aikoql_storage_v2::db::{Config, Db};
use common::dir;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CHILD_ENV: &str = "AIKOQL_V2_KILL_CHILD";
const DIR_ENV: &str = "AIKOQL_V2_KILL_DIR";
const N: u64 = 50;

fn child_dir() -> PathBuf {
    PathBuf::from(std::env::var(DIR_ENV).expect("child dir env"))
}

fn spawn_child(test_name: &str, dir: &Path) -> Child {
    Command::new(std::env::current_exe().expect("current exe"))
        .arg("--exact")
        .arg(test_name)
        .env(CHILD_ENV, "1")
        .env(DIR_ENV, dir)
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

fn key(i: u64) -> Vec<u8> {
    format!("k{i:03}").into_bytes()
}

fn value(i: u64) -> Vec<u8> {
    format!("v{i}").into_bytes()
}

#[test]
fn child_kill_after_park_recovers_exactly() {
    // dir() AFTER the child branch: the child would otherwise create its own
    // pid-namespaced dir at this line and, being hard-killed, never sweep it.
    if std::env::var_os(CHILD_ENV).is_some() {
        let cdir = child_dir();
        let db = Db::open(Config::new(cdir.clone())).unwrap();
        for i in 0..N {
            db.put(&key(i), &value(i)).unwrap();
        }
        std::fs::write(cdir.join("done"), b"1").unwrap();
        loop {
            std::thread::sleep(Duration::from_secs(60)); // killed by the parent
        }
    }
    let d = dir("crash-park");
    let mut child = spawn_child("child_kill_after_park_recovers_exactly", &d);
    wait_for(&d.join("done"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    let db = Db::open(Config::new(d.clone())).unwrap();
    for i in 0..N {
        assert_eq!(
            db.get(&key(i)).unwrap(),
            Some(value(i)),
            "acked write {i} lost"
        );
    }
    assert_eq!(
        db.put(b"next", b"x").unwrap(),
        N + 1,
        "recovered seqs must be exactly 1..={N}"
    );
}

#[test]
fn child_kill_mid_burst_recovers_prefix() {
    // dir() AFTER the child branch (see child_kill_after_park_recovers_exactly).
    if std::env::var_os(CHILD_ENV).is_some() {
        let cdir = child_dir();
        let mut cfg = Config::new(cdir.clone());
        cfg.memtable_bytes = 200; // flushes interleave with the burst
                                  // SE2-M10: the scenario pins flush/WAL recovery — keep the trigger's
                                  // compaction crash windows out of it (compact_crash covers those).
        cfg.l0_compact_trigger = 0;
        let db = Db::open(cfg).unwrap();
        for i in 0..200u64 {
            db.put(&key(i), &value(i)).unwrap();
            if i % 10 == 9 {
                std::fs::write(cdir.join("count"), format!("{}", i + 1)).unwrap();
            }
        }
        loop {
            std::thread::sleep(Duration::from_secs(60)); // killed by the parent
        }
    }
    let d = dir("crash-burst");
    let mut child = spawn_child("child_kill_mid_burst_recovers_prefix", &d);
    let marker = d.join("count");
    let start = Instant::now();
    let mut count = 0u64;
    while count < 30 {
        if let Ok(s) = std::fs::read_to_string(&marker) {
            if let Ok(n) = s.trim().parse::<u64>() {
                count = n;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "child never marked progress"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    let db = Db::open(Config::new(d.clone())).unwrap();
    // Every recovered seq is a contiguous prefix 1..=m — no gap, no loss of
    // an acked write (m >= the count the child reported before the kill).
    // At most the one in-flight unacked batch may have made it.
    let m = db.put(b"next", b"x").unwrap() - 1;
    assert!(
        m >= count,
        "acked writes lost: recovered {m} < reported {count}"
    );
    for i in 0..m {
        assert_eq!(
            db.get(&key(i)).unwrap(),
            Some(value(i)),
            "prefix broken at {i}"
        );
    }
    assert_eq!(
        db.get(&key(m)).unwrap(),
        None,
        "one past the prefix must be absent"
    );
}
