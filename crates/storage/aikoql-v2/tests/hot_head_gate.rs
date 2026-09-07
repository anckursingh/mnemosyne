//! SE2-M11 — the QA M3 head gate (TC-PERF-0303 shape): 100 000 repeated
//! head lookups on a cached head, `SE2M11_NIGHTLY=1` strict opt-in,
//! report-only P50 vs the 20 µs gate (a perf number never gates a test —
//! the report cell is the evidence). The QA spec's HeadIndex is NOT
//! pre-built (plan §"where the QA spec is wrong" #3): after restart points +
//! small blocks + the decode-winner-only cache-hit path, a head get is a
//! memtable hit or one small block read; the index is added only if this
//! measurement shows the delta exists.

mod common;

use aikoql_storage_v2::db::{Config, Db};
use common::{dir, percentiles};
use std::path::Path;

const GATE: &str = "SE2M11_NIGHTLY";

/// The QA M3 gate: a hot head get ≤ 20 µs.
const HEAD_GATE_NS: u128 = 20_000;

/// Number of timed lookups.
const LOOKUPS: usize = 100_000;

fn nightly_on() -> bool {
    match std::env::var(GATE) {
        Err(_) => false,
        Ok(v) if v == "1" => true,
        Ok(v) => panic!("{GATE} must be unset or \"1\", got {v:?} (strict opt-in)"),
    }
}

/// The kernel's head key: `head/` + 16-byte koid (semantic_equivalence shape).
fn head_key(koid: &[u8; 16]) -> Vec<u8> {
    let mut k = Vec::with_capacity(5 + 16);
    k.extend_from_slice(b"head/");
    k.extend_from_slice(koid);
    k
}

#[test]
fn hot_head_gate() {
    if !nightly_on() {
        eprintln!("SKIPPED (set SE2M11_NIGHTLY=1 to run the hot-head gate)");
        return;
    }
    // Kernel-shaped dataset at the adoption row size (~1.4 KB): a 16 KiB
    // block holds ~11 rows — the plan's "~11 rows decode ≈ µs on hit" shape.
    const FILLERS: usize = 256;
    const ROW_BYTES: usize = 1400;
    let target_koid = [0xEEu8; 16];
    let target = head_key(&target_koid);
    let target_value = vec![b'h'; ROW_BYTES];

    let mut cfg = Config::new(dir("hot-head"));
    cfg.memtable_bytes = usize::MAX; // one explicit flush below
    let db = Db::open(cfg).unwrap();
    db.put(&target, &target_value).unwrap();
    for i in 0..FILLERS {
        // Fillers spread over the koid space so the segment is genuinely
        // multi-block and the target's block sits among real neighbors.
        let mut koid = [0u8; 16];
        koid[..8].copy_from_slice(&(i as u64).to_be_bytes());
        db.put(&head_key(&koid), &[b'f'; ROW_BYTES]).unwrap();
    }
    db.flush().unwrap();

    // Warm the target's block, then time the cached path. The answer pin
    // runs per lookup (slice compare — no expected-value clone in the
    // timed region beyond the get's own winner clone).
    assert_eq!(db.get(&target).unwrap(), Some(target_value.clone()));
    let stats_before = db.read_path_stats();
    let mut samples = Vec::with_capacity(LOOKUPS);
    for _ in 0..LOOKUPS {
        let t0 = std::time::Instant::now();
        assert_eq!(
            db.get(&target).unwrap().as_deref(),
            Some(&target_value[..]),
            "the cached path must never change an answer"
        );
        samples.push(t0.elapsed().as_nanos());
    }
    let stats_after = db.read_path_stats();
    let cache = db.cache_stats();

    // The measurement must BE the cached path, or the report lies: every
    // timed lookup a cache hit and zero physical block reads during the run.
    assert!(
        cache.hits >= LOOKUPS as u64,
        "the timed lookups must hit the cache ({} hits)",
        cache.hits
    );
    assert_eq!(
        stats_after.blocks_read, stats_before.blocks_read,
        "a cached head performs no physical block read during the run"
    );

    let (p50, p95, p99) = percentiles(samples);
    let verdict = if p50 <= HEAD_GATE_NS { "PASS" } else { "FAIL" };
    let machine = format!(
        "{}/{}; {} logical cores; {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "processor NOT_REPORTED".into()),
    );

    let report = format!(
        "# Hot Head Gate — SE2-M11\n\n\
         Generated only when `SE2M11_NIGHTLY=1` (strict opt-in). Perf numbers are\n\
         report cells, never asserts — the report regenerates only with the env set.\n\n\
         - Test: `hot_head_gate`\n\
         - Build mode: {}\n\
         - Machine: {machine}\n\
         - Workload: {} kernel-shaped head rows (`head/` + 16-byte koid) × ~1.4 KB\n\
           values, one segment (16 KiB blocks, ~11 rows/block), the target's block\n\
           warmed once, then {LOOKUPS} cached lookups of the same head — answers\n\
           pinned byte-exact per lookup, cache hits {}, physical block reads during\n\
           the run: {}\n\n\
         - P50: {p50} ns ({:.1} µs)\n\
         - P95: {p95} ns ({:.1} µs)\n\
         - P99: {p99} ns ({:.1} µs)\n\
         - QA M3 gate: hot head ≤ 20 µs ({HEAD_GATE_NS} ns) — {verdict}\n",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        FILLERS + 1,
        cache.hits,
        stats_after.blocks_read - stats_before.blocks_read,
        p50 as f64 / 1000.0,
        p95 as f64 / 1000.0,
        p99 as f64 / 1000.0,
    );
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("artifacts")
        .join("storage-engine-v2");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hot-head.md"), report).unwrap();
}
