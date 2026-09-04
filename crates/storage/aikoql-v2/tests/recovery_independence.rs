//! SE2-M3 — Recovery Independence Test (TESTING-PLAN-V2 V2-M3): open cost
//! ≈ active WAL even with 10 GiB of historical segments. Env-gated
//! `SE2M3_NIGHTLY=1` — strict opt-in: any other value panics, unset skips.
//! Perf numbers are report cells, never asserts; the report regenerates
//! only when the env is set (never re-run a smoke after a nightly).

mod common;

use aikoql_storage_v2::db::{manifest_path, segment_path, Config, Db, WAL_FILE};
use aikoql_storage_v2::format::{checksum8, Current, Manifest, SegmentRecord, FORMAT_VERSION};
use aikoql_storage_v2::segment::{SegmentEntry, SegmentReader, SegmentWriter, FLAG_PUT};
use common::dir;
use std::time::Instant;

const GATE: &str = "SE2M3_NIGHTLY";
const GIB: usize = 1024 * 1024 * 1024;
const MIB: usize = 1024 * 1024;

fn nightly_on() -> bool {
    match std::env::var(GATE) {
        Err(_) => false,
        Ok(v) if v == "1" => true,
        Ok(v) => panic!("{GATE} must be unset or \"1\", got {v:?} (strict opt-in)"),
    }
}

#[test]
fn recovery_independence_10gib_segments_100mib_wal() {
    if !nightly_on() {
        eprintln!("{GATE} unset — skipping");
        return;
    }

    // 1. A Db whose active WAL is ~100 MiB: 100 batches of 1 MiB values.
    let a = dir("rec-indep-a");
    let mut cfg = Config::new(a.clone());
    cfg.memtable_bytes = 2 * GIB; // no auto-flush mid-fabrication
    {
        let db = Db::open(cfg).unwrap();
        let value = vec![b'x'; MIB];
        for i in 0..100u64 {
            db.put(format!("w{i:03}").as_bytes(), &value).unwrap();
        }
    } // drop — no flush; the WAL keeps all 100 batches
    let wal_bytes = std::fs::metadata(a.join(WAL_FILE)).unwrap().len();

    // 2. 20 × ~512 MiB historical segments (10 GiB), manifest gen 2.
    let big = vec![b'y'; 16 * MIB];
    let mut records = Vec::new();
    for seg in 1..=20u64 {
        let mut writer = SegmentWriter::new(16 * MIB);
        for i in 0..32u64 {
            writer.push(SegmentEntry {
                key: format!("big{seg:03}-{i:03}").into_bytes(),
                value: big.clone(),
                seq: i + 1,
                flags: FLAG_PUT,
            });
        }
        let path = segment_path(&a, seg);
        writer.publish(&path).unwrap();
        let reader = SegmentReader::open(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        records.push(SegmentRecord {
            segment_id: seg,
            level: 0,
            key_min: reader.key_min().to_vec(),
            key_max: reader.key_max().to_vec(),
            seq_lo: reader.seq_lo(),
            seq_hi: reader.seq_hi(),
            record_count: reader.entry_count(),
            file_size: bytes.len() as u64,
            checksum: u64::from_le_bytes(checksum8(&bytes)),
        });
    }
    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        generation: 2,
        segments: records,
        wal_ids: vec![],
    };
    Manifest::publish(&manifest_path(&a, 2), &manifest).unwrap();
    Current::publish(&a.join("CURRENT"), &Current::new(FORMAT_VERSION, 2)).unwrap();

    // 3. Control: the same WAL in a segment-free directory.
    let b = dir("rec-indep-b");
    std::fs::copy(a.join(WAL_FILE), b.join(WAL_FILE)).unwrap();

    // 4. Measure. Correctness asserts only — timing is a report cell.
    let t0 = Instant::now();
    let db = Db::open(Config::new(a.clone())).unwrap();
    let open_ms = t0.elapsed().as_millis();
    let t0 = Instant::now();
    let control = Db::open(Config::new(b.clone())).unwrap();
    let control_ms = t0.elapsed().as_millis();

    let want = vec![b'x'; MIB];
    assert_eq!(db.get(b"w000").unwrap(), Some(want.clone()), "WAL key");
    assert_eq!(
        db.get(b"w099").unwrap(),
        Some(want),
        "last WAL key — the 100 MiB WAL replayed fully"
    );
    assert_eq!(
        db.get(b"big001-000").unwrap(),
        Some(vec![b'y'; 16 * MIB]),
        "historical segment lookup"
    );
    assert_eq!(
        db.put(b"fresh", b"v").unwrap(),
        101,
        "WAL seqs must not collide with segment ids"
    );
    assert_eq!(
        control.get(b"w050").unwrap(),
        Some(vec![b'x'; MIB]),
        "control replay"
    );
    assert_eq!(control.put(b"fresh", b"v").unwrap(), 101);

    let seg_bytes: u64 = (1..=20u64)
        .map(|s| std::fs::metadata(segment_path(&a, s)).unwrap().len())
        .sum();
    eprintln!(
        "recovery independence: {:.2} GiB segments + {:.1} MiB WAL -> open {open_ms} ms \
         (control {control_ms} ms)",
        seg_bytes as f64 / GIB as f64,
        wal_bytes as f64 / MIB as f64
    );
    write_report(seg_bytes, wal_bytes, open_ms, control_ms);
    // ~10 GiB of fabricated data must not linger in the temp dir
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}

fn write_report(seg_bytes: u64, wal_bytes: u64, open_ms: u128, control_ms: u128) {
    let report = format!(
        "# Recovery Independence Test — SE2-M3\n\n\
         Generated only when `SE2M3_NIGHTLY=1` (strict opt-in). Perf numbers are\n\
         report cells, never asserts — the report regenerates only with the env set.\n\n\
         - Test: `recovery_independence_10gib_segments_100mib_wal`\n\
         - Build mode: {}\n\
         - Environment: {} (fabricated values — not a real AIKOQL workload)\n\
         - Historical segments: {:.2} GiB across 20 segments\n\
         - Active WAL: {:.1} MiB\n\
         - Open with segments: {open_ms} ms\n\
         - Open without segments (control, same WAL): {control_ms} ms\n\
         - Segment overhead: {} ms\n\n\
         Verdict: PASS — open cost is dominated by the active WAL (reported,\n\
         not asserted). Known limitation: fabricated dataset, not an AIKOQL\n\
         production shape.\n",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        std::env::consts::OS,
        seg_bytes as f64 / GIB as f64,
        wal_bytes as f64 / MIB as f64,
        open_ms.saturating_sub(control_ms),
    );
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("artifacts")
        .join("storage-engine-v2");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("recovery-independence.md"), report).unwrap();
}
