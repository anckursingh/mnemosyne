//! KSE-19 — resource usage (MRFC-KSE-001 §25).
//!
//! §25 asks for RSS / heap / cache memory / index memory / peak allocation /
//! disk / CPU at 100K / 1M / 10M KOs, and the verdict "must not require the
//! entire knowledge graph in memory".
//!
//! Measured per size: peak RSS (sampled), heap ≈ live store bytes (the
//! BTreeMap mirror IS the heap), index memory = the derived-index row bytes
//! (a subset of the store, not a separate cache), disk = WAL bytes, CPU =
//! build wall time (single-threaded, so wall ≈ CPU). Honest rows: cache
//! memory 0 (there is no cache layer — the store itself is in RAM), peak
//! allocation NOT_MEASURED (no allocator tracing wired). 10M is a
//! PROJECTION (bytes/KO × 10M) — the projection IS the §25 verdict
//! evidence: this engine holds the entire graph in a BTreeMap and replays
//! 100% of the WAL at open, so its memory is linear in dataset size by
//! design.
//!
//! Sizing (strict opt-in, KSE-12/13 convention): the suite runs the 10K
//! smoke; `KSE19_NIGHTLY=1` additionally runs 100K + 1M. Any other value
//! of the env var is a FAILURE (env-set-but-dead must never silently
//! skip). RSS sampling: a loader CHILD process builds the store while a
//! PowerShell sampler polls WorkingSet64 of the child pid — isolation from
//! parallel tests, zero new Rust deps. 500 ms sample granularity ⇒ the
//! peak is a lower bound (spikes between samples are missed). RSS is
//! Windows-only (PowerShell); non-Windows runs carry heap/disk with an
//! honest NOT_SAMPLED RSS row — the nightly CI job (ubuntu) reports
//! everything except RSS.

mod common;

use aikoql_kernel::storage::store::StorageEngine;
use aikoql_kernel::transaction::kernel::ManualClock;
use aikoql_kernel::{Kernel, Metadata, RememberRequest, Subject, Value};
use aikoql_storage::AikoqlStorageEngine;
use common::{ctx, structural_sweep, tmp};
#[cfg(windows)]
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::Command;
#[cfg(windows)]
use std::process::Stdio;
use std::sync::Arc;

const SEED: u64 = 0x19_0000;
const TYPE: &str = "kse19_ko";
const LOADER_ENV: &str = "KSE19_LOADER";
const PATH_ENV: &str = "KSE19_PATH";
const RESULTS_ENV: &str = "KSE19_RESULTS";
const N_ENV: &str = "KSE19_N";
const NIGHTLY_ENV: &str = "KSE19_NIGHTLY";

fn meta() -> Metadata {
    Metadata {
        type_name: TYPE.into(),
        tenant: None,
        schema_version: 1,
        tags: vec![],
    }
}

/// Child process: build a store of N KOs (seq i, subject, ~56 B body) and
/// write the measured results file. The clock ticks 1 ms per commit —
/// realistic wall behavior for a long load.
fn loader_main() {
    let path = PathBuf::from(std::env::var(PATH_ENV).unwrap());
    let results = PathBuf::from(std::env::var(RESULTS_ENV).unwrap());
    let n: u64 = std::env::var(N_ENV).unwrap().parse().unwrap();

    let engine = Arc::new(AikoqlStorageEngine::open(&path).unwrap());
    let clock = Arc::new(ManualClock::new(10_000));
    let k = Arc::new(Kernel::open(engine.clone(), clock.clone(), SEED).unwrap());

    let t0 = std::time::Instant::now();
    for i in 1..=n {
        clock.tick(1);
        let mut req = RememberRequest::create(ctx(), meta());
        req.properties.insert("seq".into(), Value::Int(i as i64));
        req.properties
            .insert("subject".into(), Value::Text(format!("s{i}")));
        req.properties.insert(
            "body".into(),
            Value::Text(format!("kse19 payload {i:09} {}", "x".repeat(40))),
        );
        let _ = k.remember(req).unwrap();
    }
    let build_ms = t0.elapsed().as_secs_f64() * 1e3;

    // Pins before reporting: the type index serves everything, and the
    // store is structurally sound at this scale.
    let rows = k.scan_by_type(&Subject::new("alice"), TYPE).unwrap();
    assert_eq!(
        rows.len() as u64,
        n,
        "kse19: type scan served {}/{} after load",
        rows.len(),
        n
    );
    structural_sweep(&k, engine.as_ref(), &format!("kse19-n{n}"));

    // Store bytes by prefix family.
    let mut store_bytes = 0u64;
    let mut ko_bytes = 0u64;
    let mut index_bytes = 0u64;
    for (key, val) in engine.scan(b"").unwrap() {
        let size = (key.len() + val.len()) as u64;
        store_bytes += size;
        if key.starts_with(b"ko/") {
            ko_bytes += size;
        }
        if key.starts_with(b"head/")
            || key.starts_with(b"type/")
            || key.starts_with(b"relo/")
            || key.starts_with(b"reli/")
        {
            index_bytes += size;
        }
    }
    let wal_bytes = std::fs::metadata(&path).unwrap().len();

    std::fs::write(
        &results,
        format!(
            "n={n}\nbuild_ms={build_ms}\nstore_bytes={store_bytes}\nko_bytes={ko_bytes}\nindex_bytes={index_bytes}\nwal_bytes={wal_bytes}\n"
        ),
    )
    .unwrap();
}

struct Kse19Row {
    n: u64,
    build_ms: f64,
    peak_rss: u64,
    rss_sampled: bool,
    store_bytes: u64,
    index_bytes: u64,
    wal_bytes: u64,
}

fn measure_kse19(n: u64, label: &str) -> Kse19Row {
    let path = tmp(&format!("kse19-{label}-{n}"));
    let results = tmp(&format!("kse19-results-{label}-{n}"));
    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(&exe)
        .arg("--exact")
        .arg("kse19_loader")
        .env(LOADER_ENV, "1")
        .env(PATH_ENV, &path)
        .env(RESULTS_ENV, &results)
        .env(N_ENV, n.to_string())
        .spawn()
        .unwrap();

    // PowerShell RSS sampler (Windows only): polls WorkingSet64 of the
    // loader pid every 500 ms, exits when the loader is gone. Non-Windows
    // runs skip RSS with an honest NOT_SAMPLED row — the loader still runs
    // and the report carries heap/disk.
    let mut peak_rss = 0u64;
    let mut samples = 0usize;
    #[cfg(windows)]
    {
        let pid = child.id();
        let script = format!(
            "while (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ Write-Output (Get-Process -Id {pid}).WorkingSet64; Start-Sleep -Milliseconds 500 }}"
        );
        let mut sampler = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        // The loader's exit ends both the load and (within one sample
        // period) the sampler — reading to EOF is the wait.
        let out = sampler.stdout.take().unwrap();
        for b in BufReader::new(out).lines().map_while(Result::ok) {
            if let Ok(v) = b.trim().parse::<u64>() {
                peak_rss = peak_rss.max(v);
                samples += 1;
            }
        }
        let _ = sampler.wait();
    }
    let status = child.wait().unwrap();
    assert!(status.success(), "kse19: loader child failed");
    #[cfg(windows)]
    assert!(samples > 0, "kse19: RSS sampler collected no samples");

    let mut store_bytes = 0u64;
    let mut index_bytes = 0u64;
    let mut wal_bytes = 0u64;
    let mut build_ms = 0.0;
    for line in std::fs::read_to_string(&results).unwrap().lines() {
        let (kname, v) = line.split_once('=').unwrap();
        match kname {
            "build_ms" => build_ms = v.parse().unwrap(),
            "store_bytes" => store_bytes = v.parse().unwrap(),
            "index_bytes" => index_bytes = v.parse().unwrap(),
            "wal_bytes" => wal_bytes = v.parse().unwrap(),
            _ => {}
        }
    }

    Kse19Row {
        n,
        build_ms,
        peak_rss,
        rss_sampled: samples > 0,
        store_bytes,
        index_bytes,
        wal_bytes,
    }
}

#[test]
fn kse19_loader() {
    if std::env::var(LOADER_ENV).is_ok() {
        loader_main();
    }
    // Without the env gate this test does nothing — the loader runs only as
    // a child of measure_kse19.
}

fn sizes() -> Vec<u64> {
    match std::env::var(NIGHTLY_ENV) {
        Err(std::env::VarError::NotPresent) => vec![10_000],
        Ok(v) if v == "1" => vec![100_000, 1_000_000],
        other => panic!("KSE19_NIGHTLY strict opt-in: unset or 1, got {other:?}"),
    }
}

#[test]
fn kse19_resource_usage() {
    for n in sizes() {
        let _ = measure_kse19(n, "test");
    }
}

// ---------------------------------------------------------------------------
// Report: artifacts/storage-engine/resource-usage.md
// ---------------------------------------------------------------------------

#[test]
fn kse19_report() {
    let rows: Vec<Kse19Row> = sizes()
        .into_iter()
        .map(|n| measure_kse19(n, "report"))
        .collect();

    let mut table = String::from(
        "| KOs | build (wall≈CPU) | peak RSS | heap (live store) | \
         index memory | disk (WAL) | bytes/KO (store) | bytes/KO (disk) |\n\
         |---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    let mut projection = String::new();
    for r in &rows {
        let store_bko = r.store_bytes as f64 / r.n as f64;
        let disk_bko = r.wal_bytes as f64 / r.n as f64;
        let rss_cell = if r.rss_sampled {
            format!("{} B", r.peak_rss)
        } else {
            "NOT_SAMPLED (non-Windows)".to_string()
        };
        table.push_str(&format!(
            "| {} | {:.0} ms | {rss_cell} | {} B | {} B | {} B | {:.0} | {:.0} |\n",
            r.n, r.build_ms, r.store_bytes, r.index_bytes, r.wal_bytes, store_bko, disk_bko
        ));
    }
    // 10M projection from the largest measured row (the §25 verdict).
    if let Some(last) = rows.last() {
        projection = format!(
            "10M projection (linear in N, from the {} row): heap ≈ {:.2} GB, \
             disk ≈ {:.2} GB — the engine holds the ENTIRE graph in a \
             BTreeMap and replays 100% of the WAL at open (KSE-15), so \
             memory is linear by design. §25 verdict evidence: it DOES \
             require the whole graph in memory — ~{:.0} B/KO.",
            last.n,
            last.store_bytes as f64 / last.n as f64 * 10_000_000.0 / 1e9,
            last.wal_bytes as f64 / last.n as f64 * 10_000_000.0 / 1e9,
            last.store_bytes as f64 / last.n as f64,
        );
    }

    let profile = if cfg!(debug_assertions) {
        "debug (CPU inflated; RSS comparable)"
    } else {
        "release"
    };
    let report = format!(
        "# KSE-19 — Resource Usage (MRFC-KSE-001 §25)\n\n\
         Date: 2026-09-01 · seed {SEED:#x} · engine: AikoqlStorageEngine · \
         build profile: {profile} · sizes run in \
         this suite run: {sizes}\n\n\
         {table}\n\
         {projection}\n\n\
         ## Honest metric mapping\n\n\
         - RSS: sampled every 500 ms on the loader child (WorkingSet64) — \
         the peak is a LOWER BOUND (spikes between samples are missed); \
         includes the loader process itself\n\
         - heap: the live store bytes (Σ k+v of every row) — the BTreeMap \
         mirror IS the heap; node overhead adds a constant factor not \
         counted here\n\
         - cache memory: 0 by construction — there is no cache layer; the \
         store itself is in RAM\n\
         - index memory: the derived-index rows (head/type/relo/reli) — a \
         subset of the store, not a separate structure\n\
         - peak allocation: NOT_MEASURED (no allocator tracing wired)\n\
         - disk: WAL bytes at load end (fsync per commit — KSE-15)\n\
         - CPU: build wall time, single-threaded (wall ≈ CPU); debug build \
         inflates instruction cost, not memory\n\n\
         ## Honest limits\n\n\
         - single-writer child; RSS of concurrent access (multi-reader) not \
         measured here (KSE-13 covers behavior, not memory)\n\
         - the 10M row is a projection, not a run — running 10M is a \
         machine choice, not a gate\n\
         - peak RSS spans the WHOLE load including the end-of-load pins \
         (full-store materialization scans + per-head lineage decode) — \
         transient allocations that exist only at the tail of the run, so \
         the peak overstates steady-state ingest RSS\n\
         - RSS is Windows-only (PowerShell WorkingSet64 poll); CI (ubuntu) \
         rows carry heap/disk without RSS\n\
         - KOID identity at 1M relies on the HLC counter (clock ticks 1 ms \
         per commit here); same-instant commits are pinned by KSE-8\n",
        sizes = rows
            .iter()
            .map(|r| r.n.to_string())
            .collect::<Vec<_>>()
            .join("/"),
    );
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../artifacts/storage-engine");
    std::fs::write(dir.join("resource-usage.md"), report).unwrap();
}
