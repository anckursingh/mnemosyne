//! KSE-5 — knowledge locality (MRFC-KSE-001 §11).
//!
//! KSE-040: the cost of one logical KO retrieval — KO + facts (payload) +
//! relationships (adjacency) + provenance + version history — measured over
//! the same seeded dataset on redb, RocksDB, and AikoqlStorageEngine.
//!
//! What is honestly measurable per backend (the report carries the same
//! caveats):
//! - logical requests: kernel→engine get/scan calls per retrieval, counted
//!   by a pass-through CountingEngine. Identical across backends by kernel
//!   construction — the test PINS the equality (a divergence would mean the
//!   kernel behaves differently per backend, which §32 forbids).
//! - physical records/blocks + bytes: redb reports leaf/branch pages via
//!   Database::stats; Aikoql reads nothing at retrieval time (all state is
//!   in RAM) but replays 100% of the write history at open — its durable
//!   cost is measured as WAL records/bytes + reopen time; RocksDB per-read
//!   IO counters are not wired (perf context off) — resident bytes = the
//!   sum of SST files.
//! - P50/P95/P99: microseconds per retrieval, 20 reps × 100 KOs per backend.
//!
//! Timing runs are DEBUG-build indicative numbers — no timing assertions
//! (they would flake); the gate is the equality pin + the report file.

mod common;

use aikoql_kernel::knowledge::kom::Value;
use aikoql_kernel::storage::store_redb::RedbEngine;
use aikoql_kernel::transaction::kernel::ManualClock;
use aikoql_kernel::{Direction, Kernel, Metadata, RelationshipRef, RememberRequest, Subject};
use aikoql_storage::AikoqlStorageEngine;
use common::{percentiles, tmp, CountingEngine, LogicalCounts};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

const N_KOS: usize = 100;
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

struct Measurement {
    per_retrieval: LogicalCounts,
    writes: (u64, u64, u64), // write_batches, puts, dels
    p50: u128,
    p95: u128,
    p99: u128,
    store_bytes: u64,          // durable bytes the backend owns on disk
    live_bytes: Option<u64>,   // bytes of live KV data (redb / aikoql full scan)
    pages: Option<(u64, u64)>, // (leaf, branch) — redb only
    reopen_ms: Option<u128>,   // WAL replay at open — aikoql only
}

/// The dataset: N KOs, each = create (3 fact payload props + provenance) +
/// relationships (3 outbound links, ring) + 2 further updates (version
/// history). Retrieval = get + history + outbound_edges + inbound_edges.
fn seed(k: &Kernel) -> Vec<aikoql_kernel::KOID> {
    let mut koids = Vec::with_capacity(N_KOS);
    for i in 0..N_KOS {
        let mut req = RememberRequest::create(alice(), meta());
        for f in 0..3 {
            req.properties.insert(
                format!("fact-{f}"),
                Value::Text(format!(
                    "kse5 fact #{i} item {f}: payload bytes that make the retrieval cost real"
                )),
            );
        }
        req.properties
            .insert("provenance".into(), Value::Text(format!("kse5-src:{i}")));
        koids.push(k.remember(req).unwrap().koid);
    }
    for i in 0..N_KOS {
        let mut req = RememberRequest::update(alice(), koids[i], meta());
        for r in 1..=3 {
            req.relationships.push(RelationshipRef {
                rel_type: "links".into(),
                target: koids[(i + r) % N_KOS],
                direction: Direction::Outbound,
            });
        }
        k.remember(req).unwrap();
    }
    for koid in &koids {
        for _ in 0..2 {
            k.remember(RememberRequest::update(alice(), *koid, meta()))
                .unwrap();
        }
    }
    koids
}

fn retrieval(k: &Kernel, koid: &aikoql_kernel::KOID) {
    let _ = k.get(alice(), koid).unwrap();
    let _ = k.history(alice(), koid).unwrap();
    let _ = k.outbound_edges(koid, None).unwrap();
    let _ = k.inbound_edges(koid, None).unwrap();
}

/// Time `REPS` retrievals over every KO; returns the sorted samples.
fn time_retrievals(k: &Kernel, koids: &[aikoql_kernel::KOID]) -> Vec<u128> {
    let mut samples = Vec::with_capacity(koids.len() * REPS);
    for koid in koids {
        for _ in 0..REPS {
            let t0 = Instant::now();
            retrieval(k, koid);
            samples.push(t0.elapsed().as_micros());
        }
    }
    samples
}

/// One retrieval, returning the engine-level request counts (delta).
fn per_retrieval_counts(
    counting: &CountingEngine,
    k: &Kernel,
    koid: &aikoql_kernel::KOID,
) -> LogicalCounts {
    let before = LogicalCounts::snapshot(counting);
    retrieval(k, koid);
    LogicalCounts::snapshot(counting).delta(before)
}

/// Measure the AikoqlStorageEngine backend. Returns (measurement, report
/// notes) — notes carry the replay story.
fn measure_aikoql(p: &Path, counting: Arc<CountingEngine>) -> Measurement {
    let clock = Arc::new(ManualClock::new(10_000));
    let k = Kernel::open(counting.clone(), clock.clone(), SALT).unwrap();
    let koids = seed(&k);
    let per_retrieval = per_retrieval_counts(&counting, &k, &koids[0]);
    let (p50, p95, p99) = percentiles(time_retrievals(&k, &koids));
    let writes = LogicalCounts::writes(&counting);
    // Live bytes = the whole map, summed from a full scan.
    let live_bytes: u64 = counting
        .inner
        .scan(b"")
        .unwrap()
        .iter()
        .map(|(k, v)| (k.len() + v.len()) as u64)
        .sum();
    let wal_bytes = std::fs::metadata(p).unwrap().len();
    drop((koids, k, clock, counting));
    // Reopen replay: the durable cost Aikoql pays at open.
    let t0 = Instant::now();
    let e2 = AikoqlStorageEngine::open(p).unwrap();
    let reopen_ms = t0.elapsed().as_millis();
    drop(e2);
    Measurement {
        per_retrieval,
        writes,
        p50,
        p95,
        p99,
        store_bytes: wal_bytes,
        live_bytes: Some(live_bytes),
        pages: None,
        reopen_ms: Some(reopen_ms),
    }
}

/// Measure the redb backend; physical pages/bytes come from Database::stats.
fn measure_redb(p: &Path, counting: Arc<CountingEngine>) -> Measurement {
    let clock = Arc::new(ManualClock::new(10_000));
    let k = Kernel::open(counting.clone(), clock.clone(), SALT).unwrap();
    let koids = seed(&k);
    let per_retrieval = per_retrieval_counts(&counting, &k, &koids[0]);
    let (p50, p95, p99) = percentiles(time_retrievals(&k, &koids));
    let writes = LogicalCounts::writes(&counting);
    drop((koids, k, clock, counting)); // release the redb lock before stats
    let db = redb::Database::open(p).unwrap();
    let stats = db.begin_write().unwrap().stats().unwrap();
    Measurement {
        per_retrieval,
        writes,
        p50,
        p95,
        p99,
        store_bytes: std::fs::metadata(p).unwrap().len(),
        live_bytes: Some(stats.stored_bytes()),
        pages: Some((stats.leaf_pages(), stats.branch_pages())),
        reopen_ms: None,
    }
}

/// Measure RocksDB (opt-in feature). Per-read IO counters are not wired —
/// resident bytes = the sum of SST files in the store directory.
#[cfg(feature = "kse5-rocksdb")]
fn measure_rocksdb(p: &Path, counting: Arc<CountingEngine>) -> Measurement {
    let clock = Arc::new(ManualClock::new(10_000));
    let k = Kernel::open(counting.clone(), clock.clone(), SALT).unwrap();
    let koids = seed(&k);
    let per_retrieval = per_retrieval_counts(&counting, &k, &koids[0]);
    let (p50, p95, p99) = percentiles(time_retrievals(&k, &koids));
    let writes = LogicalCounts::writes(&counting);
    drop((koids, k, clock, counting));
    let store_bytes: u64 = std::fs::read_dir(p)
        .unwrap()
        .map(|e| std::fs::metadata(e.unwrap().path()).unwrap().len())
        .sum();
    Measurement {
        per_retrieval,
        writes,
        p50,
        p95,
        p99,
        store_bytes,
        live_bytes: None,
        pages: None,
        reopen_ms: None,
    }
}

fn report_md(redb: &Measurement, rocksdb: &Option<Measurement>, aikoql: &Measurement) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "# KSE-040 — KO Read Amplification (MRFC-KSE-001 §11)\n\n\
         Measured {} on {} (debug build — indicative, not release numbers).\n\
         Dataset: {N_KOS} KOs × (create with 3 fact payload props + provenance \
         marker, rels update with 3 outbound links, 2 version updates); \
         retrieval = get + history + outbound_edges + inbound_edges; \
         {REPS} reps × {N_KOS} KOs timed per backend.\n\n",
        chrono_now(),
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into()),
    ));
    s.push_str("| metric | redb | RocksDB | Aikoql |\n|---|---|---|---|\n");
    s.push_str(&format!(
        "| logical requests / retrieval | {} | {} | {} |\n",
        redb.per_retrieval,
        rocksdb
            .as_ref()
            .map_or("NOT_MEASURED".into(), |m| m.per_retrieval.to_string()),
        aikoql.per_retrieval
    ));
    s.push_str(&format!(
        "| logical writes (seed) | {} batches, {} puts, {} dels | same | same |\n",
        redb.writes.0, redb.writes.1, redb.writes.2
    ));
    s.push_str(&format!(
        "| physical records (read time) | {} leaf pages | NOT_MEASURED (perf context off) | 0 (RAM; {} WAL records replayed at open) |\n",
        redb.pages.unwrap_or((0, 0)).0, aikoql.writes.0
    ));
    s.push_str(&format!(
        "| physical blocks | {} leaf + {} branch pages | NOT_MEASURED | 0 (RAM) |\n",
        redb.pages.unwrap_or((0, 0)).0,
        redb.pages.unwrap_or((0, 0)).1
    ));
    s.push_str(
        "| bytes read / retrieval | NOT_MEASURED (mmap, no IO tracing) | NOT_MEASURED | 0 (RAM after replay) |\n",
    );
    s.push_str(&format!(
        "| durable store bytes | {} (live {}) | {} | {} (live {}, amplification {:.2}×) |\n",
        redb.store_bytes,
        redb.live_bytes.unwrap_or(0),
        rocksdb
            .as_ref()
            .map_or("NOT_MEASURED".into(), |m| m.store_bytes.to_string()),
        aikoql.store_bytes,
        aikoql.live_bytes.unwrap_or(1),
        aikoql.store_bytes as f64 / aikoql.live_bytes.unwrap_or(1) as f64
    ));
    s.push_str(&format!(
        "| P50 / P95 / P99 (µs) | {} / {} / {} | {} | {} / {} / {} |\n",
        redb.p50,
        redb.p95,
        redb.p99,
        rocksdb.as_ref().map_or("NOT_MEASURED".into(), |m| {
            format!("{} / {} / {}", m.p50, m.p95, m.p99)
        }),
        aikoql.p50,
        aikoql.p95,
        aikoql.p99
    ));
    s.push_str(&format!(
        "| reopen cost | 0 (lazy mmap) | 0 | {} ms replay of {} WAL records ({} bytes) |\n",
        aikoql.reopen_ms.unwrap_or(0),
        aikoql.writes.0,
        aikoql.store_bytes
    ));
    s.push_str(
        "\n## Read\n\n\
         - The kernel issues the SAME logical requests over every backend \
         (pinned by the test) — locality is purely physical.\n\
         - Aikoql's read path is 0-disk (RAM) but it pays for that at open: \
         the whole write history replays every restart (the unbounded-log \
         ponytail in lib.rs). Amplification above = WAL bytes / live bytes; \
         it grows with every version commit.\n\
         - redb pays lazily (page faults during reads) and keeps only live \
         pages on disk.\n\
         - RocksDB per-read IO is unmeasured until perf-context counters are \
         wired (feature `kse5-rocksdb` covers latency + resident bytes only).\n",
    );
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

/// KSE-040 — KO read amplification across the three backends.
#[test]
fn kse040_ko_read_amplification() {
    // redb
    let redb_p = tmp("redb");
    let counting = CountingEngine::new(Arc::new(RedbEngine::open(&redb_p).unwrap()));
    let redb = measure_redb(&redb_p, counting);

    // Aikoql
    let aikoql_p = tmp("aikoql");
    let counting = CountingEngine::new(Arc::new(AikoqlStorageEngine::open(&aikoql_p).unwrap()));
    let aikoql = measure_aikoql(&aikoql_p, counting);

    // RocksDB — strict opt-in: measured only with the feature, reported as
    // NOT_MEASURED otherwise (never silently skipped).
    #[cfg(feature = "kse5-rocksdb")]
    let rocksdb = {
        let rocks_p = tmp("rocksdb");
        let counting = CountingEngine::new(Arc::new(
            aikoql_rocksdb::RocksDbEngine::open(&rocks_p).unwrap(),
        ));
        Some(measure_rocksdb(&rocks_p, counting))
    };
    #[cfg(not(feature = "kse5-rocksdb"))]
    let rocksdb: Option<Measurement> = None;

    // PIN: the kernel issues identical logical requests over every backend
    // (§32: no backend-specific behavior above the boundary). A divergence
    // here would mean a semantic difference, not a locality difference.
    assert_eq!(
        redb.per_retrieval, aikoql.per_retrieval,
        "logical requests must not depend on the backend"
    );
    if let Some(r) = &rocksdb {
        assert_eq!(redb.per_retrieval, r.per_retrieval);
        assert_eq!(redb.writes, r.writes);
    }
    assert_eq!(redb.writes, aikoql.writes, "seed workload must match");

    let report = report_md(&redb, &rocksdb, &aikoql);
    println!("{report}");
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../artifacts/storage-engine");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("kse5-locality.md"), report).unwrap();

    for p in [&redb_p, &aikoql_p] {
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_dir_all(p);
    }
}
