//! V2-Adopt — the W1..W8 workloads re-run on v2 against the same matrix
//! v1's M7 adoption used (MRFC-KSE-001 §27-28 + design §26 gates).
//!
//! Mirrors `crates/storage/aikoql/tests/kse_m7_workloads.rs` workload for
//! workload — same seed, same ops, same metrics (throughput, P50/P95/P99,
//! logical bytes read/written, CPU/RSS/disk) — on four backends: memory
//! (reference), redb, aikoql (the adopted v1 baseline) and aikoql-v2 (the
//! candidate). One seeded dataset per backend, everything through
//! `&dyn StorageEngine` + the Kernel (§32).
//!
//! Workload mapping: W1/W2 KO head lookup (the same storage leg — KSE-18,
//! measured twice on fresh samples), W3 version lookup + history, W4
//! traversal at F=10/100/1000, W5 type scan, W6 ingestion = the seed
//! phase, W7 context compilation, W8 mixed 70/20/10.
//!
//! The §26 adoption gates ride along:
//! - gate 1 (bounded recovery): evidenced by the SE2-M3 suite —
//!   artifacts/storage-engine-v2/recovery-independence.md (replay only the
//!   active WAL; real-kill recovery in M3/M4/M6) — cited, not re-measured.
//! - gates 2+3 (>RAM queryable, configurable memory): pinned HERE by
//!   `v2_gate2_3_dataset_larger_than_ram` — an ~820 KB dataset served from
//!   on-disk segments under a 64 KiB memtable + zero block cache, every
//!   answer byte-exact before and after reopen.
//! - gate 4 (group commit): throughput evidence is the SE2-M6 nightly
//!   matrix (`SE2M6_NIGHTLY=1`, writes group-commit.md) — cited, not
//!   re-measured here.
//! - gate 5 (KO lookup competitive): the W1/W2 rows below vs the v1
//!   baseline — perf verdict at `V2ADOPT_NIGHTLY=1` only (smoke reports
//!   the ratios, never a verdict).
//!
//! Sizing is strict opt-in: `V2ADOPT_NIGHTLY=1` (100K KOs / 10K deep × 10
//! versions / 20K ops per workload) or unset (2K / 2K / 2K smoke). Any
//! other value = FAIL (no silent skips). `V2ADOPT_LOADER=1` gates the RSS
//! loader child.
//!
//! Writes `artifacts/storage-engine-v2/workloads.md` (the §28 matrix +
//! the §26 gate table) and `artifacts/storage-engine-v2/result.json`
//! (PR#2 review SE-11: the same evidence plus run metadata as
//! machine-readable JSON for automated comparison) — both only at
//! `V2ADOPT_NIGHTLY=1`, so a smoke run never clobbers the canonical
//! artifacts (SE2-M19).

mod common;

use aikoql_kernel::storage::store::{MemoryEngine, StorageEngine, WriteBatch};
use aikoql_kernel::storage::store_redb::RedbEngine;
use aikoql_kernel::transaction::kernel::ManualClock;
use aikoql_kernel::{Direction, Kernel, Metadata, RelationshipRef, Subject, Value, KOID};
use aikoql_storage::AikoqlStorageEngine;
use aikoql_storage_v2::db::{Config, Db};
use aikoql_storage_v2::stats::ReadPathStats;
use aikoql_storage_v2::AikoqlStorageEngineV2;
use common::run_date;
use common::{bytes_written, ctx, percentiles, tmp, CountingEngine, LogicalCounts};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[cfg(windows)]
use std::io::{BufRead, BufReader};
#[cfg(windows)]
use std::process::{Command, Stdio};

const NIGHTLY_ENV: &str = "V2ADOPT_NIGHTLY";
const LOADER_ENV: &str = "V2ADOPT_LOADER";
const LOADER_BACKEND_ENV: &str = "V2ADOPT_LOADER_BACKEND";
const SEED: u64 = 0x27_0000;
const N_TYPES: usize = 100;
const DEEP_VERSIONS: usize = 10; // "10+ versions each" (§27 W3)
/// Gate 5 bound: the KO-lookup rows may be at most this much slower than
/// the adopted v1 baseline to count as competitive (v1's own gate vs redb
/// used the same 2× envelope, mirrored here against v1).
const GATE5_SLOWDOWN_BOUND: f64 = 2.0;

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
        other => panic!("{NIGHTLY_ENV} strict opt-in: unset or 1, got {other:?}"),
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
    AikoqlV2,
}

impl BackendKind {
    fn name(self) -> &'static str {
        match self {
            BackendKind::Memory => "memory",
            BackendKind::Redb => "redb",
            BackendKind::Aikoql => "aikoql",
            BackendKind::AikoqlV2 => "aikoql-v2",
        }
    }
    fn from_name(s: &str) -> BackendKind {
        match s {
            "redb" => BackendKind::Redb,
            "aikoql" => BackendKind::Aikoql,
            "aikoql-v2" => BackendKind::AikoqlV2,
            _ => panic!("unknown loader backend {s}"),
        }
    }
    fn open(self, path: &Path) -> Arc<dyn StorageEngine> {
        match self {
            BackendKind::Memory => Arc::new(MemoryEngine::new()),
            BackendKind::Redb => Arc::new(RedbEngine::open(path).unwrap()),
            BackendKind::Aikoql => Arc::new(AikoqlStorageEngine::open(path).unwrap()),
            BackendKind::AikoqlV2 => Arc::new(AikoqlStorageEngineV2::open(path).unwrap()),
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

/// The seeding phases, engine-agnostic (SE2-M21: the attribution probe
/// seeds one v2 dataset through the Kernel over the adapter, while the
/// matrix's `seed()` counts bytes through a CountingEngine — same phases,
/// same order, so the datasets are the same by construction).
/// Returns (ids, deep, hubs, commits).
fn seed_phases(
    k: &Kernel,
    clock: &ManualClock,
    sz: Size,
) -> (Vec<KOID>, Vec<KOID>, [KOID; 3], u64) {
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
        let mut req = rmv(k, &ids[i], "m7_0");
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
        let mut req = rmv(k, &ids[idx], "m7_0");
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
            let mut req = rmv(k, &id, "m7_0");
            req.properties.insert("v".into(), Value::Int(v as i64));
            let _ = k.remember(req).unwrap();
        }
    }

    let commits = (sz.n * 2 + 3 + sz.deep * (DEEP_VERSIONS - 2)) as u64;
    (ids, deep, hubs, commits)
}

fn seed(kind: BackendKind, path: &Path, sz: Size) -> Seeded {
    let engine = kind.open(path);
    let counting = CountingEngine::new(engine);
    let clock = Arc::new(ManualClock::new(10_000));
    let k = Arc::new(Kernel::open(counting.clone(), clock.clone(), SEED).unwrap());
    let before = LogicalCounts::snapshot(&counting);
    let t0 = Instant::now();
    let (ids, deep, hubs, commits) = seed_phases(&k, &clock, sz);
    let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let seed_read = LogicalCounts::snapshot(&counting).delta(before).bytes;
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
fn v2_m7_loader() {
    if std::env::var(LOADER_ENV).is_err() {
        return; // parent run: nothing to do
    }
    let backend = std::env::var(LOADER_BACKEND_ENV).unwrap();
    let sz = size();
    let path = tmp(&format!("v2-m7-loader-{backend}"));
    let kind = BackendKind::from_name(&backend);
    let _ = seed(kind, &path, sz);
    cleanup_dataset(&path);
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
            .arg("v2_m7_loader")
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
        assert!(status.success(), "v2: {backend} loader child failed");
        assert!(
            samples > 0,
            "v2: {backend} RSS sampler collected no samples"
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

/// P50 ratio of the v2 row vs the same row on `base` (None when either
/// side has no samples) — the gate-5 lens against the v1 baseline.
fn p50_ratio(v2: &BackendResult, base: &BackendResult, label: &str) -> Option<f64> {
    let a = v2.rows.iter().find(|r| r.label == label)?;
    let b = base.rows.iter().find(|r| r.label == label)?;
    if a.p50 > 0 && b.p50 > 0 {
        Some(a.p50 as f64 / b.p50 as f64)
    } else {
        None
    }
}

fn gate_cell(g: Option<bool>) -> &'static str {
    match g {
        Some(true) => "PASS",
        Some(false) => "FAIL",
        None => "NOT_EVIDENCED",
    }
}

/// Gate-5 evidence: the v2 KO-lookup P50 slowdown vs the v1 baseline.
/// The verdict is nightly-gated; the ratios are reported on every run.
/// Shared by the report and result.json (SE-11) so they cannot drift.
fn gate5_evidence(backends: &[BackendResult]) -> (Option<bool>, Option<f64>, Option<f64>) {
    let v2 = backends.iter().find(|b| b.name == "aikoql-v2").unwrap();
    let aik = backends.iter().find(|b| b.name == "aikoql").unwrap();
    let (r1, r2) = (
        p50_ratio(v2, aik, "KO get (W1)"),
        p50_ratio(v2, aik, "head get (W2)"),
    );
    let verdict = nightly().then(|| {
        r1.is_some_and(|r| r <= GATE5_SLOWDOWN_BOUND)
            && r2.is_some_and(|r| r <= GATE5_SLOWDOWN_BOUND)
    });
    (verdict, r1, r2)
}

fn benchmark_report(backends: &[BackendResult], sz: Size) -> String {
    let profile = if cfg!(debug_assertions) {
        "debug (CPU inflated; RSS comparable — kse19)"
    } else {
        "release"
    };
    let scale = if nightly() {
        "V2ADOPT_NIGHTLY"
    } else {
        "smoke"
    };
    let (gate5, r1, r2) = gate5_evidence(backends);

    let mut s = String::new();
    let date = run_date();
    s.push_str(&format!(
        "# W1..W8 Workloads — v2 vs redb vs v1 (MRFC-KSE-001 §27-28 + design §26)\n\n\
         Date: {date} · profile: {profile} · seed {SEED:#x} · scale: {} KOs / {} deep × {} versions / {} ops ({scale} — strict opt-in)\n\n\
         The same workload shapes v1's M7 adoption ran, on the same seed. All workloads through the Kernel on `&dyn StorageEngine` (§32). One seeded dataset per backend.\n\n",
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
    s.push_str("\n## §26 adoption gates\n\n");
    s.push_str("| gate (§26) | result | evidence |\n|---|---|---|\n");
    s.push_str(&format!(
        "| 1. recovery bounded by the active WAL | PASS | SE2-M3 suite — artifacts/storage-engine-v2/recovery-independence.md: replay only the active WAL, orphan/missing-segment policies; real-kill recovery suites in M3/M4/M6 |\n\
         | 2. dataset larger than RAM remains queryable | PASS | `v2_gate2_3_dataset_larger_than_ram` (this suite): ~820 KB dataset under a 64 KiB memtable + zero cache → served from on-disk segments, full scan byte-exact, survives reopen |\n\
         | 3. memory limits configurable | PASS | the same probe pins both knobs: `memtable_bytes=64 KiB` forced flushes (≥2 SEGMENT files); `cache_bytes=0` detaches the cache (silent stats), a 4 KiB cap is consulted (misses) yet holds nothing (oversize block never retained) |\n\
         | 4. group commit improves concurrent throughput without weakening Sync | — | SE2-M6 suite green (Sync baseline reproduced exactly); throughput evidence = the `SE2M6_NIGHTLY=1` matrix → artifacts/storage-engine-v2/group-commit.md |\n\
         | 5. KO lookup competitive with the MVP baseline (v1) | {} | W1 {:.2}× v1, W2 {:.2}× v1 (P50; bound ≤ {GATE5_SLOWDOWN_BOUND}× — perf verdict only at V2ADOPT_NIGHTLY=1, this run is {scale}) |\n",
        gate_cell(gate5),
        r1.unwrap_or(0.0),
        r2.unwrap_or(0.0),
    ));
    s.push_str("\n## Reference rows (not re-measured here)\n\n");
    s.push_str(
        "- snapshot: v2 rides the trait defaults (redb snapshot — REC-002); v1 byte-exact restore pinned (KSE-14); redb single-file opens as redb.\n\
         - recovery: v2 real-kill recovery pinned by the SE2-M3/M4/M6 suites (recovery-independence.md); v1 by KSE-15.\n\
         - concurrent mixed load: v2 pinned behaviorally by the SE2-M6 group-commit suite (KSE-13 order); v1 by KSE-13. W8 above is the single-threaded mixed row.\n\
         - 1M/10M ingestion scale: v1 1M creates = 1242 s / 645 B per KO heap (KSE-19, measured). v2 at 1M NOT_MEASURED.\n",
    );
    s.push_str("\n## Honest metric mapping\n\n");
    s.push_str(
        "- throughput/latency: per-op wall on one thread; percentiles over the instrumented pass (P50/P95/P99 in µs)\n\
         - bytes read: CountingEngine bytes returned over the workload (get + scan Σ k+v)\n\
         - bytes written: CountingEngine batch Σ put k+v (logical, pre-codec)\n\
         - W6 ingestion P50/P95/P99 = mean commit cost (the seed loop isn't per-op instrumented)\n\
         - CPU: seed wall, single-threaded (wall ≈ CPU); disk: file (redb/aikoql) or dir (aikoql-v2) at seed end; memory = none\n\
         - RSS: Windows-only WorkingSet64 poll on a loader child (peak is a lower bound — kse19); CI/ubuntu rows NOT_SAMPLED\n\
         - memory backend: RAM-only reference, not an adoption candidate\n\
         - W2 = the same storage leg as W1 (k.get is the kernel's only public head read — KSE-18 pins head+version rows); \
         measured twice on fresh samples, not a faked second API\n\
         - v2 RSS on aikoql-v2 includes the 64 MiB memtable + 8 MiB block-cache defaults; gates 2+3 show the knobs bound them\n",
    );
    s
}

// ---- SE-11 result.json (PR#2 review) --------------------------------------
// Benchmark evidence controls ADOPT / NOT ADOPT / default backend, so every
// result ships machine-readable metadata beside the human report:
// artifacts/storage-engine-v2/result.json. Hand-built JSON — no serde
// dependency for one small writer. The helpers mirror the v1 suite's
// writer (aikoql/tests/kse_m7_workloads.rs).

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

/// PR#2 review SE-11: machine-readable metadata + the measured rows for
/// automated comparison of the runs that decide V2 adoption. Only the
/// suite's own knobs are reported as env vars — a full environment dump
/// would leak credentials (e.g. AIKOQL_TCP_TOKEN).
fn result_json(backends: &[BackendResult], sz: Size) -> String {
    let args = std::env::args().collect::<Vec<_>>().join(" ");
    let (gate5, r1, r2) = gate5_evidence(backends);
    // The suite opens the engine at the v2 defaults (engine.rs:
    // AikoqlStorageEngineV2::open → Config::new) — report the live
    // default values, not hardcoded ones.
    let cfg = Config::new(PathBuf::new());
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
        json_str("M7 W1..W8 workloads on v2 (MRFC-KSE-001 §27-28 + design §26)"),
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
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../artifacts/storage-engine-v2")
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
        json_str(&format!(
            "v2 engine defaults: memtable_bytes={}, cache_bytes={}",
            cfg.memtable_bytes, cfg.cache_bytes,
        )),
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
        " \"gates\": {{ \"gate5_ko_lookup_competitive\": {{ \"verdict\": {}, \"w1_p50_ratio_vs_v1\": {}, \"w2_p50_ratio_vs_v1\": {}, \"bound\": {} }} }}\n",
        opt(gate5),
        match r1 {
            Some(v) => format!("{v:.3}"),
            None => "null".into(),
        },
        match r2 {
            Some(v) => format!("{v:.3}"),
            None => "null".into(),
        },
        GATE5_SLOWDOWN_BOUND,
    ));
    s.push_str("}\n");
    s
}

// ---- the suite -----------------------------------------------------------

#[test]
fn v2_m7_workloads() {
    let sz = size();
    let mut results = Vec::new();
    let kinds: Vec<BackendKind> = vec![
        BackendKind::Memory,
        BackendKind::Redb,
        BackendKind::Aikoql,
        BackendKind::AikoqlV2,
    ];
    let mut paths = Vec::new();
    for kind in kinds {
        let path = tmp(&format!("v2-m7-{}", kind.name()));
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
        cleanup_dataset(p);
    }
    // The artifact is canonical at adoption scale only — a smoke run (the
    // plain suite) must not clobber it (SE2-M19: it used to).
    if std::env::var_os(NIGHTLY_ENV).is_some() {
        let dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../artifacts/storage-engine-v2");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("workloads.md"), benchmark_report(&results, sz)).unwrap();
        // SE-11 (PR#2 review): machine-readable twin of workloads.md for
        // automated comparison (Markdown = human report, JSON = diffable).
        std::fs::write(dir.join("result.json"), result_json(&results, sz)).unwrap();
    }
}

// ---- §26 gates 2 + 3 probe ------------------------------------------------
// A dataset many times the memory budget, served byte-exact from on-disk
// segments — the >RAM queryability gate pinned at correctness scale (the
// bound is the mechanism, not a perf number: a 64 KiB memtable forces the
// flushes, a zero cache forces the disk reads).
#[test]
fn v2_gate2_3_dataset_larger_than_ram() {
    const N: usize = 400;
    const VALUE_LEN: usize = 2048;
    let path = tmp("v2-gate2");
    let mut cfg = Config::new(path.clone());
    cfg.memtable_bytes = 64 * 1024;
    cfg.cache_bytes = 0;
    // SE2-M10: the gate pins the ≥2 SEGMENT files the flushes produce — an
    // auto-triggered compaction would merge them away.
    cfg.l0_compact_trigger = 0;
    let engine = AikoqlStorageEngineV2::open_with_config(cfg).unwrap();

    // ~820 KB logical over a 64 KiB memtable: every batch after the first
    // few forces a flush, so the dataset lives in immutable segments.
    let mut reference: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for chunk in 0..(N / 20) {
        let mut b = WriteBatch::new();
        for i in 0..20 {
            let key = format!("g2/{:06}", chunk * 20 + i).into_bytes();
            let value: Vec<u8> = (0..VALUE_LEN)
                .map(|j| ((chunk * 20 + i + j) & 0xFF) as u8)
                .collect();
            reference.insert(key.clone(), value.clone());
            b.put(key, value);
        }
        engine.write_batch(&b).unwrap();
    }
    let segs = std::fs::read_dir(&path)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("SEGMENT-")
        })
        .count();
    assert!(segs >= 2, "gate 2: expected flushed segments, found {segs}");

    // every answer byte-exact from the bounded-memory engine
    let expect: Vec<(Vec<u8>, Vec<u8>)> = reference
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    assert_eq!(engine.scan(b"g2/").unwrap(), expect);
    for (k, v) in reference.iter().step_by(37) {
        assert_eq!(engine.get(k).unwrap(), Some(v.clone()));
    }
    drop(engine);

    // reopen with the same knobs: the memtable is gone — the answers now
    // come from the segments. cache_bytes=0 detaches the cache (db.rs:
    // `cache: None`), so reads hit the file directly and the stats stay
    // silent — the knob's documented semantics, pinned here.
    let mut cfg = Config::new(path.clone());
    cfg.cache_bytes = 0;
    let db = Db::open(cfg).unwrap();
    assert_eq!(db.scan(b"g2/").unwrap(), expect);
    for (k, v) in reference.iter().step_by(13) {
        assert_eq!(db.get(k).unwrap(), Some(v.clone()));
    }
    let stats = db.cache_stats();
    assert_eq!(
        (stats.hits, stats.misses, stats.bytes),
        (0, 0, 0),
        "gate 3: cache_bytes=0 must detach the cache entirely"
    );
    drop(db);

    // reopen with a 4 KiB cache — smaller than one data block (~40 KiB):
    // the cache is consulted (misses count) yet holds nothing (an oversize
    // block is never retained) — the cap bounds memory at both ends.
    let mut cfg = Config::new(path.clone());
    cfg.cache_bytes = 4096;
    let db = Db::open(cfg).unwrap();
    for (k, v) in reference.iter().step_by(13) {
        assert_eq!(db.get(k).unwrap(), Some(v.clone()));
    }
    let stats = db.cache_stats();
    assert!(
        stats.misses > 0,
        "gate 3: segment reads must consult the attached cache"
    );
    assert_eq!(
        stats.bytes, 0,
        "gate 3: a block larger than the cap is never retained"
    );
    drop(db);
    cleanup_dataset(&path);
}

/// Remove a seeded dataset once its run is complete — these directories used
/// to accumulate in the OS temp dir (hundreds of MB per nightly).
fn cleanup_dataset(path: &Path) {
    // A backend leaves a file (redb, aikoql v1), a directory (aikoql-v2), or
    // nothing (Memory — idempotent by contract). remove_dir_all is dir-only:
    // on a file it fails with 267 on Windows, so dispatch on the path type.
    let res = std::fs::symlink_metadata(path).and_then(|m| {
        if m.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        }
    });
    match res {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("test dataset not removed: {}: {e}", path.display()),
    }
    assert!(
        !path.exists(),
        "test dataset not removed: {}",
        path.display()
    );
}

// ---- SE2-M21 attribution probe ----------------------------------------------
// The adoption-scale legs of the point-read attribution: M21-01..04 pin the
// ACCOUNTING on unit-scale legs; this pins WHERE a warm adoption-scale
// k.get spends its time. One v2 dataset seeded through the Kernel over the
// adapter (the same seeding phases as the matrix — byte-identical datasets),
// then per-leg per-op ReadPathStats deltas: W1/W2 kernel k.get (2 engine
// gets per op), the exact engine reads the kernel makes (head/<koid>,
// ko/<koid><ts>), and the pure mechanism legs on a small Db (memtable hit,
// cached-block hit). Writes artifacts/storage-engine-v2/attribution.md —
// the M22 input. Strict opt-in: `SE2M21_ATTRIB=1`.

const ATTRIB_ENV: &str = "SE2M21_ATTRIB";
const ATTRIB_ROW_BYTES: usize = 1400; // the M11 adoption row shape

/// One op's attribution: external wall, the engine's whole-get timer, each
/// timed phase (counter deltas), the untimed engine residual, and the
/// kernel-side overhead (wall − get_wall). Everything in ns.
#[derive(Default, Clone, Copy)]
struct AttribOp {
    wall: u64,
    get_wall: u64,
    lock: u64,
    memtable: u64,
    bloom: u64,
    index: u64,
    cache: u64,
    io: u64,
    decode: u64,
    residual: u64,
    overhead: u64,
}

impl AttribOp {
    fn at(self, i: usize) -> u64 {
        [
            self.wall,
            self.get_wall,
            self.lock,
            self.memtable,
            self.bloom,
            self.index,
            self.cache,
            self.io,
            self.decode,
            self.residual,
            self.overhead,
        ][i]
    }
}

const PHASE_NAMES: [&str; 11] = [
    "wall (external)",
    "get_wall (engine gets)",
    "lock_wait",
    "memtable lookup",
    "bloom probe",
    "index lookup",
    "block cache lookup",
    "block io",
    "block decode",
    "residual (engine untimed)",
    "overhead (kernel + adapter)",
];

/// One instrumented leg: `ops` executions of `run`, each wrapped in a
/// per-op ReadPathStats delta (the snapshots straddle the op and nothing
/// else writes during a leg, so a delta is exactly that op's counters).
/// The stats closure reads the same engine the ops hit. Returns the
/// per-op records plus the leg's counter delta — the mechanism pins.
fn attrib_leg(
    stats: impl Fn() -> ReadPathStats,
    seed: u64,
    ops: usize,
    mut run: impl FnMut(&mut Xs),
) -> (Vec<AttribOp>, ReadPathStats) {
    let before = stats();
    let mut rng = Xs(seed);
    let mut recs = Vec::with_capacity(ops);
    for _ in 0..ops {
        let s = stats();
        let t0 = Instant::now();
        run(&mut rng);
        let wall = t0.elapsed().as_nanos() as u64;
        let d = common::stats_delta(stats(), s);
        let parts = d.lock_wait_ns
            + d.memtable_lookup_ns
            + d.bloom_probe_ns
            + d.index_lookup_ns
            + d.block_cache_lookup_ns
            + d.block_io_ns
            + d.block_decode_ns;
        recs.push(AttribOp {
            wall,
            get_wall: d.get_wall_ns,
            lock: d.lock_wait_ns,
            memtable: d.memtable_lookup_ns,
            bloom: d.bloom_probe_ns,
            index: d.index_lookup_ns,
            cache: d.block_cache_lookup_ns,
            io: d.block_io_ns,
            decode: d.block_decode_ns,
            residual: d.get_wall_ns.saturating_sub(parts),
            overhead: wall.saturating_sub(d.get_wall_ns),
        });
    }
    (recs, common::stats_delta(stats(), before))
}

fn col(recs: &[AttribOp], i: usize) -> Vec<u128> {
    recs.iter().map(|r| r.at(i) as u128).collect()
}

fn attrib_leg_report(label: &str, kind: &str, d: ReadPathStats, recs: &[AttribOp]) -> String {
    let mut s = String::new();
    s.push_str(&format!("## {label}\n\n{kind}\n\n"));
    s.push_str(&format!(
        "counters: lookups {} · memtable hits {} · segments {} · cache hits {} misses {} · blocks read {} · bytes read {} · entries decoded {}\n\n",
        d.lookups,
        d.memtable_hits,
        d.segments_considered,
        d.block_cache_hits,
        d.block_cache_misses,
        d.blocks_read,
        d.bytes_read,
        d.entries_decoded,
    ));
    s.push_str("| phase | p50 | p95 | p99 |\n|---|---|---|---|\n");
    for (i, name) in PHASE_NAMES.iter().enumerate() {
        let (a, b, c) = percentiles(col(recs, i));
        s.push_str(&format!(
            "| {name} | {:.2} µs | {:.2} | {:.2} |\n",
            a as f64 / 1000.0,
            b as f64 / 1000.0,
            c as f64 / 1000.0
        ));
    }
    s.push('\n');
    s
}

#[test]
fn v2_attribution_probe() {
    match std::env::var(ATTRIB_ENV) {
        Err(std::env::VarError::NotPresent) => return,
        Ok(v) if v == "1" => {}
        other => panic!("{ATTRIB_ENV} strict opt-in: unset or 1, got {other:?}"),
    }
    // adoption scale — the same dataset shape the M19 matrix ran (100K KOs,
    // 10K deep × 10 versions, 20K ops). Attribution only means something
    // where the working set dwarfs the 8 MiB block cache.
    let sz = Size {
        n: 100_000,
        deep: 10_000,
        ops: 20_000,
        scan_rounds: 10,
    };

    // one v2 dataset, seeded through the Kernel over the adapter — the
    // probe holds the adapter, so the kernel leg and the engine legs
    // measure the same database (no CountingEngine: the counters are the
    // v2 ReadPathStats themselves).
    let path = tmp("v2-attrib");
    let engine = Arc::new(AikoqlStorageEngineV2::open(&path).unwrap());
    let clock = Arc::new(ManualClock::new(10_000));
    let k = Arc::new(Kernel::open(engine.clone(), clock.clone(), SEED).unwrap());
    let (ids, _deep, _hubs, _commits) = seed_phases(&k, &clock, sz);
    let stats = || engine.read_path_stats().unwrap();

    // W1 + W2: the kernel leg — k.get on a uniform id sample (W2 a fresh
    // sample, the harness convention). Pin: exactly 2 engine gets per op
    // (head/<koid> + ko/<koid><ts>, kernel cache off by default).
    let (w1, d1) = attrib_leg(stats, SEED ^ 0x21, sz.ops, |rng| {
        let id = ids[rng.below(ids.len())];
        let _ = k.get(ctx(), &id).unwrap();
    });
    let (w2, d2) = attrib_leg(stats, SEED ^ 0x22, sz.ops, |rng| {
        let id = ids[rng.below(ids.len())];
        let _ = k.get(ctx(), &id).unwrap();
    });
    assert_eq!(d1.lookups, (sz.ops * 2) as u64, "k.get = 2 engine gets");
    assert_eq!(d2.lookups, (sz.ops * 2) as u64, "k.get = 2 engine gets");

    // the engine legs read the exact keys the kernel leg reads: head/<koid>
    // and ko/<koid><ts>, ts captured in an uncounted warm-up pass.
    let mut rng = Xs(SEED ^ 0x21);
    let mut targets = Vec::with_capacity(sz.ops);
    for _ in 0..sz.ops {
        let id = ids[rng.below(ids.len())];
        targets.push((id, k.get(ctx(), &id).unwrap().commit_ts));
    }
    let (head_leg, dh) = attrib_leg(stats, SEED ^ 0x23, sz.ops, |rng| {
        let (id, _) = targets[rng.below(targets.len())];
        let mut key = Vec::with_capacity(5 + 16);
        key.extend_from_slice(b"head/");
        key.extend_from_slice(id.as_bytes());
        assert!(engine.get(&key).unwrap().is_some());
    });
    let (obj_leg, dk) = attrib_leg(stats, SEED ^ 0x24, sz.ops, |rng| {
        let (id, ts) = targets[rng.below(targets.len())];
        let mut key = Vec::with_capacity(3 + 16 + 8);
        key.extend_from_slice(b"ko/");
        key.extend_from_slice(id.as_bytes());
        key.extend_from_slice(&ts.to_be_bytes());
        assert!(engine.get(&key).unwrap().is_some());
    });
    assert_eq!(dh.lookups, sz.ops as u64);
    assert_eq!(dk.lookups, sz.ops as u64);

    // the pure mechanism legs: a small Db with the adoption row shape —
    // the memtable path and the cached-block path do not depend on dataset
    // scale, and their counters prove each leg IS its mechanism.
    let small = tmp("v2-attrib-small");
    let mut cfg = Config::new(small.clone());
    cfg.memtable_bytes = usize::MAX; // nothing flushes — every get a hit
    let db = Db::open(cfg).unwrap();
    for i in 0..1000 {
        db.put(
            &format!("a/{i:04}").into_bytes(),
            &vec![b'f'; ATTRIB_ROW_BYTES],
        )
        .unwrap();
    }
    let (mem_leg, dm) = attrib_leg(
        || db.read_path_stats(),
        SEED ^ 0x25,
        20_000,
        |rng| {
            let i = rng.below(1000);
            assert!(db.get(&format!("a/{i:04}").into_bytes()).unwrap().is_some());
        },
    );
    assert_eq!(
        dm.memtable_hits, 20_000,
        "the memtable leg is memtable hits"
    );
    assert_eq!(dm.segments_considered, 0);

    db.flush().unwrap();
    for i in 0..1000 {
        assert!(db.get(&format!("a/{i:04}").into_bytes()).unwrap().is_some()); // warm pass
    }
    let (hit_leg, dc) = attrib_leg(
        || db.read_path_stats(),
        SEED ^ 0x26,
        20_000,
        |rng| {
            let i = rng.below(1000);
            assert!(db.get(&format!("a/{i:04}").into_bytes()).unwrap().is_some());
        },
    );
    assert!(dc.block_cache_hits >= 20_000, "the hit leg is cache hits");
    assert_eq!(dc.block_cache_misses, 0);
    assert_eq!(dc.blocks_read, 0, "a cached get performs no physical read");
    drop(db);
    cleanup_dataset(&small);

    // the verdict: where a warm W1 k.get goes, naming the dominant engine
    // phase for M22.
    let p50 = |recs: &[AttribOp], i: usize| {
        let (a, _, _) = percentiles(col(recs, i));
        a
    };
    let dom = (2..9)
        .max_by_key(|&i| p50(&w1, i))
        .expect("phase columns exist");
    let verdict = format!(
        "## Where a warm W1 `k.get` goes (M22 input)\n\n\
         - external wall P50 {:.2} µs = engine get_wall {:.2} µs + kernel/adapter overhead {:.2} µs\n\
         - the kernel leg runs 2 engine gets per op; engine-leg P50s: head row {:.2} µs, version row {:.2} µs\n\
         - dominant engine phase at adoption scale: {} ({:.2} µs of {:.2} µs get_wall); engine residual {:.2} µs\n",
        p50(&w1, 0) as f64 / 1000.0,
        p50(&w1, 1) as f64 / 1000.0,
        p50(&w1, 10) as f64 / 1000.0,
        p50(&head_leg, 1) as f64 / 1000.0,
        p50(&obj_leg, 1) as f64 / 1000.0,
        PHASE_NAMES[dom],
        p50(&w1, dom) as f64 / 1000.0,
        p50(&w1, 1) as f64 / 1000.0,
        p50(&w1, 9) as f64 / 1000.0,
    );

    let dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../artifacts/storage-engine-v2");
    std::fs::create_dir_all(&dir).unwrap();
    let machine = format!(
        "{}/{}; {} logical cores; {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "processor NOT_REPORTED".into()),
    );
    let mut report = format!(
        "# Point-Read Cost Attribution — SE2-M21\n\n\
         Generated only when `{ATTRIB_ENV}=1` (strict opt-in). Perf numbers are report cells, never asserts.\n\n\
         - Test: `v2_attribution_probe`\n\
         - Build mode: {}\n\
         - Machine: {machine}\n\
         - Date: {}\n\
         - Dataset: one v2 database, {} KOs / {} deep × {DEEP_VERSIONS} versions / {} ops per leg, seeded through the Kernel over the adapter (SEED {SEED:#x}); the mechanism legs run on a second small Db with the same row shape\n\
         - Reference: M19 warm W1/W2 P50 ≈ 37 µs ≈ 27 µs engine + 10 µs kernel; M17 hot-path 3.5 µs; M18 hot context 92.8 µs\n\n\
         Each row = P50/P95/P99 over {} ops; engine phases are per-op ReadPathStats deltas (SE2-M8 counters + the M21 lock_wait/bloom/get_wall closure), kernel overhead = external wall − engine get_wall.\n\n",
        if cfg!(debug_assertions) { "debug" } else { "release" },
        run_date(),
        sz.n,
        sz.deep,
        sz.ops,
        sz.ops,
    );
    report.push_str(&attrib_leg_report(
        "W1 kernel get (k.get)",
        "the gate-5 leg: 2 engine gets per op",
        d1,
        &w1,
    ));
    report.push_str(&attrib_leg_report(
        "W2 kernel get (fresh sample)",
        "same storage leg, independent sample",
        d2,
        &w2,
    ));
    report.push_str(&attrib_leg_report(
        "Engine leg — head/<koid>",
        "the small row (KSE-18)",
        dh,
        &head_leg,
    ));
    report.push_str(&attrib_leg_report(
        "Engine leg — ko/<koid><ts>",
        "the ~1.4 KB version row",
        dk,
        &obj_leg,
    ));
    report.push_str(&attrib_leg_report(
        "Memtable hit (active memtable)",
        "the M17 hot-path mechanism, same row shape",
        dm,
        &mem_leg,
    ));
    report.push_str(&attrib_leg_report(
        "Cache hit (flushed + warmed block)",
        "cached-block mechanism",
        dc,
        &hit_leg,
    ));
    report.push_str(&verdict);
    std::fs::write(dir.join("attribution.md"), report).unwrap();

    // the accounting closure holds at adoption scale too (M21-01 unit pins
    // the same bound on mixed small legs; this is the gate-5 leg) — after
    // the report write, so a failing probe still lands the artifact
    let sum = |recs: &[AttribOp], i: usize| recs.iter().map(|r| r.at(i) as u128).sum::<u128>();
    let phase_sums = (2..9)
        .map(|i| (PHASE_NAMES[i], sum(&w1, i)))
        .collect::<Vec<_>>();
    assert!(
        sum(&w1, 9) * 10 <= sum(&w1, 1),
        "W1 accounting does not close at adoption scale: residual {} ns of get_wall {} ns ({:.1}%) — phase sums: {} · memtable hits {:.1}/op · segments {:.1}/get · blocks {:.1}/get · cache hits {:.1} misses {:.1}/get · entries decoded {:.1}/get",
        sum(&w1, 9),
        sum(&w1, 1),
        sum(&w1, 9) as f64 / sum(&w1, 1) as f64 * 100.0,
        phase_sums
            .iter()
            .map(|(n, s)| format!("{n} {} ns", s))
            .collect::<Vec<_>>()
            .join(" · "),
        d1.memtable_hits as f64 / sz.ops as f64,
        d1.segments_considered as f64 / (sz.ops * 2) as f64,
        d1.blocks_read as f64 / (sz.ops * 2) as f64,
        d1.block_cache_hits as f64 / (sz.ops * 2) as f64,
        d1.block_cache_misses as f64 / (sz.ops * 2) as f64,
        d1.entries_decoded as f64 / (sz.ops * 2) as f64,
    );
    cleanup_dataset(&path);
}
