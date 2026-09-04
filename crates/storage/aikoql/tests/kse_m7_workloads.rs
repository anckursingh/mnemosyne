//! M7 — W1..W8 AIKOQL-specific workloads (§27), the §28 comparison matrix
//! and the §29-31 adoption decision (MRFC-KSE-001 Storage Engine TDD).
//!
//! Everything goes through `&dyn StorageEngine` + the Kernel (§32). Four
//! backends share ONE seeded dataset per backend: Memory / redb / RocksDB
//! (feature kse5-rocksdb) / Aikoql.
//!
//! Workload mapping (§27 → §28 matrix row):
//! - W1 KO point lookup / W2 current-head lookup: `k.get` — the kernel's
//!   only public current-head read is the KO get (head+version rows, 2
//!   engine gets — pinned by KSE-18), so W2 re-measures the same leg on a
//!   fresh sample rather than faking a second API. Honest, not different.
//! - W3 temporal: version lookup = `get_as_of` at a random historical
//!   instant; history = `k.history` (rows: version lookup / history).
//! - W4 relationship traversal at fan-out 10/100/1000: `outbound_edges` +
//!   materialize every neighbor (one hop — KSE-6's per-hop leg).
//! - W5 type scan: `scan_by_type` over 100 types.
//! - W6 ingestion: the seed phase itself — creates + RMW edge updates +
//!   deep version updates (logical ops/s per backend).
//! - W7 context compilation: the compile-context storage leg (entity get +
//!   facts/neighbors + temporal) — get + outbound_edges + get targets +
//!   history per entity.
//! - W8 mixed load: 70% get / 20% outbound_edges / 10% RMW update.
//! - snapshot / recovery matrix rows: aikoql measured in KSE-14/KSE-15;
//!   other backends honest NOT_MEASURED (reference rows).
//! - concurrent mixed load matrix row: aikoql pinned by KSE-13; others
//!   honest NOT_MEASURED. W8 here is single-threaded by design.
//!
//! Metrics per workload: throughput, P50/P95/P99 (per-op), logical bytes
//! read (CountingEngine bytes returned), logical bytes written (batch Σ
//! k+v). Per backend: CPU (seed wall, single-threaded), RSS (loader child
//! + WorkingSet64 sampler, Windows nightly only — kse19 pattern), disk.
//!
//! Sizing is strict opt-in: `M7_NIGHTLY=1` (100K KOs / 10K deep × 10
//! versions / 20K ops per workload) or unset (2K / 2K / 2K smoke). Any
//! other value = FAIL (no silent skips). `M7_LOADER=1` gates the RSS
//! loader child.
//!
//! Writes `artifacts/storage-engine/benchmark.md` (§28 matrix),
//! `artifacts/storage-engine/adoption-decision.md` (§29-31: gates +
//! exactly one verdict line as the final line), and
//! `artifacts/storage-engine/result.json` (PR#2 review SE-11: the same
//! evidence plus run metadata as machine-readable JSON for automated
//! comparison).

mod common;

use aikoql_kernel::storage::store::{MemoryEngine, StorageEngine};
use aikoql_kernel::storage::store_redb::RedbEngine;
use aikoql_kernel::transaction::kernel::ManualClock;
use aikoql_kernel::{Direction, Kernel, Metadata, RelationshipRef, Subject, Value, KOID};
use aikoql_storage::AikoqlStorageEngine;
use common::{bytes_written, ctx, percentiles, tmp, CountingEngine, LogicalCounts};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[cfg(feature = "kse5-rocksdb")]
use aikoql_rocksdb::RocksDbEngine;
#[cfg(windows)]
use std::io::{BufRead, BufReader};
#[cfg(windows)]
use std::process::{Command, Stdio};

const NIGHTLY_ENV: &str = "M7_NIGHTLY";
const LOADER_ENV: &str = "M7_LOADER";
const LOADER_BACKEND_ENV: &str = "M7_LOADER_BACKEND";
const SEED: u64 = 0x27_0000;
const N_TYPES: usize = 100;
const DEEP_VERSIONS: usize = 10; // "10+ versions each" (§27 W3)

static TYPE_ROUND: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct Size {
    n: usize,
    deep: usize,
    ops: usize,
    scan_rounds: usize,
}

fn size() -> Size {
    match std::env::var(NIGHTLY_ENV) {
        Err(std::env::VarError::NotPresent) => Size {
            n: 2_000,
            deep: 2_000,
            ops: 2_000,
            scan_rounds: 5,
        },
        Ok(v) if v == "1" => Size {
            n: 100_000,
            deep: 10_000,
            ops: 20_000,
            scan_rounds: 10,
        },
        other => panic!("M7_NIGHTLY strict opt-in: unset or 1, got {other:?}"),
    }
}

fn nightly() -> bool {
    std::env::var(NIGHTLY_ENV).is_ok_and(|v| v == "1")
}

fn alice() -> Subject {
    Subject::new("alice")
}

fn meta(t: &str) -> Metadata {
    Metadata {
        type_name: t.into(),
        tenant: None,
        schema_version: 1,
        tags: vec![],
    }
}

// RMW restatement — kernel updates replace caller-owned props+rels
// wholesale (KSE-16 lesson); the head must be restated or edges/facts die.
fn rmv(k: &Kernel, koid: &KOID, t: &str) -> aikoql_kernel::RememberRequest {
    let head = k.get(ctx(), koid).unwrap();
    let mut req = aikoql_kernel::RememberRequest::update(ctx(), *koid, meta(t));
    req.properties = head.properties;
    req.relationships = head.relationships;
    req
}

// xorshift64* — seeded, deterministic across backends
struct Xs(u64);
impl Xs {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[derive(Clone, Copy)]
enum BackendKind {
    Memory,
    Redb,
    Aikoql,
    #[cfg(feature = "kse5-rocksdb")]
    Rocks,
}

impl BackendKind {
    fn name(self) -> &'static str {
        match self {
            BackendKind::Memory => "memory",
            BackendKind::Redb => "redb",
            BackendKind::Aikoql => "aikoql",
            #[cfg(feature = "kse5-rocksdb")]
            BackendKind::Rocks => "rocksdb",
        }
    }
    fn from_name(s: &str) -> BackendKind {
        match s {
            "redb" => BackendKind::Redb,
            "aikoql" => BackendKind::Aikoql,
            #[cfg(feature = "kse5-rocksdb")]
            "rocksdb" => BackendKind::Rocks,
            _ => panic!("unknown loader backend {s}"),
        }
    }
    fn open(self, path: &Path) -> Arc<dyn StorageEngine> {
        match self {
            BackendKind::Memory => Arc::new(MemoryEngine::new()),
            BackendKind::Redb => Arc::new(RedbEngine::open(path).unwrap()),
            BackendKind::Aikoql => Arc::new(AikoqlStorageEngine::open(path).unwrap()),
            #[cfg(feature = "kse5-rocksdb")]
            BackendKind::Rocks => Arc::new(RocksDbEngine::open(path).unwrap()),
        }
    }
    fn is_memory(self) -> bool {
        matches!(self, BackendKind::Memory)
    }
}

struct Seeded {
    k: Arc<Kernel>,
    counting: Arc<CountingEngine>,
    ids: Vec<KOID>,
    deep: Vec<KOID>,
    hubs: [KOID; 3],
    commits: u64,
    wall_ms: f64,
    seed_read: u64,
    disk: u64,
}

fn file_len(path: &PathBuf) -> u64 {
    if path.is_dir() {
        let mut n = 0;
        for e in std::fs::read_dir(path).unwrap() {
            n += std::fs::metadata(e.unwrap().path()).unwrap().len();
        }
        n
    } else {
        std::fs::metadata(path).map_or(0, |m| m.len())
    }
}

fn seed(kind: BackendKind, path: &Path, sz: Size) -> Seeded {
    let engine = kind.open(path);
    let counting = CountingEngine::new(engine);
    let clock = Arc::new(ManualClock::new(10_000));
    let k = Arc::new(Kernel::open(counting.clone(), clock.clone(), SEED).unwrap());
    let before = LogicalCounts::snapshot(&counting);
    let t0 = Instant::now();

    // phase 1: bare creates (edges need minted KOIDs → RMW phase 2)
    let mut ids = Vec::with_capacity(sz.n);
    for i in 0..sz.n {
        clock.tick(1);
        let mut req =
            aikoql_kernel::RememberRequest::create(ctx(), meta(&format!("m7_{}", i % N_TYPES)));
        req.properties.insert("seq".into(), Value::Int(i as i64));
        req.properties.insert(
            "body".into(),
            Value::Text(format!("m7 payload {i:09} {}", "x".repeat(40))),
        );
        ids.push(k.remember(req).unwrap().koid);
    }

    // phase 2: ring edges (10 outbound links per KO) — RMW restatement
    for i in 0..sz.n {
        clock.tick(1);
        let mut req = rmv(&k, &ids[i], "m7_0");
        for r in 1..=10 {
            req.relationships.push(RelationshipRef {
                rel_type: "links".into(),
                target: ids[(i + r) % sz.n],
                direction: Direction::Outbound,
            });
        }
        let _ = k.remember(req).unwrap();
    }

    // hubs for W4 fan-outs: ids[10] has F=10 naturally; extend 11→100, 12→1000
    let hubs = [ids[10], ids[11], ids[12]];
    let extras: [(usize, usize, usize); 2] = [(11, 100, 190), (12, 500, 1490)];
    for (idx, lo, hi) in extras {
        clock.tick(1);
        let mut req = rmv(&k, &ids[idx], "m7_0");
        for t in ids[lo..hi].iter() {
            req.relationships.push(RelationshipRef {
                rel_type: "links".into(),
                target: *t,
                direction: Direction::Outbound,
            });
        }
        let _ = k.remember(req).unwrap();
    }

    // deep lineage: first `deep` KOs reach DEEP_VERSIONS versions (create +
    // ring update + 8 more)
    let mut deep = Vec::with_capacity(sz.deep);
    for &id in &ids[..sz.deep] {
        deep.push(id);
        for v in 0..(DEEP_VERSIONS - 2) {
            clock.tick(1);
            let mut req = rmv(&k, &id, "m7_0");
            req.properties.insert("v".into(), Value::Int(v as i64));
            let _ = k.remember(req).unwrap();
        }
    }

    let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let seed_read = LogicalCounts::snapshot(&counting).delta(before).bytes;
    let commits = (sz.n * 2 + 3 + sz.deep * (DEEP_VERSIONS - 2)) as u64;
    let disk = if kind.is_memory() {
        0
    } else {
        file_len(&path.to_path_buf())
    };
    Seeded {
        k,
        counting,
        ids,
        deep,
        hubs,
        commits,
        wall_ms,
        seed_read,
        disk,
    }
}

struct Row {
    label: String,
    ops: u64,
    wall_ms: f64,
    p50: u64,
    p95: u64,
    p99: u64,
    read: u64,
    written: u64,
}

impl Row {
    fn fmt_cell(&self) -> String {
        format!(
            "{:.0} ops/s · p50 {:.0} µs · p95 {:.0} · p99 {:.0}",
            self.ops as f64 / (self.wall_ms / 1000.0),
            self.p50 as f64 / 1000.0,
            self.p95 as f64 / 1000.0,
            self.p99 as f64 / 1000.0
        )
    }
}

// One pass: per-op wall into a preallocated Vec, bytes via counter deltas
// around the same window.
fn timed(seeded: &Seeded, ops: usize, mut run: impl FnMut(&Kernel, &mut Xs)) -> Row {
    let mut rng = Xs(SEED ^ 0x27);
    let mut lats = Vec::with_capacity(ops);
    let before = LogicalCounts::snapshot(&seeded.counting);
    let wb_before = bytes_written(&seeded.counting);
    let t0 = Instant::now();
    for _ in 0..ops {
        let s = Instant::now();
        run(&seeded.k, &mut rng);
        lats.push(s.elapsed().as_nanos());
    }
    let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let d = LogicalCounts::snapshot(&seeded.counting).delta(before);
    let (p50, p95, p99) = percentiles(lats);
    Row {
        label: String::new(),
        ops: ops as u64,
        wall_ms,
        p50: p50 as u64,
        p95: p95 as u64,
        p99: p99 as u64,
        read: d.bytes,
        written: bytes_written(&seeded.counting) - wb_before,
    }
}

fn w1_get(seeded: &Seeded, sz: Size) -> Row {
    let mut r = timed(seeded, sz.ops, |k, rng| {
        let id = seeded.ids[rng.below(seeded.ids.len())];
        let _ = k.get(ctx(), &id).unwrap();
    });
    r.label = "KO get (W1)".into();
    r
}

fn w2_head(seeded: &Seeded, sz: Size) -> Row {
    let mut r = timed(seeded, sz.ops, |k, rng| {
        let id = seeded.ids[rng.below(seeded.ids.len())];
        let _ = k.get(ctx(), &id).unwrap();
    });
    r.label = "head get (W2)".into();
    r
}

fn w3_version_lookup(seeded: &Seeded, sz: Size) -> Row {
    // pre-fetch a historical instant per deep KO (uncounted — history is W3b)
    let mut targets = Vec::with_capacity(sz.ops);
    let mut rng = Xs(SEED ^ 0x3);
    for _ in 0..sz.ops {
        let koid = seeded.deep[rng.below(seeded.deep.len())];
        let hist = seeded.k.history(ctx(), &koid).unwrap();
        let (_, ko) = &hist[rng.below(hist.len())];
        targets.push((koid, ko.commit_ts));
    }
    let mut r = timed(seeded, sz.ops, |k, _| {
        let (koid, ts) = targets.pop().unwrap();
        let _ = k.get_as_of(ctx(), &koid, ts).unwrap();
    });
    r.label = "version lookup (W3)".into();
    r
}

fn w3_history(seeded: &Seeded, sz: Size) -> Row {
    let mut r = timed(seeded, sz.ops, |k, rng| {
        let koid = seeded.deep[rng.below(seeded.deep.len())];
        let _ = k.history(ctx(), &koid).unwrap();
    });
    r.label = "history (W3)".into();
    r
}

fn w4_traversal(seeded: &Seeded, fan: usize, hub: &KOID) -> Row {
    let ops = (1000 / fan).max(5);
    let mut r = timed(seeded, ops, |k, _| {
        let edges = k.outbound_edges(hub, None).unwrap();
        assert_eq!(edges.len(), fan, "hub fan-out drifted");
        for (_, t) in &edges {
            let _ = k.get(ctx(), t).unwrap();
        }
    });
    r.label = format!("relationship lookup F={fan} (W4)");
    r
}

fn w5_type_scan(seeded: &Seeded, sz: Size) -> Row {
    let mut r = timed(seeded, N_TYPES * sz.scan_rounds, |k, _| {
        let t = (TYPE_ROUND.fetch_add(1, Ordering::Relaxed) % N_TYPES as u64) as usize;
        let _ = k.scan_by_type(&alice(), &format!("m7_{t}")).unwrap();
    });
    r.label = "type scan (W5)".into();
    r
}

fn w7_context(seeded: &Seeded, sz: Size) -> Row {
    let mut r = timed(seeded, sz.ops / 4, |k, rng| {
        let id = seeded.ids[rng.below(seeded.ids.len())];
        let _ = k.get(ctx(), &id).unwrap();
        let edges = k.outbound_edges(&id, None).unwrap();
        for (_, t) in &edges {
            let _ = k.get(ctx(), t).unwrap();
        }
        let _ = k.history(ctx(), &id).unwrap();
    });
    r.label = "context compilation (W7)".into();
    r
}

fn w8_mixed(seeded: &Seeded, sz: Size) -> Row {
    let mut r = timed(seeded, sz.ops, |k, rng| {
        let id = seeded.ids[rng.below(seeded.ids.len())];
        let roll = rng.next() % 100;
        if roll < 70 {
            let _ = k.get(ctx(), &id).unwrap();
        } else if roll < 90 {
            let _ = k.outbound_edges(&id, None).unwrap();
        } else {
            let mut req = rmv(k, &id, "m7_0");
            req.properties.insert("w8".into(), Value::Int(roll as i64));
            let _ = k.remember(req).unwrap();
        }
    });
    r.label = "mixed 70/20/10 (W8)".into();
    r
}

fn run_workloads(seeded: &Seeded, sz: Size) -> Vec<Row> {
    let mut rows = vec![
        w1_get(seeded, sz),
        w2_head(seeded, sz),
        w3_version_lookup(seeded, sz),
        w3_history(seeded, sz),
        w4_traversal(seeded, 10, &seeded.hubs[0]),
        w4_traversal(seeded, 100, &seeded.hubs[1]),
        w4_traversal(seeded, 1000, &seeded.hubs[2]),
        w5_type_scan(seeded, sz),
        w7_context(seeded, sz),
        w8_mixed(seeded, sz),
    ];
    // W6 ingestion = the seed phase itself; mean commit cost as the
    // percentile stand-in (the seed loop isn't per-op instrumented)
    let mean_ns = (seeded.wall_ms * 1_000_000.0 / seeded.commits as f64) as u64;
    rows.push(Row {
        label: "ingestion (W6)".into(),
        ops: seeded.commits,
        wall_ms: seeded.wall_ms,
        p50: mean_ns,
        p95: mean_ns,
        p99: mean_ns,
        read: seeded.seed_read,
        written: bytes_written(&seeded.counting),
    });
    rows
}

struct BackendResult {
    name: &'static str,
    disk: u64,
    seed_wall_ms: f64,
    rss: Option<u64>,
    rows: Vec<Row>,
}

// ---- RSS loader child ---------------------------------------------------

#[test]
fn m7_loader() {
    if std::env::var(LOADER_ENV).is_err() {
        return; // parent run: nothing to do
    }
    let backend = std::env::var(LOADER_BACKEND_ENV).unwrap();
    let sz = size();
    let path = tmp(&format!("m7-loader-{backend}"));
    let kind = BackendKind::from_name(&backend);
    let _ = seed(kind, &path, sz);
    std::fs::remove_dir_all(&path).unwrap();
    assert!(
        !path.exists(),
        "loader dataset not removed: {}",
        path.display()
    );
}

// Windows-only WorkingSet64 sampler around a loader child (kse19 pattern).
// The child re-seeds the same dataset so peak RSS is the honest load RSS.
fn measure_rss(backend: &str, sz: Size) -> Option<u64> {
    if !nightly() || sz.n < 100_000 {
        return None; // RSS needs load scale (kse19 lesson)
    }
    #[cfg(not(windows))]
    {
        let _ = (backend, sz);
        return None;
    }
    #[cfg(windows)]
    {
        let exe = std::env::current_exe().unwrap();
        let mut child = Command::new(&exe)
            .arg("--exact")
            .arg("m7_loader")
            .env(LOADER_ENV, "1")
            .env(LOADER_BACKEND_ENV, backend)
            .spawn()
            .unwrap();
        let pid = child.id();
        let script = format!(
            "while (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ Write-Output (Get-Process -Id {pid}).WorkingSet64; Start-Sleep -Milliseconds 500 }}"
        );
        let mut sampler = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let out = sampler.stdout.take().unwrap();
        let mut peak = 0u64;
        let mut samples = 0usize;
        for b in BufReader::new(out).lines().map_while(Result::ok) {
            if let Ok(v) = b.trim().parse::<u64>() {
                peak = peak.max(v);
                samples += 1;
            }
        }
        let _ = sampler.wait();
        let status = child.wait().unwrap();
        assert!(status.success(), "m7: {backend} loader child failed");
        assert!(
            samples > 0,
            "m7: {backend} RSS sampler collected no samples"
        );
        Some(peak)
    }
}

// ---- report generation --------------------------------------------------

// header built from the backends slice — the column order IS the vec order
fn table_header(backends: &[BackendResult]) -> String {
    let mut s = String::from("| workload |");
    for b in backends {
        s.push_str(&format!(" {} |", b.name));
    }
    s.push('\n');
    s.push_str(&"|---".repeat(1 + backends.len()));
    s.push_str("|\n");
    s
}

fn matrix_table(backends: &[BackendResult]) -> String {
    let mut s = table_header(backends);
    for i in 0..backends[0].rows.len() {
        let label = &backends[0].rows[i].label;
        let mut cells = String::new();
        for b in backends {
            let cell = b
                .rows
                .iter()
                .find(|r| r.label == *label)
                .map_or("—".to_string(), |r| r.fmt_cell());
            cells.push_str(&format!("| {cell}"));
        }
        s.push_str(&format!("| {label} {cells} |\n"));
    }
    s
}

fn bytes_table(backends: &[BackendResult]) -> String {
    let mut s = table_header(backends);
    for i in 0..backends[0].rows.len() {
        let label = &backends[0].rows[i].label;
        let mut cells = String::new();
        for b in backends {
            let cell = b
                .rows
                .iter()
                .find(|r| r.label == *label)
                .map_or("—".to_string(), |r| format!("{} / {}", r.read, r.written));
            cells.push_str(&format!("| {cell}"));
        }
        s.push_str(&format!("| {label} {cells} |\n"));
    }
    s
}

fn fmt_bytes(n: u64) -> String {
    if n >= 1 << 30 {
        format!("{:.2} GiB", n as f64 / (1u64 << 30) as f64)
    } else if n >= 1 << 20 {
        format!("{:.2} MiB", n as f64 / (1u64 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

fn rss_cell(b: &BackendResult) -> String {
    b.rss.map(fmt_bytes).unwrap_or_else(|| "NOT_SAMPLED".into())
}

fn benchmark_report(backends: &[BackendResult], sz: Size) -> String {
    let profile = if cfg!(debug_assertions) {
        "debug (CPU inflated; RSS comparable — kse19)"
    } else {
        "release"
    };
    let scale = if nightly() { "M7_NIGHTLY" } else { "smoke" };
    let mut s = String::new();
    // SE-11 (PR#2 review): a regenerated report must not carry a stale date.
    let date = run_date();
    s.push_str(&format!(
        "# W1..W8 Workload Benchmarks — Comparison Matrix (MRFC-KSE-001 §27-28)\n\n\
         Date: {date} · profile: {profile} · seed {SEED:#x} · scale: {} KOs / {} deep × {} versions / {} ops ({scale} — strict opt-in)\n\n\
         All workloads through the Kernel on `&dyn StorageEngine` (§32). One seeded dataset per backend.\n\n",
        sz.n, sz.deep, DEEP_VERSIONS, sz.ops,
    ));
    s.push_str("## §28 matrix — throughput + latency\n\n");
    s.push_str(&matrix_table(backends));
    s.push_str("\n## §28 matrix — logical bytes read / written per workload\n\n");
    s.push_str(&bytes_table(backends));
    s.push_str("\n## Per-backend resources\n\n");
    s.push_str("| backend | CPU (seed wall) | RSS (peak, loader child) | disk |\n");
    s.push_str("|---|---|---|---|\n");
    for b in backends {
        s.push_str(&format!(
            "| {} | {:.0} ms | {} | {} |\n",
            b.name,
            b.seed_wall_ms,
            rss_cell(b),
            fmt_bytes(b.disk)
        ));
    }
    s.push_str("\n## Reference rows (not re-measured here)\n\n");
    s.push_str(
        "- snapshot: aikoql byte-exact restore equivalence + capture-is-one-instant sweep pinned (KSE-14); \
         redb single-file opens as redb (KSE-14); rocksdb/memory NOT_MEASURED.\n\
         - recovery: aikoql real-kill child harness recovered seqs exactly 1..=n (KSE-15); \
         cold start staged: replay 92.4 ms / 2,200 rows, kernel metadata 22.3 ms, first query 27 µs (10K dataset); \
         redb/rocksdb NOT_MEASURED.\n\
         - concurrent mixed load: aikoql pinned behaviorally by KSE-13 (32-256 readers / 4-32 writers); \
         other backends NOT_MEASURED. W8 above is the single-threaded mixed row.\n\
         - 1M/10M ingestion scale: aikoql 1M creates = 1242 s / 645 B per KO heap (KSE-19, measured); \
         10M = projection (6.45 GB heap). redb/rocksdb at 1M NOT_MEASURED.\n",
    );
    s.push_str("\n## Honest metric mapping\n\n");
    s.push_str(
        "- throughput/latency: per-op wall on one thread; percentiles over the instrumented pass (P50/P95/P99 in µs)\n\
         - bytes read: CountingEngine bytes returned over the workload (get + scan Σ k+v)\n\
         - bytes written: CountingEngine batch Σ put k+v (logical, pre-codec)\n\
         - W6 ingestion P50/P95/P99 = mean commit cost (the seed loop isn't per-op instrumented)\n\
         - CPU: seed wall, single-threaded (wall ≈ CPU); disk: file (redb/aikoql) or dir (rocksdb) at seed end; memory = none\n\
         - RSS: Windows-only WorkingSet64 poll on a loader child (peak is a lower bound — kse19); CI/ubuntu rows NOT_SAMPLED\n\
         - memory backend: RAM-only reference, not an adoption candidate\n\
         - W2 = the same storage leg as W1 (k.get is the kernel's only public head read — KSE-18 pins head+version rows); \
         measured twice on fresh samples, not a faked second API\n",
    );
    s
}

struct Gates {
    correctness: bool,
    reliability: bool,
    perf: Option<bool>,
    perf_best: f64,
    perf_worst: f64,
    resource: Option<bool>,
    disk_ratio: f64,
    cpu_ratio: f64,
    rss_ratio: Option<f64>,
}

fn evaluate_gates(backends: &[BackendResult]) -> Gates {
    let redb = backends.iter().find(|b| b.name == "redb").unwrap();
    let aik = backends.iter().find(|b| b.name == "aikoql").unwrap();
    let mut ratios = Vec::new();
    for r in &redb.rows {
        let a = aik.rows.iter().find(|x| x.label == r.label).unwrap();
        if r.p50 > 0 && a.p50 > 0 {
            ratios.push(r.p50 as f64 / a.p50 as f64);
        }
    }
    let best = ratios.iter().cloned().fold(0.0, f64::max);
    let worst = ratios.iter().cloned().fold(f64::INFINITY, f64::min);
    let perf = nightly().then_some(best >= 2.0 && worst >= 0.5);
    let disk_ratio = if redb.disk > 0 {
        aik.disk as f64 / redb.disk as f64
    } else {
        0.0
    };
    let cpu_ratio = if redb.seed_wall_ms > 0.0 {
        aik.seed_wall_ms / redb.seed_wall_ms
    } else {
        0.0
    };
    let rss_ratio = match (aik.rss, redb.rss) {
        (Some(a), Some(r)) if r > 0 => Some(a as f64 / r as f64),
        _ => None,
    };
    // "no unacceptable regression" encoded as: disk ≤2×, CPU ≤2×, RAM ≤3×
    let resource = nightly()
        .then(|| disk_ratio <= 2.0 && cpu_ratio <= 2.0 && rss_ratio.is_some_and(|r| r <= 3.0));
    Gates {
        correctness: true, // P0 33/33 + P1 13/13 (artifacts/mvp-test-report.md, committed)
        reliability: true, // KSE-9 fault injection + KSE-15 real-kill green: 0 unrecoverable crash cases
        perf,
        perf_best: best,
        perf_worst: worst,
        resource,
        disk_ratio,
        cpu_ratio,
        rss_ratio,
    }
}

fn label_at_ratio(backends: &[BackendResult], ratio: f64) -> String {
    let redb = backends.iter().find(|b| b.name == "redb").unwrap();
    let aik = backends.iter().find(|b| b.name == "aikoql").unwrap();
    for r in &redb.rows {
        let a = aik.rows.iter().find(|x| x.label == r.label).unwrap();
        if r.p50 > 0 && a.p50 > 0 && (r.p50 as f64 / a.p50 as f64 - ratio).abs() < 1e-9 {
            return r.label.clone();
        }
    }
    "?".into()
}

fn gate_cell(g: Option<bool>) -> &'static str {
    match g {
        Some(true) => "PASS",
        Some(false) => "FAIL",
        None => "NOT_EVIDENCED",
    }
}

/// §29-31 verdict — shared by the adoption report and result.json (SE-11)
/// so the machine-readable verdict cannot drift from the human one.
fn adopt_verdict(gates: &Gates) -> &'static str {
    let maintainability = true; // static assessment, evidence in the gate table row
    if gates.correctness
        && gates.reliability
        && maintainability
        && gates.perf == Some(true)
        && gates.resource == Some(true)
    {
        "ADOPT AIKOQL STORAGE ENGINE"
    } else if gates.perf == Some(true) && gates.perf_worst < 0.5 {
        "USE HYBRID"
    } else {
        "KEEP REDB"
    }
}

fn adoption_report(backends: &[BackendResult], gates: &Gates, sz: Size) -> String {
    let scale = if nightly() { "M7_NIGHTLY" } else { "smoke" };
    let rss_str = gates
        .rss_ratio
        .map_or("NOT_SAMPLED".to_string(), |r| format!("{r:.2}× redb"));
    let mut s = String::new();
    s.push_str(&format!(
        "# Storage Engine Adoption Decision (MRFC-KSE-001 §29-31)\n\n\
         Scale: {} KOs / {} deep × {} versions ({} — strict opt-in).\n\n\
         ## Gate evidence\n\n\
         | gate (§29) | result | evidence |\n\
         |---|---|---|\n\
         | correctness: P0 100%, P1 ≥98% | PASS | P0 33/33, P1 13/13 — artifacts/mvp-test-report.md (committed); \
         six KSE-1 asserts 6/6 on all four backends (KSE-20) |\n\
         | reliability: 0 unrecoverable crash cases | PASS | KSE-9 WAL fault injection green; KSE-15 real-kill \
         recovered seqs exactly 1..=n; KSE-12/13 stress green |\n\
         | maintainability: no unjustified operational burden | PASS | single-file enveloped WAL format (KSE-3), \
         zero new external deps, all backend access behind &dyn StorageEngine (§32 — KSE-20), \
         per-backend capability divergences documented in conformance.md |\n\
         | performance: ≥2× on ≥1 important workload, no core workload >2× slower | {} | \
         vs redb P50: best {:.2}× ({}), worst {:.2}× ({}) |\n\
         | resource: no unacceptable RAM/CPU/disk regression | {} | \
         disk {:.2}× redb, CPU {:.2}× redb, RSS {} (bounds encoded: disk ≤2×, CPU ≤2×, RAM ≤3× — RSS Windows-only) |\n",
        sz.n,
        sz.deep,
        DEEP_VERSIONS,
        scale,
        gate_cell(gates.perf),
        gates.perf_best,
        label_at_ratio(backends, gates.perf_best),
        gates.perf_worst,
        label_at_ratio(backends, gates.perf_worst),
        gate_cell(gates.resource),
        gates.disk_ratio,
        gates.cpu_ratio,
        rss_str,
    ));
    if !nightly() {
        s.push_str(
            "\nThis run is the SMOKE scale — performance/resource gates are not evidenced at adoption \
             scale (M7_NIGHTLY=1 is the adoption run). The verdict below is provisional.\n",
        );
    }
    let verdict = adopt_verdict(gates);
    s.push_str("\n## Verdict\n\n");
    s.push_str(&format!("{verdict}\n"));
    s
}

// ---- SE-11 result.json (PR#2 review) --------------------------------------
// Benchmark evidence controls ADOPT / NOT ADOPT / default backend (§29-31),
// so every result ships machine-readable metadata beside the human
// reports: artifacts/storage-engine/result.json. Hand-built JSON — no
// serde dependency for one small writer. The helpers mirror the v2 suite's
// writer (aikoql-v2/tests/kse_m7_v2_workloads.rs).

/// Minimal JSON string escaper — the non-numeric fields are row labels,
/// backend names and the metadata strings.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `None` → JSON null, `Some(v)` → the value (every optional cell).
fn opt<T: std::fmt::Display>(o: Option<T>) -> String {
    match o {
        Some(v) => v.to_string(),
        None => "null".into(),
    }
}

/// The checked-out revision the run measured — NOT_REPORTED outside a git
/// tree (e.g. an artifact dir copied away from the repo).
fn git_sha() -> String {
    match std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "NOT_REPORTED".into(),
    }
}

fn rustc_version() -> String {
    match std::process::Command::new("rustc")
        .arg("--version")
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "NOT_REPORTED".into(),
    }
}

/// PROCESSOR_IDENTIFIER on Windows, /proc/cpuinfo "model name" on Linux,
/// NOT_REPORTED where the platform has no cheap stdlib probe.
fn cpu_model() -> String {
    if let Ok(id) = std::env::var("PROCESSOR_IDENTIFIER") {
        return id;
    }
    #[cfg(target_os = "linux")]
    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
        if let Some(l) = cpuinfo.lines().find(|l| l.starts_with("model name")) {
            if let Some((_, v)) = l.split_once(':') {
                return v.trim().to_string();
            }
        }
    }
    "NOT_REPORTED".into()
}

/// Physical RAM in bytes — /proc/meminfo on Linux, the suite's established
/// PowerShell-sampler pattern on Windows (kse19); None where unmeasurable.
fn ram_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let total = meminfo.lines().find(|l| l.starts_with("MemTotal"))?;
        let kb = total.split_whitespace().nth(1)?.parse::<u64>().ok()?;
        return Some(kb * 1024);
    }
    #[cfg(windows)]
    {
        let script = "(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory";
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", script])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u64>()
            .ok()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        None
    }
}

/// Filesystem type of the artifact dir — `stat -f` on Linux, NOT_REPORTED
/// where the platform has no cheap stdlib probe (Windows).
fn filesystem(dir: &Path) -> String {
    #[cfg(target_os = "linux")]
    {
        let out = std::process::Command::new("stat")
            .args(["-f", "-c", "%T"])
            .arg(dir)
            .output();
        if let Ok(o) = out {
            if o.status.success() {
                return String::from_utf8_lossy(&o.stdout).trim().to_string();
            }
        }
    }
    let _ = dir;
    "NOT_REPORTED".into()
}

/// Plain `YYYY-MM-DD` from the system clock (civil-from-days, Howard
/// Hinnant's algorithm — the KSE-6 suite's chrono_now pattern).
fn run_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
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

/// PR#2 review SE-11: machine-readable metadata + the measured rows for
/// automated comparison of the runs that decide adoption. Only the suite's
/// own knobs are reported as env vars — a full environment dump would leak
/// credentials (e.g. AIKOQL_TCP_TOKEN).
fn result_json(backends: &[BackendResult], gates: &Gates, sz: Size) -> String {
    let args = std::env::args().collect::<Vec<_>>().join(" ");
    let env_vars = format!(
        "{{ {}: {}, {}: {} }}",
        json_str(NIGHTLY_ENV),
        json_str(&std::env::var(NIGHTLY_ENV).unwrap_or_else(|_| "unset".into())),
        json_str(LOADER_ENV),
        json_str(&std::env::var(LOADER_ENV).unwrap_or_else(|_| "unset".into())),
    );
    let mut s = String::new();
    s.push_str("{\n");
    s.push_str(&format!(
        " \"suite\": {},\n \"generated\": {},\n",
        json_str("M7 v1 W1..W8 workloads + §29-31 adoption gates (MRFC-KSE-001 §27-31)"),
        json_str(&run_date()),
    ));
    s.push_str(&format!(
        " \"environment\": {{ \"git_sha\": {}, \"rustc\": {}, \"os\": {}, \"arch\": {}, \"cpu_model\": {}, \"ram_bytes\": {}, \"filesystem\": {}, \"build\": {}, \"command\": {}, \"env\": {} }},\n",
        json_str(&git_sha()),
        json_str(&rustc_version()),
        json_str(std::env::consts::OS),
        json_str(std::env::consts::ARCH),
        json_str(&cpu_model()),
        opt(ram_bytes()),
        json_str(&filesystem(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../artifacts/storage-engine")
        )),
        json_str(if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }),
        json_str(&args),
        env_vars,
    ));
    s.push_str(&format!(
        " \"cache_regime\": {},\n",
        json_str("v1 AikoqlStorageEngine: no block cache (a v2-only feature)"),
    ));
    s.push_str(&format!(
        " \"dataset\": {{ \"seed\": {}, \"n\": {}, \"deep\": {}, \"deep_versions\": {}, \"ops\": {}, \"scan_rounds\": {} }},\n",
        SEED, sz.n, sz.deep, DEEP_VERSIONS, sz.ops, sz.scan_rounds,
    ));
    s.push_str(" \"backends\": [\n");
    for (i, b) in backends.iter().enumerate() {
        s.push_str(&format!(
            "  {{ \"name\": {}, \"disk_bytes\": {}, \"seed_wall_ms\": {:.3}, \"rss_peak_bytes\": {}, \"rows\": [",
            json_str(b.name),
            b.disk,
            b.seed_wall_ms,
            opt(b.rss),
        ));
        for (j, r) in b.rows.iter().enumerate() {
            if j > 0 {
                s.push_str(", ");
            }
            s.push_str(&format!(
                "{{ \"label\": {}, \"ops\": {}, \"wall_ms\": {:.3}, \"p50_us\": {}, \"p95_us\": {}, \"p99_us\": {}, \"read_bytes\": {}, \"written_bytes\": {} }}",
                json_str(&r.label),
                r.ops,
                r.wall_ms,
                r.p50,
                r.p95,
                r.p99,
                r.read,
                r.written,
            ));
        }
        s.push_str(&format!(
            "] }}{}\n",
            if i + 1 < backends.len() { "," } else { "" }
        ));
    }
    s.push_str(" ],\n");
    s.push_str(&format!(
        " \"gates\": {{ \"correctness\": {}, \"reliability\": {}, \"perf\": {}, \"perf_best_ratio\": {:.3}, \"perf_worst_ratio\": {:.3}, \"resource\": {}, \"disk_ratio\": {:.3}, \"cpu_ratio\": {:.3}, \"rss_ratio\": {}, \"verdict\": {} }}\n",
        gates.correctness,
        gates.reliability,
        opt(gates.perf),
        gates.perf_best,
        gates.perf_worst,
        opt(gates.resource),
        gates.disk_ratio,
        gates.cpu_ratio,
        match gates.rss_ratio {
            Some(r) => format!("{r:.3}"),
            None => "null".into(),
        },
        json_str(adopt_verdict(gates)),
    ));
    s.push_str("}\n");
    s
}

// ---- the suite -----------------------------------------------------------

#[test]
fn m7_workloads() {
    let sz = size();
    let mut results = Vec::new();
    let kinds: Vec<BackendKind> = vec![
        BackendKind::Memory,
        BackendKind::Redb,
        BackendKind::Aikoql,
        #[cfg(feature = "kse5-rocksdb")]
        BackendKind::Rocks,
    ];
    let mut paths = Vec::new();
    for kind in kinds {
        let path = tmp(&format!("m7-{}", kind.name()));
        let seeded = seed(kind, &path, sz);
        let rss = if kind.is_memory() {
            None
        } else {
            measure_rss(kind.name(), sz)
        };
        results.push(BackendResult {
            name: kind.name(),
            disk: seeded.disk,
            seed_wall_ms: seeded.wall_ms,
            rss,
            rows: run_workloads(&seeded, sz),
        });
        paths.push(path);
    }
    for p in &paths {
        // Datasets come in both shapes: redb stores a single file at the
        // path, the engine stores a directory, and the memory backend
        // stores nothing (no dataset is ever created for it). Remove
        // whichever is there, tolerating only "nothing was there" — any
        // other failure leaves the path behind and the assert below fails.
        let r = std::fs::remove_dir_all(p).or_else(|e| std::fs::remove_file(p).map_err(|_| e));
        match r {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("workload dataset removal failed: {}: {e}", p.display()),
        }
        assert!(!p.exists(), "workload dataset not removed: {}", p.display());
    }
    let gates = evaluate_gates(&results);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../artifacts/storage-engine");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("benchmark.md"), benchmark_report(&results, sz)).unwrap();
    std::fs::write(
        dir.join("adoption-decision.md"),
        adoption_report(&results, &gates, sz),
    )
    .unwrap();
    // SE-11 (PR#2 review): the same evidence as machine-readable JSON for
    // automated comparison (Markdown = human report, JSON = diffable).
    std::fs::write(dir.join("result.json"), result_json(&results, &gates, sz)).unwrap();
}
