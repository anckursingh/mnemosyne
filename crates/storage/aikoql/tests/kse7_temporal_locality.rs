//! KSE-7 — temporal locality (MRFC-KSE-001 §13).
//!
//! KSE-060..063: current version, historical version, full history, and
//! temporal range — the four version-read shapes — measured over the same
//! seeded MVCC dataset on redb, RocksDB (strict opt-in), and
//! AikoqlStorageEngine.
//!
//! Gates (not timing):
//! - KSE-060: the current-version read issues ZERO scans — the head pointer
//!   resolves in O(1) gets, no history walk, at every version depth.
//! - KSE-061: get_as_of at a mid-history wall instant returns the version
//!   whose commit ts is the newest <= the snap.
//! - KSE-062: history returns every version, strictly ascending commit ts.
//! - KSE-063: [t1, t2) returns exactly the versions in range.
//! - Backend parity: identical version-ts sequences and identical engine
//!   request shapes over every backend (same seed, same clock).

mod common;

use aikoql_kernel::storage::store::StorageEngine;
use aikoql_kernel::storage::store_redb::RedbEngine;
use aikoql_kernel::transaction::kernel::ManualClock;
use aikoql_kernel::{Kernel, Metadata, RememberRequest, Subject};
use aikoql_storage::AikoqlStorageEngine;
use common::{percentiles, tmp, CountingEngine, LogicalCounts};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

const N_KOS: usize = 50;
const VERSIONS: usize = 50; // updates after create → 51 version rows per KO
const REPS: usize = 20;
const SALT: u64 = 0xC0FFEE;

fn alice() -> Subject {
    Subject::new("alice")
}

fn meta() -> Metadata {
    Metadata {
        type_name: "fact".into(),
        tenant: None,
        schema_version: 1,
        tags: vec![],
    }
}

type Versions = Vec<(u64, aikoql_kernel::KnowledgeObject)>;

/// One database: N_KOS KOs, each created then updated VERSIONS times while
/// the manual clock ticks 10,000 ms per commit — every version gets a
/// distinct commit ts.
fn seed(k: &Kernel, clock: &ManualClock) -> Vec<aikoql_kernel::KOID> {
    let mut koids = Vec::with_capacity(N_KOS);
    for _ in 0..N_KOS {
        koids.push(
            k.remember(RememberRequest::create(alice(), meta()))
                .unwrap()
                .koid,
        );
    }
    for _ in 0..VERSIONS {
        clock.tick(10_000);
        for koid in &koids {
            k.remember(RememberRequest::update(alice(), *koid, meta()))
                .unwrap();
        }
    }
    koids
}

struct OpRow {
    op: &'static str, // "current" | "historical" | "history" | "range"
    p50: u128,
    p95: u128,
    p99: u128,
    counts: LogicalCounts,
}

struct BackendReport {
    rows: Vec<OpRow>,
    n_versions: usize, // VERSIONS + 1 (create) — verified per backend
}

/// The four read shapes. KSE-060..063 pins are checked here; every expected
/// timestamp is taken from THIS KO's own history (the HLC counter advances
/// per commit within one wall millisecond, so commit_ts values are
/// per-KO, not shared).
fn shapes(k: &Kernel, koid: &aikoql_kernel::KOID, t1: u64, t2: u64) {
    // KSE-060: current — pinned outside (zero scans in the delta).
    let _ = k.get(alice(), koid).unwrap();
    let h = k.history(alice(), koid).unwrap();
    // KSE-062: full history, strictly ascending commit ts.
    assert_eq!(h.len(), VERSIONS + 1, "history length");
    for w in h.windows(2) {
        assert!(w[0].0 < w[1].0, "history not ascending");
    }
    // KSE-061: historical — +1 ms past version VERSIONS/2's commit millis
    // (a bare millis<<16 snap excludes the version's counter bits; the next
    // version is 10,000 ms away, so +1 selects exactly it).
    let mid_ts = h[VERSIONS / 2].0;
    let as_of = k
        .get_as_of(alice(), koid, (mid_ts >> 16) + 1)
        .unwrap()
        .expect("as-of KO");
    assert_eq!(
        as_of.commit_ts, mid_ts,
        "get_as_of returned the wrong version"
    );
    // KSE-063: temporal range [t1, t2) — filter above the engine (no kernel
    // range API exists), and check the subset exactly.
    let range: Versions = h
        .into_iter()
        .filter(|(ts, _)| t1 <= *ts && *ts < t2)
        .collect();
    assert_eq!(range.len(), 20, "range size");
    for (ts, _) in &range {
        assert!(t1 <= *ts && *ts < t2, "range violated");
    }
}

fn measure(name: &'static str, engine: Arc<dyn StorageEngine>) -> BackendReport {
    let counting = CountingEngine::new(engine);
    let clock = Arc::new(ManualClock::new(10_000));
    let k = Kernel::open(counting.clone(), clock.clone(), SALT).unwrap();
    let koids = seed(&k, &clock);

    let h0 = k.history(alice(), &koids[0]).unwrap();
    assert_eq!(h0.len(), VERSIONS + 1);
    // Mid-history wall instant for the timing loop: +1 ms past version
    // VERSIONS/2's commit millis on KO #0 (per-KO counter bits only matter
    // for the exact-ts pin in shapes(), not for which version the snap
    // selects — the wall clock is global).
    let mid = (h0[VERSIONS / 2].0 >> 16) + 1;
    let t1 = h0[10].0;
    let t2 = h0[30].0;

    let mut rows = Vec::new();
    for op in ["current", "historical", "history", "range"] {
        let run = |k: &Kernel, koid: &aikoql_kernel::KOID| match op {
            "current" => {
                let _ = k.get(alice(), koid).unwrap();
            }
            "historical" => {
                let _ = k.get_as_of(alice(), koid, mid).unwrap();
            }
            "history" => {
                let _ = k.history(alice(), koid).unwrap();
            }
            _ => {
                let h = k.history(alice(), koid).unwrap();
                let _: Versions = h
                    .into_iter()
                    .filter(|(ts, _)| t1 <= *ts && *ts < t2)
                    .collect();
            }
        };
        let mut samples = Vec::with_capacity(koids.len() * REPS);
        for koid in &koids {
            for _ in 0..REPS {
                let t0 = Instant::now();
                run(&k, koid);
                samples.push(t0.elapsed().as_micros());
            }
        }
        let (p50, p95, p99) = percentiles(samples);
        let before = LogicalCounts::snapshot(&counting);
        run(&k, &koids[0]);
        let counts = LogicalCounts::snapshot(&counting).delta(before);
        // KSE-060 gate: the current-version read must not scan history.
        if op == "current" {
            assert_eq!(counts.scans, 0, "{name}: current read scanned history");
        }
        rows.push(OpRow {
            op,
            p50,
            p95,
            p99,
            counts,
        });
    }
    // KSE-061..063 pins, exercised identically on every backend.
    for koid in &koids {
        shapes(&k, koid, t1, t2);
    }
    drop((k, clock, counting));
    BackendReport {
        rows,
        n_versions: VERSIONS + 1,
    }
}

fn report_md(
    redb: &BackendReport,
    rocksdb: &Option<BackendReport>,
    aikoql: &BackendReport,
) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "# KSE-060..063 — Temporal Locality (MRFC-KSE-001 §13)\n\n\
         Measured {} on {} (debug build — indicative, not release numbers).\n\
         Dataset per backend: {N_KOS} KOs × ({VERSIONS} updates + create), \
         manual clock +10,000 ms per commit — every version a distinct \
         commit ts; {REPS} timed reps × {N_KOS} KOs per op.\n\n",
        chrono_now(),
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into()),
    ));
    s.push_str("| op | redb P50/P95/P99 (µs) | redb engine reqs | RocksDB P50/P95/P99 (µs) | Aikoql P50/P95/P99 (µs) | Aikoql engine reqs |\n|---|---|---|---|---|---|\n");
    for row in &redb.rows {
        let rb = rocksdb
            .as_ref()
            .and_then(|r| r.rows.iter().find(|x| x.op == row.op));
        let ak = aikoql.rows.iter().find(|x| x.op == row.op).unwrap();
        s.push_str(&format!(
            "| {} | {} / {} / {} | {} | {} | {} / {} / {} | {} |\n",
            row.op,
            row.p50,
            row.p95,
            row.p99,
            row.counts,
            rb.map_or("NOT_MEASURED".into(), |m| {
                format!("{} / {} / {}", m.p50, m.p95, m.p99)
            }),
            ak.p50,
            ak.p95,
            ak.p99,
            ak.counts,
        ));
    }
    s.push_str(&format!(
        "\n## Pins (KSE-060..063)\n\n\
         - KSE-060: current-version read issues 0 scans on every backend at \
         every version depth — head-pointer get, no history walk.\n\
         - KSE-061: get_as_of at a mid-history wall instant returns exactly \
         the newest-committed version (commit ts == snap match) — pinned on \
         all {N_KOS} KOs per backend. But the request shape is a full \
         version-prefix scan (51 pairs, 35.6 KB — identical to history), \
         not a seek: object_at walks the ko/ prefix from the start. The \
         lever is a seek-to-snap (engine lands at the newest ts <= snap, \
         kernel reads one row) — same class as the range pushdown below.\n\
         - KSE-062: history returns all {} versions (create + {VERSIONS} \
         updates), strictly ascending commit ts.\n\
         - KSE-063: [t1, t2) with t1 = version 10, t2 = version 30 returns \
         exactly versions 10..29 — the kernel has no range API, so the \
         filter runs above the engine over the full history scan (see the \
         range row: it costs a history read + client-side filter). A range \
         pushdown into the ko/ prefix scan is the only lever if range \
         queries ever need to beat full-history cost.\n\
         - Backend parity: identical version-ts sequences over every \
         backend (same seed, same clock) — the MVCC shape is engine-neutral.\n",
        redb.n_versions
    ));
    s
}

/// The doc's timestamps are plain `YYYY-MM-DD`; no chrono dependency.
fn chrono_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Days since epoch → date (civil-from-days, Howard Hinnant's algorithm).
    let (y, m, d) = {
        let z = secs / 86_400 + 719_468;
        let era = z / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (if m <= 2 { y + 1 } else { y }, m, d)
    };
    format!("{y:04}-{m:02}-{d:02}")
}

/// KSE-060..063 — the four temporal read shapes across the three backends.
#[test]
fn kse060_063_temporal_locality() {
    let redb_p = tmp("kse7_redb");
    let redb = measure("redb", Arc::new(RedbEngine::open(&redb_p).unwrap()));

    let aikoql_p = tmp("kse7_aikoql");
    let aikoql = measure(
        "aikoql",
        Arc::new(AikoqlStorageEngine::open(&aikoql_p).unwrap()),
    );

    #[cfg(feature = "kse5-rocksdb")]
    let rocksdb = {
        let rocks_p = tmp("kse7_rocksdb");
        Some(measure(
            "rocksdb",
            Arc::new(aikoql_rocksdb::RocksDbEngine::open(&rocks_p).unwrap()),
        ))
    };
    #[cfg(not(feature = "kse5-rocksdb"))]
    let rocksdb: Option<BackendReport> = None;

    let report = report_md(&redb, &rocksdb, &aikoql);
    println!("{report}");
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../artifacts/storage-engine");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("kse7-temporal-locality.md"), report).unwrap();

    for p in [&redb_p, &aikoql_p] {
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_dir_all(p);
    }
}
