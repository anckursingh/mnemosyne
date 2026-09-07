//! KSE-142 — recovery scaling (certification §6:
//! `docs/AIKOQL_Storage_Engine_MVP_Certification_TDD.md`).
//!
//! The doc's gap: recovery semantics are proven at small WAL sizes (KSE-022,
//! KSE-083, KSE-15) but the scaling curve is unmeasured and the MVP limit
//! undefined. This suite measures open at 1/10/100 MB of WAL (1 GB opt-in)
//! and validates 100% semantic recovery at each size.
//!
//! Shape: the parent generates a sized WAL with a deterministic workload
//! (fixed keyspace, version history grows — the AIKOQL shape), then a CHILD
//! process re-derives the live model from (seed, batches) alone, opens the
//! WAL, and validates the recovered store against it byte-exact. RSS:
//! phase-anchored self-reports (final) + a 100 ms parent poll (peak, lower
//! bound). `open_ms` is the cold open; "replay" is open minus a warm-cache
//! streamed read (upper bound on replay CPU — the read inside open ran on a
//! cold cache).
//!
//! Sizing (strict opt-in, KSE-12/19 convention): the suite runs the 1 MB
//! smoke; `KSE142_NIGHTLY=1` adds 10/100 MB, `=2` also 1 GB. Any other
//! value is a FAILURE (env-set-but-dead must never silently skip). RSS is
//! Windows-only (PowerShell); non-Windows rows carry timings with an honest
//! NOT_SAMPLED RSS cell.

mod common;

use aikoql_kernel::storage::store::StorageEngine;
use aikoql_storage::AikoqlStorageEngine;
use common::{tmp, walgen};
use std::path::PathBuf;
use std::process::Command;

const SEED: u64 = 0x1420_0000;
const MB: u64 = 1_000_000;

const LOADER_ENV: &str = "KSE142_LOADER";
const PATH_ENV: &str = "KSE142_PATH";
const RESULTS_ENV: &str = "KSE142_RESULTS";
const B_ENV: &str = "KSE142_B";
const NIGHTLY_ENV: &str = "KSE142_NIGHTLY";

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
        other => panic!("KSE142_NIGHTLY strict opt-in: unset, 1, or 2, got {other:?}"),
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

/// Child: rebuild the model from (seed, B), open the WAL, and validate the
/// recovered store against it. Timings and RSS land in the results file;
/// any validation failure panics before the file is written.
fn loader_main() {
    let path = PathBuf::from(std::env::var(PATH_ENV).unwrap());
    let results = PathBuf::from(std::env::var(RESULTS_ENV).unwrap());
    let b: u64 = std::env::var(B_ENV).unwrap().parse().unwrap();

    // Model: the same seeded sequence the parent wrote (pure, no IO).
    let mut g = walgen::Gen::new(cfg());
    for _ in 0..b {
        g.step();
    }
    let model = g.model();

    // Warm-cache read time: the same page-cache IO open() performs, timed
    // without materializing the WAL (streamed through a 1 MiB buffer).
    let t_read0 = std::time::Instant::now();
    let mut f = std::fs::File::open(&path).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    let mut wal_bytes = 0usize;
    loop {
        use std::io::Read;
        let r = f.read(&mut buf).unwrap();
        if r == 0 {
            break;
        }
        wal_bytes += r;
    }
    let read_ms = t_read0.elapsed().as_secs_f64() * 1e3;

    let t_open0 = std::time::Instant::now();
    let engine = AikoqlStorageEngine::open(&path).unwrap();
    let open_ms = t_open0.elapsed().as_secs_f64() * 1e3;
    let replay_approx_ms = (open_ms - read_ms).max(0.0);
    let final_rss = self_rss_cell();

    // First query: the model's smallest key must serve immediately.
    let (first_key, first_val) = model.iter().next().unwrap();
    let t_q0 = std::time::Instant::now();
    assert_eq!(
        engine.get(first_key).unwrap().as_deref(),
        Some(first_val.as_slice()),
        "kse142: first query served wrong data"
    );
    let ttfq_ms = t_q0.elapsed().as_secs_f64() * 1e3;

    // --- correctness: 100% semantic recovery (spec §6) ---
    // logical key count
    assert_eq!(
        engine.scan(b"").unwrap().len(),
        model.len(),
        "kse142: logical key count drifted"
    );
    // reference key/value data — full equality, not a spot check
    for (k, v) in model {
        assert_eq!(
            engine.get(k).unwrap().as_deref(),
            Some(v.as_slice()),
            "kse142: reference data drifted at {:?}",
            String::from_utf8_lossy(k)
        );
    }
    // prefix scans per family, byte-exact against the model's family
    for fam in 0..cfg().families {
        let prefix = format!("{fam}/");
        let want: Vec<Vec<u8>> = model
            .keys()
            .filter(|k| k.starts_with(prefix.as_bytes()))
            .cloned()
            .collect();
        let got: Vec<Vec<u8>> = engine
            .scan(prefix.as_bytes())
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(got, want, "kse142: family {fam} prefix scan drifted");
    }
    // deletes: the last deleted key must be absent
    if let Some(d) = &g.last_del {
        assert_eq!(
            engine.get(d).unwrap(),
            None,
            "kse142: deleted key resurrected"
        );
    }
    // overwrites: the pinned multi-put key must hold its FINAL value
    if let Some((k, v)) = &g.overwrite_pin {
        assert_eq!(
            engine.get(k).unwrap().as_deref(),
            Some(v.as_slice()),
            "kse142: overwrite pinned to a stale value"
        );
    }

    std::fs::write(
        &results,
        format!(
            "read_ms={read_ms}\nopen_ms={open_ms}\nreplay_approx_ms={replay_approx_ms}\n\
             ttfq_ms={ttfq_ms}\nwal_bytes={wal_bytes}\nfinal_rss={final_rss}\n"
        ),
    )
    .unwrap();
}

#[test]
fn kse142_loader() {
    if std::env::var(LOADER_ENV).is_ok() {
        loader_main();
    }
    // Without the env gate this test does nothing — the loader runs only as
    // a child of measure_kse142.
}

struct Row {
    wal_bytes: u64,
    records: u64,
    puts: u64,
    dels: u64,
    overwrites: u64,
    recreates: u64,
    unique_keys: usize,
    live_keys: usize,
    open_ms: f64,
    replay_approx_ms: f64,
    ttfq_ms: f64,
    peak_rss: u64,
    rss_sampled: bool,
    final_rss: u64,
}

fn measure(size: u64, label: &str) -> Row {
    let path = tmp(&format!("kse142-{label}-{size}"));
    let (gen, wal_bytes) = walgen::generate(&path, cfg(), size);

    // Generator determinism pin: the child rebuilds the model from
    // (seed, batches) alone — re-run the sequence here and compare.
    let mut g2 = walgen::Gen::new(cfg());
    for _ in 0..gen.batches {
        g2.step();
    }
    assert_eq!(
        g2.model(),
        gen.model(),
        "kse142: generator not deterministic"
    );

    let results = tmp(&format!("kse142-results-{label}-{size}"));
    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(&exe)
        .arg("--exact")
        .arg("kse142_loader")
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
    assert!(status.success(), "kse142: loader child failed");

    let mut open_ms = 0.0;
    let mut replay_approx_ms = 0.0;
    let mut ttfq_ms = 0.0;
    let mut final_rss = 0u64;
    let mut child_wal = 0u64;
    for line in std::fs::read_to_string(&results).unwrap().lines() {
        let (kname, v) = line.split_once('=').unwrap();
        match kname {
            "open_ms" => open_ms = v.parse().unwrap(),
            "replay_approx_ms" => replay_approx_ms = v.parse().unwrap(),
            "ttfq_ms" => ttfq_ms = v.parse().unwrap(),
            "final_rss" => final_rss = v.parse().unwrap(),
            "wal_bytes" => child_wal = v.parse().unwrap(),
            _ => {}
        }
    }
    assert_eq!(child_wal, wal_bytes, "kse142: child read != generated WAL");

    Row {
        wal_bytes,
        records: gen.batches,
        puts: gen.puts,
        dels: gen.dels,
        overwrites: gen.overwrites,
        recreates: gen.recreates,
        unique_keys: gen.unique_keys(),
        live_keys: gen.model().len(),
        open_ms,
        replay_approx_ms,
        ttfq_ms,
        peak_rss,
        rss_sampled: samples > 0,
        final_rss,
    }
}

#[test]
fn kse142_recovery_scaling() {
    for size in sizes() {
        let _ = measure(size, "test");
    }
}

// ---------------------------------------------------------------------------
// Report: artifacts/storage-engine/kse142-recovery-scaling.md
// ---------------------------------------------------------------------------

#[test]
fn kse142_report() {
    let rows: Vec<Row> = sizes().into_iter().map(|s| measure(s, "report")).collect();

    let mut table = String::from(
        "| WAL (exact) | records | unique keys | live keys | overwrites | recreates | deletes | avg value B | \
         open ms | replay (open−read) ms | first query ms | peak RSS | final RSS |\n\
         |---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for r in &rows {
        let peak_cell = if r.rss_sampled {
            format!("{} B", r.peak_rss)
        } else {
            "NOT_SAMPLED".to_string()
        };
        let final_cell = if r.final_rss > 0 {
            format!("{} B", r.final_rss)
        } else {
            "NOT_SAMPLED".to_string()
        };
        table.push_str(&format!(
            "| {:.2} MB | {} | {} | {} | {:.1}% | {:.1}% | {:.1}% | {} | {:.1} | {:.1} | {:.3} | {peak_cell} | {final_cell} |\n",
            r.wal_bytes as f64 / 1e6,
            r.records,
            r.unique_keys,
            r.live_keys,
            r.overwrites as f64 / r.puts as f64 * 100.0,
            r.recreates as f64 / r.puts as f64 * 100.0,
            r.dels as f64 / (r.puts + r.dels) as f64 * 100.0,
            cfg().value_len,
            r.open_ms,
            r.replay_approx_ms,
            r.ttfq_ms,
        ));
    }

    // Proposed recovery SLO, data-driven from the largest measured row:
    // linear slope x 1.5 headroom. A proposal, reported — not asserted.
    let slo = rows.last().map(|last| {
        let slope_ms_per_mb = last.open_ms / (last.wal_bytes as f64 / 1e6);
        format!(
            "- open(100 MB WAL) <= {:.0} ms\n\
             - open(1 GB WAL) <= {:.0} ms\n\
             computed as linear slope ({slope_ms_per_mb:.1} ms/MB from the {:.2} MB row) x 1.5 headroom — replay is linear by construction (KSE-15).",
            slope_ms_per_mb * 100.0 * 1.5,
            slope_ms_per_mb * 1000.0 * 1.5,
            last.wal_bytes as f64 / 1e6,
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
        "# KSE-142 — Recovery Scaling (certification §6)\n\n\
         Date: 2026-09-01 · seed {SEED:#x} · engine: AikoqlStorageEngine · \
         build profile: {profile} · sizes run: {sizes_here} · test: \
         kse142_recovery_scaling.rs\n\n\
         {table}\n\n\
         ## Correctness — 100% semantic recovery (asserted, all sizes)\n\n\
         | check | pin |\n|---|---|\n\
         | logical key count | scan-all count == model live keys |\n\
         | reference key/value data | full equality of every live key against the re-derived model |\n\
         | prefix scans | 4 key families, byte-exact against the model slice |\n\
         | deletes | last deleted key serves None |\n\
         | overwrites | pinned multi-put key serves its FINAL value |\n\
         | no corruption | open succeeds only after every envelope checksum verifies (any damage fails closed — KSE-082B) |\n\
         | no OOM | the loader child completed within the measured peak RSS |\n\n\
         ## Proposed recovery SLO\n\n{slo}\n\n\
         ## Honest limits\n\n\
         - peak RSS is polled at 100 ms on the loader child — a LOWER BOUND \
         (spikes between samples are missed); a fast smoke child can outrun \
         the sampler's startup, shown as NOT_SAMPLED\n\
         - final RSS is a phase-anchored self-report taken after open(), \
         before the validation scans — validation transient memory is not in \
         it\n\
         - open_ms is the cold open (read + replay + handles); replay is \
         approximated as open minus a WARM-cache streamed read — an upper \
         bound on replay CPU, because the read inside open ran cold\n\
         - the workload keeps a fixed 10K-key keyspace with growing version \
         history — live-key scaling is KSE-19's surface, WAL scaling is \
         this one's\n\
         - RSS is Windows-only (PowerShell WorkingSet64); non-Windows rows \
         carry timings with NOT_SAMPLED RSS\n\
         - child runs race sibling tests for CPU (kse19 convention); wall \
         times are evidence, not gates\n",
        slo = slo.unwrap_or_else(|| "- (no measured rows)".into()),
    );
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../artifacts/storage-engine");
    std::fs::write(dir.join("kse142-recovery-scaling.md"), report).unwrap();
}
