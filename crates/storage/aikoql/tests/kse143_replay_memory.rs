//! KSE-143 — large replay resource stability (certification §7:
//! `docs/AIKOQL_Storage_Engine_MVP_Certification_TDD.md`).
//!
//! The doc's gap: steady-state memory is measured (KSE-19) but STARTUP PEAK
//! is not — replay materializes WAL buffers, decoder allocations, and
//! BTreeMap growth on top of the final store. The deployment risk is peak,
//! not final. This suite measures both per the doc's RED shape: generate WAL
//! → close → baseline → open (replay) → peak during replay → final RSS →
//! first query — and publishes the required multiplier Peak RSS / Final RSS.
//!
//! Shape: same deterministic workload and child-process isolation as
//! kse142_recovery_scaling (shared `common::walgen`). The child self-reports
//! RSS at three exact phases (baseline, post-open, post-query); the parent
//! polls peak at 100 ms over the child's whole window — an upper bound on
//! the replay peak (it includes the baseline and query tails) and a lower
//! bound on the true transient peak (sampling granularity).
//!
//! Sizing (strict opt-in, KSE-12/19 convention): the suite runs the 1 MB
//! smoke; `KSE143_NIGHTLY=1` adds 10/100 MB, `=2` also 1 GB. Any other
//! value is a FAILURE (env-set-but-dead must never silently skip). RSS is
//! Windows-only (PowerShell); non-Windows rows carry timings with an honest
//! NOT_SAMPLED RSS cell.

mod common;

use aikoql_kernel::storage::store::StorageEngine;
use aikoql_storage::AikoqlStorageEngine;
use common::{tmp, walgen};
use std::path::PathBuf;
use std::process::Command;

const SEED: u64 = 0x1430_0000;
const MB: u64 = 1_000_000;

const LOADER_ENV: &str = "KSE143_LOADER";
const PATH_ENV: &str = "KSE143_PATH";
const RESULTS_ENV: &str = "KSE143_RESULTS";
const B_ENV: &str = "KSE143_B";
const NIGHTLY_ENV: &str = "KSE143_NIGHTLY";

fn cfg() -> walgen::Config {
    walgen::Config {
        seed: SEED,
        keys: 10_000,
        families: 4,
        value_len: 256,
        puts_per_batch: 8,
        dels_per_batch: 1,
    }
}

fn sizes() -> Vec<u64> {
    match std::env::var(NIGHTLY_ENV) {
        Err(std::env::VarError::NotPresent) => vec![MB],
        Ok(v) if v == "1" => vec![MB, 10 * MB, 100 * MB],
        Ok(v) if v == "2" => vec![MB, 10 * MB, 100 * MB, 1000 * MB],
        other => panic!("KSE143_NIGHTLY strict opt-in: unset, 1, or 2, got {other:?}"),
    }
}

#[cfg(windows)]
fn self_rss_cell() -> u64 {
    common::self_rss().unwrap_or(0)
}
#[cfg(not(windows))]
fn self_rss_cell() -> u64 {
    0
}

/// Child, per the doc's RED shape: baseline → open (replay) → final → first
/// query. The three RSS cells are self-reported at the exact phases.
fn loader_main() {
    let path = PathBuf::from(std::env::var(PATH_ENV).unwrap());
    let results = PathBuf::from(std::env::var(RESULTS_ENV).unwrap());
    let b: u64 = std::env::var(B_ENV).unwrap().parse().unwrap();

    // Model for the first-query pin (same seeded sequence as the parent).
    let mut g = walgen::Gen::new(cfg());
    for _ in 0..b {
        g.step();
    }
    let (first_key, first_val) = {
        let (k, v) = g.model().iter().next().unwrap();
        (k.clone(), v.clone())
    };

    let baseline_rss = self_rss_cell();
    let t0 = std::time::Instant::now();
    let engine = AikoqlStorageEngine::open(&path).unwrap();
    let open_ms = t0.elapsed().as_secs_f64() * 1e3;
    let final_rss = self_rss_cell();

    assert_eq!(
        engine.get(&first_key).unwrap().as_deref(),
        Some(first_val.as_slice()),
        "kse143: first query served wrong data"
    );
    let post_query_rss = self_rss_cell();

    std::fs::write(
        &results,
        format!(
            "open_ms={open_ms}\nbaseline_rss={baseline_rss}\nfinal_rss={final_rss}\n\
             post_query_rss={post_query_rss}\nlive_keys={}\nrecords={b}\n",
            g.model().len(),
        ),
    )
    .unwrap();
}

#[test]
fn kse143_loader() {
    if std::env::var(LOADER_ENV).is_ok() {
        loader_main();
    }
    // Without the env gate this test does nothing — the loader runs only as
    // a child of measure_kse143.
}

struct Row {
    wal_bytes: u64,
    records: u64,
    live_keys: usize,
    open_ms: f64,
    baseline_rss: u64,
    peak_rss: u64,
    rss_sampled: bool,
    final_rss: u64,
    post_query_rss: u64,
}

fn measure(size: u64, label: &str) -> Row {
    let path = tmp(&format!("kse143-{label}-{size}"));
    // Generate then close — the doc's RED shape opens from a closed WAL.
    let (gen, wal_bytes) = walgen::generate(&path, cfg(), size);
    let results = tmp(&format!("kse143-results-{label}-{size}"));
    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(&exe)
        .arg("--exact")
        .arg("kse143_loader")
        .env(LOADER_ENV, "1")
        .env(PATH_ENV, &path)
        .env(RESULTS_ENV, &results)
        .env(B_ENV, gen.batches.to_string())
        .spawn()
        .unwrap();

    let (peak_rss, samples) = {
        #[cfg(windows)]
        {
            common::sample_child_peak(&mut child, 100)
        }
        #[cfg(not(windows))]
        {
            let _ = &mut child;
            (0u64, 0usize)
        }
    };
    let status = child.wait().unwrap();
    assert!(status.success(), "kse143: loader child failed");

    let mut open_ms = 0.0;
    let mut baseline_rss = 0u64;
    let mut final_rss = 0u64;
    let mut post_query_rss = 0u64;
    let mut live_keys = 0usize;
    let mut records = 0u64;
    for line in std::fs::read_to_string(&results).unwrap().lines() {
        let (kname, v) = line.split_once('=').unwrap();
        match kname {
            "open_ms" => open_ms = v.parse().unwrap(),
            "baseline_rss" => baseline_rss = v.parse().unwrap(),
            "final_rss" => final_rss = v.parse().unwrap(),
            "post_query_rss" => post_query_rss = v.parse().unwrap(),
            "live_keys" => live_keys = v.parse().unwrap(),
            "records" => records = v.parse().unwrap(),
            _ => {}
        }
    }
    assert_eq!(records, gen.batches, "kse143: child record count drifted");

    Row {
        wal_bytes,
        records,
        live_keys,
        open_ms,
        baseline_rss,
        peak_rss,
        rss_sampled: samples > 0,
        final_rss,
        post_query_rss,
    }
}

#[test]
fn kse143_replay_memory() {
    for size in sizes() {
        let _ = measure(size, "test");
    }
}

// ---------------------------------------------------------------------------
// Report: artifacts/storage-engine/kse143-replay-memory.md
// ---------------------------------------------------------------------------

#[test]
fn kse143_report() {
    let rows: Vec<Row> = sizes().into_iter().map(|s| measure(s, "report")).collect();

    let mut table = String::from(
        "| WAL (exact) | records | live keys | baseline RSS | peak RSS | final RSS | \
         post-query RSS | peak/final | open ms |\n\
         |---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for r in &rows {
        let sampled = r.rss_sampled && r.baseline_rss > 0 && r.final_rss > 0;
        let (baseline_cell, peak_cell, final_cell, post_query_cell, ratio_cell) = if sampled {
            (
                format!("{} B", r.baseline_rss),
                format!("{} B", r.peak_rss),
                format!("{} B", r.final_rss),
                format!("{} B", r.post_query_rss),
                format!("{:.2}x", r.peak_rss as f64 / r.final_rss as f64),
            )
        } else {
            (
                "NOT_SAMPLED".to_string(),
                "NOT_SAMPLED".to_string(),
                "NOT_SAMPLED".to_string(),
                "NOT_SAMPLED".to_string(),
                "NOT_SAMPLED".to_string(),
            )
        };
        table.push_str(&format!(
            "| {:.2} MB | {} | {} | {baseline_cell} | {peak_cell} | {final_cell} | {post_query_cell} | {ratio_cell} | {:.1} |\n",
            r.wal_bytes as f64 / 1e6,
            r.records,
            r.live_keys,
            r.open_ms,
        ));
    }

    // The required headline + a data-driven deployment memory proposal.
    let headline = rows
        .iter()
        .rfind(|r| r.rss_sampled && r.final_rss > 0)
        .map(|last| {
            let ratio = last.peak_rss as f64 / last.final_rss as f64;
            // Marginal slope: the WAL-dependent part of the peak, with the
            // process baseline (which the 1 MB row is dominated by) taken out.
            let marginal =
                (last.peak_rss.saturating_sub(last.baseline_rss)) as f64 / last.wal_bytes as f64;
            let wal_mb = last.wal_bytes as f64 / 1e6;
            let cap_mb = 100.0;
            format!(
                "Peak replay memory multiplier = **{ratio:.2}x** (peak {:.0} B / \
                 final {:.0} B, at {wal_mb:.2} MB WAL).\n\n\
                 Beyond the ~{:.0} MB process baseline, peak grows at \
                 {marginal:.2} B per WAL byte (marginal slope, {wal_mb:.2} MB \
                 row). Proposed deployment memory requirement: baseline + \
                 {marginal:.2} B/WAL-byte x the operational WAL cap x 1.2 \
                 headroom — e.g. an operational {cap_mb:.0} MB WAL implies \
                 ~{:.0} MB RAM reserved for open().",
                last.peak_rss,
                last.final_rss,
                last.baseline_rss as f64 / 1e6,
                (last.baseline_rss as f64 + marginal * cap_mb * 1e6 * 1.2) / 1e6,
            )
        });

    let profile = if cfg!(debug_assertions) {
        "debug (CPU inflated; RSS comparable)"
    } else {
        "release"
    };
    let sizes_here = rows
        .iter()
        .map(|r| format!("{:.0} MB", r.wal_bytes as f64 / 1e6))
        .collect::<Vec<_>>()
        .join("/");
    let report = format!(
        "# KSE-143 — Large Replay Resource Stability (certification §7)\n\n\
         Date: 2026-09-01 · seed {SEED:#x} · engine: AikoqlStorageEngine · \
         build profile: {profile} · sizes run: {sizes_here} · test: \
         kse143_replay_memory.rs\n\n\
         {headline}\n\n\
         {table}\n\n\
         ## Honest limits\n\n\
         - peak RSS is polled at 100 ms over the child's WHOLE window \
         (baseline + open + query) — an upper bound on the replay peak and \
         a lower bound on the true transient (spikes between samples are \
         missed); a fast smoke child can outrun the sampler's startup, \
         shown as NOT_SAMPLED\n\
         - baseline/final/post-query RSS are phase-anchored self-reports \
         (one PowerShell call each, ~200 ms of child wall time — not replay \
         time)\n\
         - open() materializes the WHOLE WAL plus the live store (KSE-19 \
         §25 verdict) — the multiplier measures that design's startup cost, \
         not a hidden cache\n\
         - the workload keeps a fixed 10K-key keyspace; peak memory is \
         dominated by WAL bytes + final store, both linear in the reported \
         dimensions\n\
         - RSS is Windows-only (PowerShell WorkingSet64); non-Windows rows \
         carry timings with NOT_SAMPLED RSS\n\
         - child runs race sibling tests for CPU (kse19 convention); the \
         memory numbers are process-isolated and not affected by siblings\n",
        headline = headline.unwrap_or_else(|| {
            "Peak replay memory multiplier = NOT_SAMPLED (no RSS rows on this platform)".into()
        }),
    );
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../artifacts/storage-engine");
    std::fs::write(dir.join("kse143-replay-memory.md"), report).unwrap();
}
