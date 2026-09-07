//! SE2-M38 — relocation stress + memory (§40 milestone 10 + §45/§46).
//!
//! Heavy arms are strict opt-in via `SE2M38_NIGHTLY` (unset skips):
//! the §40 stress (100,000 objects × updates/flushes/compactions/restarts
//! — ObjectId/LogicalId/ReplicaId pinned unchanged throughout, values
//! correct, PhysicalLocation allowed to move) and the §45 memory gate
//! (1M objects, WorkingSet64 plateaus at the half/full markers, marginal
//! bytes per object — MEASURED and reported, never asserted; the numbers
//! feed artifacts/storage-engine-v2/identity-placement.md).
//! Always-on: the same cycle at 500 objects, so the suite pins the shape
//! every run.

mod common;

use aikoql_storage_v2::db::{Config, Db, DurabilityMode};
use aikoql_storage_v2::identity::directory::{IdentityResolver, LocalIdentityDirectory};
use aikoql_storage_v2::identity::topology::{LocalReplicaDirectory, ReplicaDirectory};
use aikoql_storage_v2::identity::{LogicalId, ObjectId, ReplicaId};
use aikoql_storage_v2::placement::directory::{
    LocalPlacementResolver, Placement, PlacementResolver,
};
use aikoql_storage_v2::segment::{segment_path, SegmentReader};
use common::dir;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const NIGHTLY: &str = "SE2M38_NIGHTLY";

fn oid_of(i: u64) -> ObjectId {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&i.to_le_bytes());
    ObjectId(b)
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

/// The stress cycle: create `n` in four flush batches, update every 7th
/// object, flush, compact — then the caller restarts and verifies.
/// Returns the (ObjectId, LogicalId, ReplicaId) triple per object.
fn stress_cycle(d: &Path, n: u64) -> Vec<(ObjectId, LogicalId, ReplicaId)> {
    let mut cfg = Config::new(d.to_path_buf());
    cfg.l0_compact_trigger = 0;
    // §40 measures identity stability, not the durability boundary —
    // per-op fsync would dominate the run for zero extra pin.
    cfg.durability = DurabilityMode::Async;
    let db = Db::open(cfg).unwrap();
    let per = (n / 4).max(1);
    for chunk in 0..4u64 {
        let lo = chunk * per;
        let hi = if chunk == 3 { n } else { lo + per };
        for i in lo..hi {
            db.put_object(oid_of(i), b"k0", format!("v{i}").as_bytes())
                .unwrap();
        }
        db.flush().unwrap();
    }
    for i in (0..n).step_by(7) {
        db.put_object(oid_of(i), b"k1", format!("u{i}").as_bytes())
            .unwrap();
    }
    db.flush().unwrap();
    let cstats = db.compact().unwrap();
    let rec: Vec<_> = (0..n)
        .map(|i| {
            let a = oid_of(i);
            (a, lid_of(&db, a), rid_of(&db, a))
        })
        .collect();
    // §46 Q5 — amplification numbers only mean anything at stress scale.
    if n > 1000 {
        report_amplification(d, &cstats);
    }
    rec
}

/// §46 Q5 — compaction amplification, measured from the stress dir: the
/// compact's own stats (records relocated) plus every directory log's
/// record count and byte weight vs the live segment bytes (location
/// entries updated, bytes written, metadata write amplification).
fn report_amplification(d: &Path, cstats: &aikoql_storage_v2::compaction::CompactStats) {
    use aikoql_storage_v2::identity::directory::{load_identity_logs, load_replica_logs};
    use aikoql_storage_v2::placement::directory::load_placement_logs;
    let mut seg_bytes = 0u64;
    let mut id_bytes = 0u64;
    let mut rep_bytes = 0u64;
    let mut pl_bytes = 0u64;
    for e in std::fs::read_dir(d).unwrap() {
        let p = e.unwrap().path();
        if !p.is_file() {
            continue;
        }
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let bytes = std::fs::metadata(&p).unwrap().len();
        if name.starts_with("SEGMENT-") {
            seg_bytes += bytes;
        } else if name.starts_with("IDENTITY-") {
            id_bytes += bytes;
        } else if name.starts_with("REPLICA-") {
            rep_bytes += bytes;
        } else if name.starts_with("PLACEMENT-") {
            pl_bytes += bytes;
        }
    }
    // u64::MAX: every generation loads — these are the run's own logs.
    let mut id_records = 0usize;
    let mut rep_records = 0usize;
    let mut pl_records = 0usize;
    for log in load_identity_logs(d, u64::MAX).unwrap() {
        id_records += log.records.len();
    }
    for log in load_replica_logs(d, u64::MAX).unwrap() {
        rep_records += log.records.len();
    }
    for log in load_placement_logs(d, u64::MAX).unwrap() {
        pl_records += log.records.len();
    }
    println!(
        "Q5 amplification: compact entries_in {} entries_out {} segments_in {} segments_out {}; \
         location entries updated {pl_records} ({pl_bytes} B placement logs); \
         identity {id_records} records ({id_bytes} B), replica {rep_records} records ({rep_bytes} B); \
         live segment bytes {seg_bytes}; \
         metadata write amplification {:.3}",
        cstats.entries_in,
        cstats.entries_out,
        cstats.segments_in,
        cstats.segments_out,
        (id_bytes + rep_bytes + pl_bytes) as f64 / seg_bytes.max(1) as f64,
    );
}

/// Every object's identity triple is the recorded one (§40: unchanged).
fn verify_identity(db: &Db, rec: &[(ObjectId, LogicalId, ReplicaId)]) {
    for (a, lid, rid) in rec {
        assert_eq!(lid_of(db, *a), *lid, "ObjectId→LogicalId changed for {a:?}");
        assert_eq!(rid_of(db, *a), *rid, "ObjectId→ReplicaId changed for {a:?}");
    }
}

/// Every committed value answers correctly (§40).
fn verify_values(db: &Db, n: u64) {
    for i in 0..n {
        assert_eq!(
            db.get_object(oid_of(i), b"k0").unwrap(),
            Some(format!("v{i}").into_bytes()),
            "value for object {i} changed"
        );
        if i % 7 == 0 {
            assert_eq!(
                db.get_object(oid_of(i), b"k1").unwrap(),
                Some(format!("u{i}").into_bytes()),
                "update for object {i} lost"
            );
        }
    }
}

/// A sample of placements resolves to entries carrying the replica's id
/// (§40: PhysicalLocation may move — it must still be valid).
fn verify_placements_valid(db: &Db, d: &Path, rec: &[(ObjectId, LogicalId, ReplicaId)]) {
    for (i, (_, _, rid)) in rec.iter().enumerate().step_by(997) {
        let Some(placement) = placement_of(db, *rid) else {
            panic!("object {i} lost its placement");
        };
        if let Placement::Segment(loc) = placement {
            let entry = SegmentReader::open(&segment_path(d, loc.segment_id.0))
                .unwrap()
                .entry_at(loc.block_id, loc.entry_offset)
                .unwrap()
                .expect("the placement names an existing entry");
            assert_eq!(entry.replica_id, *rid, "the entry carries the replica's id");
        }
    }
}

fn stress_n() -> u64 {
    if std::env::var_os(NIGHTLY).is_some() {
        100_000
    } else {
        500
    }
}

#[test]
fn st001_relocation_stress() {
    let n = stress_n();
    let d = dir(if n > 1000 {
        "st001-100k"
    } else {
        "st001-smoke"
    });
    let rec = stress_cycle(&d, n);

    // Restart 1: everything committed before the compact must survive it.
    let db = Db::open(Config::new(d.clone())).unwrap();
    verify_identity(&db, &rec);
    verify_values(&db, n);
    verify_placements_valid(&db, &d, &rec);
    drop(db);

    // Restart 2: the relocated state is stable across a second restart.
    let db = Db::open(Config::new(d.clone())).unwrap();
    verify_identity(&db, &rec);
    verify_values(&db, n);
    verify_placements_valid(&db, &d, &rec);
}

// ---------------------------------------------------------------------------
// §45 memory gate — 1M objects, WorkingSet64 plateaus (kse19 pattern)
// ---------------------------------------------------------------------------

const MEM_CHILD: &str = "AIKOQL_V2_MEM_CHILD";
const MEM_DIR: &str = "AIKOQL_V2_MEM_DIR";
const MEM_N: &str = "AIKOQL_V2_MEM_N";

fn mem_child_dir() -> PathBuf {
    PathBuf::from(std::env::var(MEM_DIR).expect("child dir env"))
}

fn mem_child_n() -> u64 {
    std::env::var(MEM_N)
        .expect("child n env")
        .parse()
        .expect("child n")
}

/// The loader: creates N objects (Async — the directories and memtable
/// are the subject, not the durability boundary), parks 3s at the half
/// marker and 3s at the full marker so the parent's sampler catches two
/// stable plateaus.
fn mem_child() {
    let d = mem_child_dir();
    let n = mem_child_n();
    let mut cfg = Config::new(d);
    cfg.durability = DurabilityMode::Async;
    let db = Db::open(cfg).unwrap();
    let half = n / 2;
    for i in 0..n {
        if i == half {
            std::fs::write("half", b"1").unwrap();
            std::thread::sleep(Duration::from_secs(3));
        }
        db.put_object(oid_of(i), b"k0", format!("v{i}").as_bytes())
            .unwrap();
    }
    std::fs::write("full", b"1").unwrap();
    // §45 — per-directory resident bytes (capacity-exact, allocator
    // excluded) at the full marker, for the report's per-directory rows.
    let (id, rep, pl) = db.directory_resident_bytes();
    println!("MEM001 directories: identity {id} B, replica {rep} B, placement {pl} B");
    std::thread::sleep(Duration::from_secs(3));
}

/// The kse19 sampler: poll the child's WorkingSet64; `half_rss`/`full_rss`
/// are the max samples inside each 3s marker park (stable plateaus).
#[cfg(windows)]
fn sample_plateaus(pid: u32, marks: &Path) -> Option<(u64, u64)> {
    let script = format!(
        "while (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ Write-Output (Get-Process -Id {pid}).WorkingSet64; Start-Sleep -Milliseconds 200 }}"
    );
    let mut sampler = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let out = sampler.stdout.take().unwrap();
    let mut half_rss = 0u64;
    let mut full_rss = 0u64;
    let mut half_seen = false;
    let mut full_seen = false;
    let mut since_half = Instant::now();
    let mut since_full = Instant::now();
    for line in BufReader::new(out).lines().map_while(Result::ok) {
        if !half_seen && marks.join("half").exists() {
            half_seen = true;
            since_half = Instant::now();
        }
        if !full_seen && marks.join("full").exists() {
            full_seen = true;
            since_full = Instant::now();
        }
        if let Ok(v) = line.trim().parse::<u64>() {
            if half_seen && since_half.elapsed() < Duration::from_millis(2500) {
                half_rss = half_rss.max(v);
            }
            if full_seen && since_full.elapsed() < Duration::from_millis(2500) {
                full_rss = full_rss.max(v);
            }
        }
        if half_seen && full_seen && since_full.elapsed() > Duration::from_millis(2500) {
            break;
        }
    }
    let _ = sampler.wait();
    if half_seen && full_seen {
        Some((half_rss, full_rss))
    } else {
        None
    }
}

/// Non-Windows: no WorkingSet64 poll — the report arm runs on Windows only.
#[cfg(not(windows))]
fn sample_plateaus(_pid: u32, _marks: &Path) -> Option<(u64, u64)> {
    None
}

#[test]
fn mem001_directory_memory_report() {
    if std::env::var_os(NIGHTLY).is_none() {
        return; // strict opt-in — the 1M load is a minutes-scale, GB-scale arm
    }
    if std::env::var_os(MEM_CHILD).is_some() {
        return mem_child();
    }
    let d = dir("mem001");
    let n: u64 = 1_000_000;
    // The markers land in the child's CWD — set it to a scratch dir so
    // parallel tests never see a stale marker.
    let marks = dir("mem001-marks");
    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(&exe)
        .arg("--exact")
        .arg("mem001_directory_memory_report")
        .arg("--nocapture") // the child's own report lines must not be captured
        .env(MEM_CHILD, "1")
        .env(MEM_DIR, &d)
        .env(MEM_N, n.to_string())
        .current_dir(&marks)
        .spawn()
        .unwrap();
    let pid = child.id();
    let plateaus = sample_plateaus(pid, &marks);
    let status = child.wait().unwrap();
    assert!(status.success(), "memory loader child failed");
    let (half_rss, full_rss) = plateaus.expect("both plateaus sampled");
    let marginal = full_rss - half_rss; // n/2 objects
    let bytes_per_object = 2 * marginal / n;
    println!(
        "MEM001 report (n={n}): half {half_rss} B, full {full_rss} B, \
         marginal {marginal} B for {n}/2 objects, {bytes_per_object} bytes/object"
    );
    std::fs::remove_dir_all(&d).ok();
    std::fs::remove_dir_all(&marks).ok();
}
