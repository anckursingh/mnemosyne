//! SE2-M20 — chunked merge emission: a compaction merge publishes its
//! output as a sequence of bounded segments (`Config.merge_chunk_bytes`;
//! 64 MiB default, 0 = one unbounded segment, the pre-M20 shape) instead
//! of buffering the whole merged dataset in one writer. Chunks split on
//! entry granularity in merge order, so chunks are globally sorted and
//! non-overlapping; the manifest carries one record per chunk and ids
//! stay sequential from one counter (archive chunks pull lazily). The
//! manifest naming all chunks remains the single atomic commit point, so
//! every pre-M20 crash window is the same window with k files instead of
//! one — a kill before the manifest leaves k orphan chunks the next open
//! ignores (and may reuse their ids, the M3 orphan behavior).

mod common;

use aikoql_storage_v2::compaction::{Retention, RetentionPolicy};
use aikoql_storage_v2::db::{manifest_path, segment_path, Config, Db, ScanRow};
use aikoql_storage_v2::format::{Current, Manifest};
use aikoql_storage_v2::segment::SegmentReader;
use common::dir;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn chunked_cfg(d: PathBuf, chunk: usize) -> Config {
    let mut cfg = Config::new(d);
    cfg.memtable_bytes = 4096;
    cfg.l0_compact_trigger = 0; // the explicit compact is what is pinned
    cfg.merge_chunk_bytes = chunk;
    cfg
}

fn segment_files(d: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(d)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("SEGMENT-"))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

/// 100 keys × 100-byte values — ~12 KiB of rows, ~24 chunks at a
/// 512-byte cap (~122 bytes estimated per entry).
fn seed(db: &Db, n: usize) {
    let value = vec![b'v'; 100];
    for i in 0..n {
        db.put(format!("k{i:04}").as_bytes(), &value).unwrap();
    }
    db.flush().unwrap();
}

#[test]
fn merge_chunks_respect_chunk_cap() {
    let d = dir("merge-chunk-geometry");
    let db = Db::open(chunked_cfg(d.clone(), 512)).unwrap();
    seed(&db, 100);
    let l0 = segment_files(&d).len(); // flushes before the compact
    assert!(l0 >= 2, "the 4 KiB memtable must flush before the compact");
    let stats = db.compact().unwrap();
    let k = stats.segments_out;
    assert!(
        k >= 2,
        "a 512-byte cap over ~12 KiB of rows must split the output, got {k}"
    );
    assert_eq!(stats.entries_out, 100, "every live row lands in one chunk");

    // One file per chunk, ids sequential from the pre-compact counter:
    // the compact consumed ids l0+1 ..= l0+k.
    let files = segment_files(&d);
    let want: Vec<PathBuf> = (l0 as u64 + 1..=l0 as u64 + k)
        .map(|id| segment_path(&d, id))
        .collect();
    assert_eq!(
        files, want,
        "chunks are the only segments and ids stay sequential"
    );

    // Chunks are globally sorted, non-overlapping, and cover every row.
    let mut total = 0;
    let mut readers = Vec::new();
    for p in &files {
        let r = SegmentReader::open(p).unwrap();
        assert!(r.entry_count() > 0, "no empty chunk is published");
        total += r.entry_count();
        readers.push(r);
    }
    assert_eq!(total, 100);
    for w in readers.windows(2) {
        assert!(
            w[0].key_max() < w[1].key_min(),
            "chunks must be globally sorted and non-overlapping"
        );
    }
    // Every key answers byte-exact through the chunked manifest.
    let value = vec![b'v'; 100];
    for i in 0..100 {
        assert_eq!(
            db.get(format!("k{i:04}").as_bytes()).unwrap(),
            Some(value.clone()),
            "key {i} diverged"
        );
    }
    // next_segment_id advanced past the last chunk: the next flush lands
    // at l0 + k + 1.
    db.put(b"post", b"x").unwrap();
    db.flush().unwrap();
    assert!(
        segment_path(&d, l0 as u64 + k + 1).exists(),
        "the post-compact flush must take the id after the last chunk"
    );
}

#[test]
fn merge_chunks_answers_parity_with_unbounded() {
    // One deterministic workload (overwrites, deletes, versions) merged
    // unbounded and chunked: the logical state must be byte-identical.
    let run = |d: PathBuf, chunk: usize| -> (Vec<ScanRow>, u64) {
        let db = Db::open(chunked_cfg(d.clone(), chunk)).unwrap();
        let mut s = 7u64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for i in 0..200 {
            let k = format!("k{:03}", next() % 50).into_bytes();
            match next() % 10 {
                0..=6 => {
                    db.put(&k, &format!("v{i:03}").into_bytes()).unwrap();
                }
                _ => {
                    db.delete(&k).unwrap();
                }
            }
        }
        db.flush().unwrap();
        let stats = db.compact().unwrap();
        let scan = db.scan(b"").unwrap();
        (scan, stats.entries_out)
    };
    let (a, a_out) = run(dir("merge-chunk-parity-a"), 0);
    let (b, b_out) = run(dir("merge-chunk-parity-b"), 512);
    assert_eq!(a, b, "chunked merge must produce the same logical state");
    assert_eq!(a_out, b_out);
}

struct ArchiveA;

impl RetentionPolicy for ArchiveA {
    fn classify(&self, key: &[u8]) -> Retention {
        if key.starts_with(b"ko/a") {
            Retention::Archive
        } else {
            Retention::Keep
        }
    }
}

#[test]
fn merge_chunks_archive_splits_across_chunks() {
    let d = dir("merge-chunk-archive");
    let db = Db::open(chunked_cfg(d.clone(), 512)).unwrap();
    for i in 0..200 {
        db.put(format!("ko/a/{i:04}").as_bytes(), &[b'v'; 64])
            .unwrap();
        db.put(format!("ko/b/{i:04}").as_bytes(), &[b'w'; 64])
            .unwrap();
    }
    db.flush().unwrap();
    let stats = db.compact_with(&ArchiveA).unwrap();
    assert_eq!(stats.entries_archived, 200, "all a-rows archived");

    let mut archives: Vec<PathBuf> = std::fs::read_dir(d.join("archive"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("ARCHIVE-"))
        .map(|e| e.path())
        .collect();
    archives.sort();
    assert!(
        archives.len() >= 2,
        "the 512-byte cap must split the archive too, got {}",
        archives.len()
    );
    let mut total = 0;
    for p in &archives {
        total += SegmentReader::open(p).unwrap().entry_count();
    }
    assert_eq!(total, 200, "every archived row lands in exactly one chunk");

    // The live space: a-rows gone, b-rows byte-exact.
    for i in 0..200 {
        assert_eq!(db.get(format!("ko/a/{i:04}").as_bytes()).unwrap(), None);
        assert_eq!(
            db.get(format!("ko/b/{i:04}").as_bytes()).unwrap(),
            Some(vec![b'w'; 64])
        );
    }
}

// --- crash matrix: the pre-M20 windows, k chunks instead of one ---

const CHILD_ENV: &str = "AIKOQL_V2_KILL_CHILD";
const DIR_ENV: &str = "AIKOQL_V2_KILL_DIR";
const STAGE_ENV: &str = "AIKOQL_V2_COMPACT_PARK";
const CHUNK_ENV: &str = "AIKOQL_V2_KILL_CHUNK";

fn spawn_child(test_name: &str, d: &Path, stage: &str, chunk: usize) -> Child {
    Command::new(std::env::current_exe().expect("current exe"))
        .arg("--exact")
        .arg(test_name)
        .env(CHILD_ENV, "1")
        .env(DIR_ENV, d)
        .env(STAGE_ENV, stage)
        .env(CHUNK_ENV, chunk.to_string())
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

/// The compact_crash workload: 120 single-op batches over 40 hot keys.
fn ops() -> Vec<(bool, Vec<u8>, Vec<u8>)> {
    let mut s = 7u64;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
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

fn expected() -> HashMap<Vec<u8>, Option<Vec<u8>>> {
    let mut m = HashMap::new();
    for (put, k, v) in ops() {
        m.insert(k, if put { Some(v) } else { None });
    }
    m
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

fn child_branch() -> bool {
    if std::env::var_os(CHILD_ENV).is_none() {
        return false;
    }
    let mut cfg = chunked_cfg(
        PathBuf::from(std::env::var(DIR_ENV).expect("child dir env")),
        std::env::var(CHUNK_ENV)
            .expect("child chunk env")
            .parse()
            .expect("chunk cap"),
    );
    cfg.memtable_bytes = 512;
    cfg.l0_compact_trigger = 0;
    let db = Db::open(cfg).unwrap();
    run_workload_then_compact(&db);
    unreachable!("the parent kills the parked child");
}

/// Reopen after the kill: every key holds its expected value and the
/// sequence resumes at 121; then a flush exercises segment-id reuse over
/// the orphan chunks (reopen's next_segment_id = old max + 1 = the first
/// orphan's id — the M3 reuse-by-rename behavior) and survives reopen.
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
    assert_eq!(
        db.put(b"next", b"x").unwrap(),
        121,
        "no acked write lost, no phantom commit"
    );
    db.flush().unwrap();
    drop(db);
    let db = Db::open(Config::new(d.to_path_buf())).unwrap();
    assert_eq!(db.get(b"next").unwrap(), Some(b"x".to_vec()));
}

#[test]
fn merge_chunks_crash_after_segment_recovers() {
    if std::env::var_os(CHILD_ENV).is_some() {
        child_branch();
    }
    let d = dir("merge-chunk-crash-seg");
    let mut child = spawn_child(
        "merge_chunks_crash_after_segment_recovers",
        &d,
        "after_segment",
        32,
    );
    wait_for(&d.join("after_segment"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    // The old manifest still governs; the k chunks are orphans.
    let current = Current::read(&d.join("CURRENT")).unwrap();
    let manifest = Manifest::read(&manifest_path(&d, current.manifest_generation)).unwrap();
    assert!(
        manifest.segments.iter().all(|s| s.level == 0),
        "no L1 record published yet"
    );
    let k = segment_files(&d).len() - manifest.segments.len();
    assert!(k >= 2, "the merge must have split, got {k} orphans");
    verify(&d);
}

#[test]
fn merge_chunks_crash_after_manifest_recovers() {
    if std::env::var_os(CHILD_ENV).is_some() {
        child_branch();
    }
    let d = dir("merge-chunk-crash-manifest");
    let mut child = spawn_child(
        "merge_chunks_crash_after_manifest_recovers",
        &d,
        "after_manifest",
        32,
    );
    wait_for(&d.join("after_manifest"), Duration::from_secs(60));
    child.kill().expect("kill child");
    child.wait().expect("wait child");

    // CURRENT still points at the old generation; the parked new manifest
    // carries one record per chunk — consecutive ids, non-overlapping key
    // ranges, every live row covered.
    let current = Current::read(&d.join("CURRENT")).unwrap();
    let manifest = Manifest::read(&manifest_path(&d, current.manifest_generation)).unwrap();
    assert!(
        manifest.segments.iter().all(|s| s.level == 0),
        "the current manifest is the pre-compaction one"
    );
    let parked = Manifest::read(&manifest_path(&d, current.manifest_generation + 1)).unwrap();
    assert!(
        parked.segments.len() >= 2,
        "the parked manifest carries one record per chunk"
    );
    assert!(parked.segments.iter().all(|s| s.level == 1));
    let ids: Vec<u64> = parked.segments.iter().map(|s| s.segment_id).collect();
    assert!(
        ids.windows(2).all(|w| w[1] == w[0] + 1),
        "chunk ids stay consecutive"
    );
    for w in parked.segments.windows(2) {
        assert!(
            w[0].key_max < w[1].key_min,
            "chunk key ranges never overlap"
        );
    }
    let live = expected().values().filter(|v| v.is_some()).count() as u64;
    assert_eq!(
        parked.segments.iter().map(|s| s.record_count).sum::<u64>(),
        live,
        "the chunks together hold exactly the live keys"
    );
    verify(&d);
}
