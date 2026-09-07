//! SE2-M0 — atomic publication (docs/TESTING-PLAN-V2.md row V2-M0).
//!
//! Publication protocol: write temp (beside the target, same volume) →
//! fsync → rename over the target. Rename is atomic on POSIX and NTFS, so
//! readers observe either the complete old file or the complete new file.
//! Invariants under test: no stray temp files after a publish, read-back
//! never observes a partial target, and a REAL kill at an arbitrary instant
//! leaves the target either complete-and-parseable or absent — never torn.
//!
//! The kill harness follows the v1 KSE-141 convention: the child is this
//! test binary re-run with `--exact` + env gates; `child.kill()` is
//! TerminateProcess/SIGKILL — no cleanup, no graceful shutdown.

mod common;

use aikoql_storage_v2::format::{Current, FormatError};
use common::dir;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const FORMAT_VERSION: u16 = 1;
const CHILD_ENV: &str = "V2PUB_CHILD";
const PATH_ENV: &str = "V2PUB_PATH";
const MARKER_ENV: &str = "V2PUB_MARKER";

#[test]
fn publish_replaces_and_leaves_no_temp() {
    let d = dir("pub-replace");
    let path = d.join("CURRENT");
    Current::publish(&path, &Current::new(FORMAT_VERSION, 1)).unwrap();
    Current::publish(&path, &Current::new(FORMAT_VERSION, 2)).unwrap();
    assert_eq!(Current::read(&path).unwrap().manifest_generation, 2);
    // The rename consumed the temp file: exactly the target remains.
    assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
}

#[test]
fn publish_never_observably_partial() {
    let d = dir("pub-loop");
    let path = d.join("CURRENT");
    for gen in 0..200u64 {
        Current::publish(&path, &Current::new(FORMAT_VERSION, gen)).unwrap();
        assert_eq!(Current::read(&path).unwrap().manifest_generation, gen);
    }
}

fn child_main() {
    let path = PathBuf::from(std::env::var(PATH_ENV).unwrap());
    let marker = PathBuf::from(std::env::var(MARKER_ENV).unwrap());
    let mut gen = 1u64;
    loop {
        // Each publish fsyncs BEFORE the rename, so once the marker exists
        // generation 50 is durable and the loop is publishing past it —
        // the parent's kill lands at an arbitrary instant of this loop.
        Current::publish(&path, &Current::new(FORMAT_VERSION, gen)).unwrap();
        if gen == 50 {
            let f = std::fs::File::create(&marker).unwrap();
            f.sync_all().unwrap();
        }
        gen += 1;
        if gen > 100_000 {
            break; // if the parent never kills us, exit rather than spin
        }
    }
}

#[test]
fn publication_crash_survives_kill() {
    if std::env::var(CHILD_ENV).is_ok() {
        child_main();
        return;
    }
    let d = dir("pub-kill");
    let path = d.join("CURRENT");
    let marker = d.join("marker");
    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(&exe)
        .arg("--exact")
        .arg("publication_crash_survives_kill")
        .env(CHILD_ENV, "1")
        .env(PATH_ENV, &path)
        .env(MARKER_ENV, &marker)
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(120);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "publication child never wrote the marker"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // Real kill: no cleanup, no graceful shutdown.
    let _ = child.kill();
    let _ = child.wait();

    // Invariant: the target is either a complete CURRENT or absent — a torn
    // write lives only in the temp file, which nobody reads.
    match Current::read(&path) {
        Ok(c) => assert_eq!(c.format_version, FORMAT_VERSION),
        Err(FormatError::Io(_)) => {}
        Err(e) => panic!("torn CURRENT after kill: {e:?}"),
    }
    // And no torn temp survives to poison the next publication.
    Current::publish(&path, &Current::new(FORMAT_VERSION, 999)).unwrap();
    assert_eq!(Current::read(&path).unwrap().manifest_generation, 999);
}
