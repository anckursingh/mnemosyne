//! KSE-120C — writer contention scaling (certification §5:
//! `docs/AIKOQL_Storage_Engine_MVP_Certification_TDD.md`).
//!
//! The doc's gap: KSE-13 pins correctness under concurrency; nothing
//! measures how throughput and latency behave as writers scale. RED = the
//! evidence does not exist — build the harness, optimize only if a measured
//! number violates an explicit MVP SLO.
//!
//! Matrix (the doc's): writers 1/2/4/8/16/32 × readers 0/32, constant total
//! workload (pure puts, unique keys — every acknowledged write is either
//! recoverable or missing, nothing in between), constant key/value shape,
//! one engine shared in-process (the engine does not support multi-process
//! sharing; within-process threads are its real contention surface — the
//! kernel's own pipeline is single-writer). Writers record per-write
//! latency; readers hammer random-key gets. After each scenario the engine
//! is dropped and REOPENED — the hard gate: recovered == acknowledged,
//! byte-exact, count-exact (no missing acked writes, no duplicates, valid
//! reopen = every envelope checksum verified).
//!
//! Metrics the doc asks for that CANNOT be measured without production
//! instrumentation are honest NOT_MEASURED rows, not invented: lock/queue
//! wait (the serialized section is engine-internal — write P50 vs the
//! 1-writer baseline IS the contention proxy), WAL append vs fsync split
//! (one serialized section, black box), RSS (steady-state memory is
//! KSE-19/143's surface).
//!
//! Sizing (strict opt-in, KSE-12/19 convention): the suite runs the full
//! 7-scenario matrix at 800 writes/scenario; `KSE120C_NIGHTLY=1` runs
//! 20,000. Any other value is a FAILURE (env-set-but-dead must never
//! silently skip).

mod common;

use aikoql_kernel::storage::store::{StorageEngine, WriteBatch};
use aikoql_storage::AikoqlStorageEngine;
use common::{percentiles, tmp};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const SEED: u64 = 0x120c_0000;
const VALUE_LEN: usize = 256;
const NIGHTLY_ENV: &str = "KSE120C_NIGHTLY";
/// The doc's matrix: (writers, readers).
const MATRIX: [(usize, usize); 7] = [
    (1, 0),
    (1, 32),
    (2, 32),
    (4, 32),
    (8, 32),
    (16, 32),
    (32, 32),
];

fn sizing() -> u64 {
    match std::env::var(NIGHTLY_ENV) {
        Err(std::env::VarError::NotPresent) => 800,
        Ok(v) if v == "1" => 20_000,
        other => panic!("KSE120C_NIGHTLY strict opt-in: unset or 1, got {other:?}"),
    }
}

fn value(writer: usize, seq: u64) -> Vec<u8> {
    (0..VALUE_LEN)
        .map(|j| {
            ((writer as u64)
                .wrapping_mul(31)
                .wrapping_add(seq)
                .wrapping_add(j as u64))
                & 0xFF
        })
        .map(|x| x as u8)
        .collect()
}

fn key_of(writer: usize, seq: u64) -> Vec<u8> {
    format!("w{writer:02}/s{seq:06}").into_bytes()
}

struct Row {
    writers: usize,
    readers: usize,
    writes: u64,
    writes_per_sec: f64,
    write_p50_ms: f64,
    write_p95_ms: f64,
    write_p99_ms: f64,
    reads: u64,
    reads_per_sec: f64,
    read_p50_ms: f64,
    read_p95_ms: f64,
    read_p99_ms: f64,
    wall_ms: f64,
}

/// One matrix cell: writers x readers over the constant workload, then the
/// reopen-recovery gate. Panics = RED.
fn scenario(writers: usize, readers: usize, total_writes: u64, label: &str) -> Row {
    assert_eq!(
        total_writes % writers as u64,
        0,
        "kse120c: workload must divide evenly across writers"
    );
    let writes_per = total_writes / writers as u64;

    let path = tmp(&format!("kse120c-{label}-w{writers}-r{readers}"));
    let engine = Arc::new(AikoqlStorageEngine::open(&path).unwrap());
    let model: Arc<Mutex<BTreeMap<Vec<u8>, Vec<u8>>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let write_lat: Arc<Mutex<Vec<u128>>> = Arc::new(Mutex::new(Vec::new()));
    let read_lat: Arc<Mutex<Vec<u128>>> = Arc::new(Mutex::new(Vec::new()));
    let reads = (readers * 500) as u64;

    let t0 = Instant::now();
    std::thread::scope(|s| {
        for w in 0..writers {
            let engine = engine.clone();
            let model = model.clone();
            let lat = write_lat.clone();
            s.spawn(move || {
                for seq in 0..writes_per {
                    let k = key_of(w, seq);
                    let v = value(w, seq);
                    let mut b = WriteBatch::new();
                    b.put(k.clone(), v.clone());
                    let t = Instant::now();
                    engine
                        .write_batch(&b)
                        .unwrap_or_else(|e| panic!("kse120c: write {w}/{seq} failed: {e:?}"));
                    lat.lock().unwrap().push(t.elapsed().as_nanos());
                    // Acknowledged only AFTER write_batch returned (applied +
                    // durable — the KSE-13 120a order).
                    model.lock().unwrap().insert(k, v);
                }
            });
        }
        for r in 0..readers {
            let engine = engine.clone();
            let lat = read_lat.clone();
            let mut state = SEED.wrapping_add(r as u64);
            s.spawn(move || {
                for _ in 0..reads / readers as u64 {
                    state = state
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    // Any (writer, seq) below the workload maps to a real
                    // key — not yet written reads as None, honestly.
                    let n = (state >> 17) % total_writes;
                    let k = key_of((n % writers as u64) as usize, n / writers as u64);
                    let t = Instant::now();
                    let _ = engine.get(&k).unwrap();
                    lat.lock().unwrap().push(t.elapsed().as_nanos());
                }
            });
        }
    });
    let wall = t0.elapsed();
    drop(engine); // close — the recovery gate reopens the same WAL

    // Hard gate: recovered == acknowledged, byte-exact (no missing acked
    // writes, no duplicates — keys are unique by construction, count pins
    // it). A successful reopen also means every envelope checksum verified
    // (no WAL corruption).
    let reopened = AikoqlStorageEngine::open(&path).unwrap();
    let scan = reopened.scan(b"").unwrap();
    let model = model.lock().unwrap();
    assert_eq!(
        scan.len(),
        model.len(),
        "kse120c: recovered {}/{} acknowledged writes",
        scan.len(),
        model.len()
    );
    for (k, v) in &scan {
        assert_eq!(
            model.get(k),
            Some(v),
            "kse120c: recovered value drifted at {:?}",
            String::from_utf8_lossy(k)
        );
    }

    let wl = write_lat.lock().unwrap();
    let (wp50, wp95, wp99) = percentiles(wl.clone());
    let rl = read_lat.lock().unwrap();
    let (rp50, rp95, rp99) = percentiles(rl.clone());
    let wall_ms = wall.as_secs_f64() * 1e3;
    Row {
        writers,
        readers,
        writes: total_writes,
        writes_per_sec: total_writes as f64 / wall_ms * 1e3,
        write_p50_ms: wp50 as f64 / 1e6,
        write_p95_ms: wp95 as f64 / 1e6,
        write_p99_ms: wp99 as f64 / 1e6,
        reads,
        reads_per_sec: reads as f64 / wall_ms * 1e3,
        read_p50_ms: rp50 as f64 / 1e6,
        read_p95_ms: rp95 as f64 / 1e6,
        read_p99_ms: rp99 as f64 / 1e6,
        wall_ms,
    }
}

#[test]
fn kse120c_writer_contention() {
    for &(w, r) in &MATRIX {
        let _ = scenario(w, r, sizing(), "test");
    }
}

// ---------------------------------------------------------------------------
// Report: artifacts/storage-engine/kse120c-writer-contention.md
// ---------------------------------------------------------------------------

#[test]
fn kse120c_report() {
    let total = sizing();
    let rows: Vec<Row> = MATRIX
        .iter()
        .map(|&(w, r)| scenario(w, r, total, "report"))
        .collect();

    let mut table = String::from(
        "| writers | readers | writes | writes/sec | write P50/P95/P99 ms | reads | \
         reads/sec | read P50/P95/P99 ms | wall s | recovered == acked |\n\
         |---:|---:|---:|---:|---:|---:|---:|---:|---:|---|\n",
    );
    for r in &rows {
        let read_cells = if r.readers == 0 {
            ("0".to_string(), "—".to_string(), "—".to_string())
        } else {
            (
                r.reads.to_string(),
                format!("{:.0}", r.reads_per_sec),
                format!(
                    "{:.2} / {:.2} / {:.2}",
                    r.read_p50_ms, r.read_p95_ms, r.read_p99_ms
                ),
            )
        };
        table.push_str(&format!(
            "| {} | {} | {} | {:.0} | {:.2} / {:.2} / {:.2} | {} | {} | {} | {:.1} | ✓ (asserted, byte-exact) |\n",
            r.writers,
            r.readers,
            r.writes,
            r.writes_per_sec,
            r.write_p50_ms,
            r.write_p95_ms,
            r.write_p99_ms,
            read_cells.0,
            read_cells.1,
            read_cells.2,
            r.wall_ms / 1e3,
        ));
    }

    // Proposed SLOs, data-driven (reported, not asserted — §9): the 1x0 row
    // is the single-writer baseline; contention shows as its P50 delta.
    let baseline = &rows[0];
    let saturated = &rows[6];
    let slo = format!(
        "- 100% acknowledged-write recovery at every writer count — the \
         only asserted gate (all scenarios, above)\n\
         - write P50 at 1 writer <= {:.1} ms (measured {:.2} ms; 1.5x \
         headroom)\n\
         - throughput must not collapse: 32-writer rate >= 25% of the \
         1-writer rate (measured {:.0}/sec vs {:.0}/sec = {:.0}%) — \
         serialization is intentional (log Mutex across append+fsync+apply, \
         KSE-13 120a), so plateau is expected; a collapse would signal lock \
         or scheduling pathology\n",
        baseline.write_p50_ms * 1.5,
        baseline.write_p50_ms,
        saturated.writes_per_sec,
        baseline.writes_per_sec,
        saturated.writes_per_sec / baseline.writes_per_sec * 100.0,
    );

    let profile = if cfg!(debug_assertions) {
        "debug (CPU inflated; RSS comparable)"
    } else {
        "release"
    };
    let report = format!(
        "# KSE-120C — Writer Contention Scaling (certification §5)\n\n\
         Date: 2026-09-01 · seed {SEED:#x} · engine: AikoqlStorageEngine · \
         build profile: {profile} · workload: {total} puts per scenario \
         (unique keys, {VALUE_LEN} B values) · test: \
         kse120c_writer_contention.rs\n\n\
         {table}\n\n\
         ## Proposed SLOs (reported, not asserted)\n\n{slo}\n\n\
         ## NOT_MEASURED (metrics that cannot be measured here)\n\n\
         - lock/queue wait: the serialized section is engine-internal — \
         write P50 vs the 1-writer baseline IS the contention proxy; a \
         separate number would need production instrumentation\n\
         - WAL append time / fsync time: one serialized section, \
         engine-internal — not separable without production instrumentation; \
         the behavioral pin is KSE-13 KSE-120a (log order == commit order)\n\
         - CPU: single-machine wall time is the scenario column; per-thread \
         CPU attribution is not separable\n\
         - RSS: steady-state memory is KSE-19/143's surface; the contention \
         matrix adds no durable state\n\n\
         ## Honest limits\n\n\
         - contention surface is within-process threads — the engine does \
         not support multi-process sharing (documented), and the kernel's \
         own pipeline is single-writer; the 32-writer row is deliberately \
         beyond any real AIKOQL workload\n\
         - readers hammer random keys, hit rate grows during the run; read \
         latency includes None gets\n\
         - write latency includes fsync (the serialized section) — it is \
         durability cost, not lock cost\n\
         - debug builds inflate CPU but not the serialization shape; \
         nightly rows should be produced in release\n\
         - wall times race sibling tests (kse19 convention); evidence, not \
         gates\n",
    );
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../artifacts/storage-engine");
    std::fs::write(dir.join("kse120c-writer-contention.md"), report).unwrap();
}
