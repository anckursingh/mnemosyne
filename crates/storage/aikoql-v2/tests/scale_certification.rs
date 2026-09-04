//! SE2-M14 — realistic scale certification (QA M8). Two deliverables:
//!
//! 1. `ds_perf_smoke` (always-on) — the DS-PERF dataset builder + every
//!    gate pin at 400 KOs: kernel-shaped rows (ko version rows, head, type
//!    index, mirrored relo/reli ring, F=10/100/1000 fan-out hubs), byte-exact
//!    verification, and the cold/warm/hot cache-state pins the nightly runs
//!    at scale.
//! 2. `scale_certification` (`SE2M14_NIGHTLY` strict opt-in: unset skips,
//!    "1" = DS-PERF-M (100K KOs × 5 versions + relationships), "2" = M +
//!    DS-PERF-L (1M × 10)) — the QA M8 benchmark matrix (cold/warm/hot ×
//!    W1/W2/W3/W4/W5/W7/W8) with throughput, P50/P95/P99, bytes read,
//!    cache hit rate, segments touched, fsync count; the QA gates as report
//!    verdicts (cold point ≤ 100 µs, warm ≤ 50 µs, hot head ≤ 20 µs,
//!    fanout F=10/100/1000 ≤ 1/10/50 ms, hot context ≤ 100 µs, group commit
//!    cited from SE2-M13); redb parity rows; RSS via a loader child; the
//!    V2-Adopt matrix re-runs on the same harness as a child
//!    (`V2ADOPT_NIGHTLY=1`, regenerating workloads.md).
//!
//! Artifact: artifacts/storage-engine-v2/scale-certification.md. Perf
//! numbers are report cells, never asserts — the pins are answer
//! correctness and cache state.

mod common;

use aikoql_storage_v2::cache::CacheStats;
use aikoql_storage_v2::db::{Config, Db};
use aikoql_storage_v2::stats::ReadPathStats;
use aikoql_storage_v2::wal::Op;
use common::{dir, percentiles, run_date, tmp};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

#[cfg(windows)]
use std::io::{BufRead, BufReader};
#[cfg(windows)]
use std::process::Stdio;

const GATE: &str = "SE2M14_NIGHTLY";
const LOADER_ENV: &str = "SE2M14_LOADER";
const SEED: u64 = 0x14_0000;
const N_TYPES: usize = 100;
const RING: usize = 10;
/// The warm sample: 400 evenly spaced KOs ≈ 6.4 MiB of blocks — under the
/// 8 MiB default cache, so the warm POINT pin (zero misses) is exact; scan
/// rows use `warm_pass` (their working set exceeds the cache — see there).
const SAMPLE: usize = 400;
const TYPE_SCANS: usize = 20;
const HOT_LOOKUPS: usize = 100_000;
const HOT_CTX_LOOKUPS: usize = 2_000;

// QA M8 gates — report verdicts, never asserts.
const COLD_GATE_NS: u128 = 100_000;
const WARM_GATE_NS: u128 = 50_000;
const HOT_HEAD_GATE_NS: u128 = 20_000;
const FANOUT_GATES_NS: [u128; 3] = [1_000_000, 10_000_000, 50_000_000];
const HOT_CONTEXT_GATE_NS: u128 = 100_000;

#[derive(Clone, Copy)]
struct Size {
    n: usize,
    versions: usize,
    label: &'static str,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Off,
    M,
    ML,
}

fn mode() -> Mode {
    match std::env::var(GATE) {
        Err(_) => Mode::Off,
        Ok(v) if v == "1" => Mode::M,
        Ok(v) if v == "2" => Mode::ML,
        Ok(v) => {
            panic!("{GATE} strict opt-in: unset, 1 (DS-PERF-M) or 2 (M + DS-PERF-L), got {v:?}")
        }
    }
}

fn datasets(m: Mode) -> Vec<Size> {
    let mut v = vec![Size {
        n: 100_000,
        versions: 5,
        label: "DS-PERF-M",
    }];
    if m == Mode::ML {
        v.push(Size {
            n: 1_000_000,
            versions: 10,
            label: "DS-PERF-L",
        });
    }
    v
}

// xorshift64* — seeded, deterministic
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

fn koid_of(r: &mut Xs) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[..8].copy_from_slice(&r.next().to_be_bytes());
    k[8..].copy_from_slice(&r.next().to_be_bytes());
    k
}

fn obj_key(koid: &[u8; 16], ts: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(3 + 16 + 8);
    v.extend_from_slice(b"ko/");
    v.extend_from_slice(koid);
    v.extend_from_slice(&ts.to_be_bytes());
    v
}

fn head_key(koid: &[u8; 16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + 16);
    v.extend_from_slice(b"head/");
    v.extend_from_slice(koid);
    v
}

fn type_key(t: usize, koid: &[u8; 16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + 10 + 1 + 16);
    v.extend_from_slice(format!("type/m7t_{t}/").as_bytes());
    v.extend_from_slice(koid);
    v
}

fn rel_out_key(src: &[u8; 16], rel: &str, dst: &[u8; 16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + 16 + 1 + rel.len() + 1 + 16);
    v.extend_from_slice(b"relo/");
    v.extend_from_slice(src);
    v.push(b'/');
    v.extend_from_slice(rel.as_bytes());
    v.push(b'/');
    v.extend_from_slice(dst);
    v
}

fn rel_in_key(dst: &[u8; 16], rel: &str, src: &[u8; 16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + 16 + 1 + rel.len() + 1 + 16);
    v.extend_from_slice(b"reli/");
    v.extend_from_slice(dst);
    v.push(b'/');
    v.extend_from_slice(rel.as_bytes());
    v.push(b'/');
    v.extend_from_slice(src);
    v
}

fn rel_prefix(tag: &[u8; 4], who: &[u8; 16], rel: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + 16 + 1 + rel.len() + 1);
    v.extend_from_slice(tag);
    v.push(b'/');
    v.extend_from_slice(who);
    v.push(b'/');
    v.extend_from_slice(rel.as_bytes());
    v.push(b'/');
    v
}

fn ko_prefix(koid: &[u8; 16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(3 + 16);
    v.extend_from_slice(b"ko/");
    v.extend_from_slice(koid);
    v
}

fn version_value(i: usize, ts: u64) -> Vec<u8> {
    format!("v-{i:07}-{ts:02}{}", "x".repeat(90)).into_bytes()
}

fn head_value(i: usize) -> Vec<u8> {
    format!("h-{i:07}-{}", "y".repeat(140)).into_bytes()
}

fn sample_indices(sz: Size) -> Vec<usize> {
    let n = SAMPLE.min(sz.n);
    (0..n).map(|j| j * sz.n / n).collect()
}

struct Built {
    koids: Vec<[u8; 16]>,
    hubs: [[u8; 16]; 3],
    fans: [usize; 3],
    batches: u64,
    rows: u64,
    wall_ms: f64,
    fsyncs: u64,
    disk: u64,
}

fn file_len(path: &Path) -> u64 {
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

/// One KO = versions `ko/` rows + head + type index + 2×RING mirrored rels
/// in ONE atomic batch (the kernel's create shape); the hubs then gain
/// F=100 / F=1000 `fan` edges (the RMW restatement shape). Sync durability,
/// default memtable/cache/compaction — the production defaults.
fn build_dataset(dir: &Path, sz: Size) -> Built {
    let db = Db::open(Config::new(dir.to_path_buf())).unwrap();
    let mut rng = Xs(SEED);
    let koids: Vec<[u8; 16]> = (0..sz.n).map(|_| koid_of(&mut rng)).collect();
    let t0 = Instant::now();
    let mut rows = 0u64;
    for i in 0..sz.n {
        let mut ops = Vec::with_capacity(sz.versions + 2 + 2 * RING);
        for ts in 1..=sz.versions as u64 {
            ops.push(Op::Put(obj_key(&koids[i], ts), version_value(i, ts)));
        }
        ops.push(Op::Put(head_key(&koids[i]), head_value(i)));
        ops.push(Op::Put(type_key(i % N_TYPES, &koids[i]), b"1".to_vec()));
        for r in 1..=RING {
            ops.push(Op::Put(
                rel_out_key(&koids[i], "links", &koids[(i + r) % sz.n]),
                b"1".to_vec(),
            ));
            ops.push(Op::Put(
                rel_in_key(&koids[(i + r) % sz.n], "links", &koids[i]),
                b"1".to_vec(),
            ));
        }
        db.write(&ops).unwrap();
        rows += ops.len() as u64;
    }
    let hubs = [koids[10], koids[11], koids[12]];
    let fans = [RING, 100.min(sz.n / 2), 1000.min(sz.n / 2)];
    for (h, f) in [(1usize, fans[1]), (2, fans[2])] {
        let mut ops = Vec::with_capacity(2 * f);
        for t in 0..f {
            let dst = &koids[(t * 7919 + 13) % sz.n];
            ops.push(Op::Put(rel_out_key(&hubs[h], "fan", dst), b"1".to_vec()));
            ops.push(Op::Put(rel_in_key(dst, "fan", &hubs[h]), b"1".to_vec()));
        }
        db.write(&ops).unwrap();
        rows += ops.len() as u64;
    }
    db.flush().unwrap();
    let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let fsyncs = db.fsync_count();
    let disk = file_len(dir);
    drop(db);
    Built {
        koids,
        hubs,
        fans,
        batches: (sz.n + 2) as u64,
        rows,
        wall_ms,
        fsyncs,
        disk,
    }
}

fn verify(dir: &Path, b: &Built, sz: Size) {
    let db = Db::open(Config::new(dir.to_path_buf())).unwrap();
    // every sampled KO byte-exact: versions, head, type index, mirror ring
    let mut rng = Xs(SEED ^ 0x5eed);
    for _ in 0..SAMPLE.min(sz.n) {
        let i = rng.below(sz.n);
        let koid = &b.koids[i];
        for ts in 1..=sz.versions as u64 {
            assert_eq!(
                db.get(&obj_key(koid, ts)).unwrap(),
                Some(version_value(i, ts)),
                "version {i}/{ts} diverged"
            );
        }
        assert_eq!(
            db.get(&head_key(koid)).unwrap(),
            Some(head_value(i)),
            "head {i} diverged"
        );
        assert_eq!(
            db.get(&type_key(i % N_TYPES, koid)).unwrap(),
            Some(b"1".to_vec()),
            "type index {i} diverged"
        );
        assert_eq!(
            db.scan(&rel_prefix(b"relo", koid, "links")).unwrap().len(),
            RING,
            "out-ring {i} drifted"
        );
        assert_eq!(
            db.scan(&rel_prefix(b"reli", koid, "links")).unwrap().len(),
            RING,
            "in-ring {i} drifted"
        );
    }
    // hubs + type scan exact
    assert_eq!(
        db.scan(&rel_prefix(b"relo", &b.hubs[0], "links"))
            .unwrap()
            .len(),
        b.fans[0]
    );
    assert_eq!(
        db.scan(&rel_prefix(b"relo", &b.hubs[1], "fan"))
            .unwrap()
            .len(),
        b.fans[1]
    );
    assert_eq!(
        db.scan(&rel_prefix(b"relo", &b.hubs[2], "fan"))
            .unwrap()
            .len(),
        b.fans[2]
    );
    assert_eq!(
        db.scan(b"type/m7t_0/").unwrap().len(),
        sz.n / N_TYPES,
        "type scan drifted"
    );
    drop(db);
}

#[derive(Default)]
struct Cells {
    blocks: u64,
    bytes: u64,
    segs: u64,
    hits: u64,
    misses: u64,
}

struct Row {
    label: String,
    ops: u64,
    wall_ms: f64,
    p50: u64,
    p95: u64,
    p99: u64,
    cells: Cells,
}

fn row_from(
    label: String,
    lats: Vec<u128>,
    wall_ms: f64,
    s0: ReadPathStats,
    s1: ReadPathStats,
    c0: CacheStats,
    c1: CacheStats,
) -> Row {
    let ops = lats.len() as u64;
    let (p50, p95, p99) = percentiles(lats);
    Row {
        label,
        ops,
        wall_ms,
        p50: p50 as u64,
        p95: p95 as u64,
        p99: p99 as u64,
        cells: Cells {
            blocks: s1.blocks_read - s0.blocks_read,
            bytes: s1.bytes_read - s0.bytes_read,
            segs: s1.segments_considered - s0.segments_considered,
            hits: c1.hits - c0.hits,
            misses: c1.misses - c0.misses,
        },
    }
}

fn timed(db: &Db, ops: usize, mut run: impl FnMut(&Db)) -> Row {
    let s0 = db.read_path_stats();
    let c0 = db.cache_stats();
    let mut lats = Vec::with_capacity(ops);
    let t0 = Instant::now();
    for _ in 0..ops {
        let s = Instant::now();
        run(db);
        lats.push(s.elapsed().as_nanos());
    }
    row_from(
        String::new(),
        lats,
        t0.elapsed().as_secs_f64() * 1000.0,
        s0,
        db.read_path_stats(),
        c0,
        db.cache_stats(),
    )
}

/// A warm POINT row: an uncounted pre-warm of the same ops, then the timed
/// pass with the exact pin — zero cache misses and zero block reads (the
/// measurement must BE the cached path, or the report lies). Only rows whose
/// working set fits the 8 MiB cache may use this (point gets; fan-outs
/// F ≤ 100).
fn warm_pinned(db: &Db, pre: impl FnOnce(&Db), ops: usize, run: impl FnMut(&Db)) -> Row {
    pre(db);
    let s0 = db.read_path_stats();
    let c0 = db.cache_stats();
    let r = timed(db, ops, run);
    let c1 = db.cache_stats();
    let s1 = db.read_path_stats();
    assert_eq!(
        c1.misses, c0.misses,
        "{}: a warm pass must be all cache hits",
        r.label
    );
    assert_eq!(
        s1.blocks_read, s0.blocks_read,
        "{}: a warm pass reads no blocks",
        r.label
    );
    r
}

/// A warm SCAN row: second pass after an uncounted pre-warm. No cache pin —
/// SE2-M12's k-way scan cursors walk one block per overlapping segment
/// (~5 at M scale), so a scan working set is ~5× the 8 MiB cache by
/// construction; the hits/misses cells report the thrash honestly. The
/// per-op answer asserts stay.
fn warm_pass(db: &Db, pre: impl FnOnce(&Db), ops: usize, run: impl FnMut(&Db)) -> Row {
    pre(db);
    timed(db, ops, run)
}

type Target = ([u8; 16], usize);

fn traverse(db: &Db, src: &[u8; 16], rel: &str, f: usize) {
    let out = db.scan(&rel_prefix(b"relo", src, rel)).unwrap();
    assert_eq!(out.len(), f, "fanout drifted");
    for (k, _) in &out {
        let dst: [u8; 16] = k[k.len() - 16..].try_into().unwrap();
        assert!(
            db.get(&head_key(&dst)).unwrap().is_some(),
            "fanout target head missing"
        );
    }
}

fn context(db: &Db, koid: &[u8; 16], sz: Size) {
    assert!(db.get(&head_key(koid)).unwrap().is_some());
    let out = db.scan(&rel_prefix(b"relo", koid, "links")).unwrap();
    assert_eq!(out.len(), RING, "context ring drifted");
    for (k, _) in &out {
        let dst: [u8; 16] = k[k.len() - 16..].try_into().unwrap();
        assert!(
            db.get(&head_key(&dst)).unwrap().is_some(),
            "context target head missing"
        );
    }
    let hist = db.scan(&ko_prefix(koid)).unwrap();
    assert_eq!(hist.len(), sz.versions, "context history drifted");
}

fn matrix(dir: &Path, b: &Built, sz: Size) -> Vec<Row> {
    let mut rows = Vec::new();
    let sample = sample_indices(sz);
    let targets: Vec<Target> = sample.iter().map(|&i| (b.koids[i], i)).collect();

    // ---- cold: cache detached (cache_bytes = 0) ----
    let mut cfg = Config::new(dir.to_path_buf());
    cfg.cache_bytes = 0;
    let db = Db::open(cfg).unwrap();
    {
        let cs = db.cache_stats();
        assert_eq!(
            (cs.hits, cs.misses, cs.bytes),
            (0, 0, 0),
            "cache_bytes=0 must detach the cache"
        );
    }
    {
        let mut it = targets.iter();
        let mut r = timed(&db, targets.len(), |db| {
            let &(koid, i) = it.next().unwrap();
            assert_eq!(
                db.get(&head_key(&koid)).unwrap(),
                Some(head_value(i)),
                "cold head diverged"
            );
        });
        r.label = "head get · cold".into();
        rows.push(r);
    }
    {
        let mut it = targets.iter();
        let mut r = timed(&db, targets.len(), |db| {
            let &(koid, i) = it.next().unwrap();
            let ts = 1 + (i % sz.versions) as u64;
            assert_eq!(
                db.get(&obj_key(&koid, ts)).unwrap(),
                Some(version_value(i, ts)),
                "cold version diverged"
            );
        });
        r.label = "version get · cold".into();
        rows.push(r);
    }
    {
        let mut it = targets.iter();
        let mut r = timed(&db, targets.len(), |db| {
            let &(koid, _) = it.next().unwrap();
            let hist = db.scan(&ko_prefix(&koid)).unwrap();
            assert_eq!(hist.len(), sz.versions, "history drifted");
        });
        r.label = "history · cold".into();
        rows.push(r);
    }
    for h in 0..3 {
        let f = b.fans[h];
        let rel = if h == 0 { "links" } else { "fan" };
        let src = &b.hubs[h];
        let ops = (2000 / f).max(4);
        let mut r = timed(&db, ops, |db| traverse(db, src, rel, f));
        r.label = format!("fanout F={f} · cold");
        rows.push(r);
    }
    {
        let mut t = 0usize;
        let mut r = timed(&db, TYPE_SCANS, |db| {
            let p = format!("type/m7t_{t}/");
            t += 1;
            assert_eq!(
                db.scan(p.as_bytes()).unwrap().len(),
                sz.n / N_TYPES,
                "type scan drifted"
            );
        });
        r.label = "type scan · cold".into();
        rows.push(r);
    }
    {
        let mut it = targets.iter();
        let mut r = timed(&db, targets.len() / 4, |db| {
            let &(koid, _) = it.next().unwrap();
            context(db, &koid, sz);
        });
        r.label = "context · cold".into();
        rows.push(r);
    }
    drop(db);

    // ---- warm + hot: default cache (8 MiB), one open — per-row pins ----
    let db = Db::open(Config::new(dir.to_path_buf())).unwrap();
    {
        let mut r = warm_pinned(
            &db,
            |db| {
                for &(koid, _) in &targets {
                    let _ = db.get(&head_key(&koid)).unwrap();
                }
            },
            targets.len(),
            {
                let mut it = targets.iter();
                move |db| {
                    let &(koid, i) = it.next().unwrap();
                    assert_eq!(
                        db.get(&head_key(&koid)).unwrap(),
                        Some(head_value(i)),
                        "warm head diverged"
                    );
                }
            },
        );
        r.label = "head get · warm".into();
        rows.push(r);
    }
    {
        let mut r = warm_pinned(
            &db,
            |db| {
                for &(koid, i) in &targets {
                    let ts = 1 + (i % sz.versions) as u64;
                    let _ = db.get(&obj_key(&koid, ts)).unwrap();
                }
            },
            targets.len(),
            {
                let mut it = targets.iter();
                move |db| {
                    let &(koid, i) = it.next().unwrap();
                    let ts = 1 + (i % sz.versions) as u64;
                    assert_eq!(
                        db.get(&obj_key(&koid, ts)).unwrap(),
                        Some(version_value(i, ts)),
                        "warm version diverged"
                    );
                }
            },
        );
        r.label = "version get · warm".into();
        rows.push(r);
    }
    {
        let mut r = warm_pass(
            &db,
            |db| {
                for &(koid, _) in &targets {
                    let _ = db.scan(&ko_prefix(&koid)).unwrap();
                }
            },
            targets.len(),
            {
                let mut it = targets.iter();
                move |db| {
                    let &(koid, _) = it.next().unwrap();
                    let hist = db.scan(&ko_prefix(&koid)).unwrap();
                    assert_eq!(hist.len(), sz.versions, "warm history drifted");
                }
            },
        );
        r.label = "history · warm".into();
        rows.push(r);
    }
    for h in 0..3 {
        let f = b.fans[h];
        let rel = if h == 0 { "links" } else { "fan" };
        let src = b.hubs[h];
        let ops = (2000 / f).max(4);
        let mut r = if f <= 100 {
            warm_pinned(
                &db,
                |db| traverse(db, &src, rel, f),
                ops,
                move |db| traverse(db, &src, rel, f),
            )
        } else {
            // F=1000's ~16 MiB head working set exceeds the 8 MiB cache —
            // no zero-miss pin here; the hits/misses cells report the
            // thrash honestly (cache sizing for big fan-outs is an M14
            // finding, not a hidden knob).
            traverse(&db, &src, rel, f); // pre-warm, uncounted
            timed(&db, ops, move |db| traverse(db, &src, rel, f))
        };
        r.label = format!("fanout F={f} · warm");
        rows.push(r);
    }
    {
        let mut r = warm_pass(
            &db,
            |db| {
                for t in 0..TYPE_SCANS {
                    let p = format!("type/m7t_{t}/");
                    let _ = db.scan(p.as_bytes()).unwrap();
                }
            },
            TYPE_SCANS,
            {
                let mut t = 0usize;
                move |db| {
                    let p = format!("type/m7t_{t}/");
                    t += 1;
                    assert_eq!(
                        db.scan(p.as_bytes()).unwrap().len(),
                        sz.n / N_TYPES,
                        "type scan drifted"
                    );
                }
            },
        );
        r.label = "type scan · warm".into();
        rows.push(r);
    }
    {
        let mut r = warm_pass(
            &db,
            |db| {
                for &(koid, _) in &targets[..targets.len() / 4] {
                    context(db, &koid, sz);
                }
            },
            targets.len() / 4,
            {
                let mut it = targets.iter();
                move |db| {
                    let &(koid, _) = it.next().unwrap();
                    context(db, &koid, sz);
                }
            },
        );
        r.label = "context · warm".into();
        rows.push(r);
    }
    {
        let koid = b.koids[0];
        let want = head_value(0);
        assert_eq!(
            db.get(&head_key(&koid)).unwrap(),
            Some(want.clone()),
            "hot head pre-warm diverged"
        );
        let s0 = db.read_path_stats();
        let c0 = db.cache_stats();
        let t0 = Instant::now();
        let mut lats = Vec::with_capacity(HOT_LOOKUPS);
        for _ in 0..HOT_LOOKUPS {
            let s = Instant::now();
            assert_eq!(
                db.get(&head_key(&koid)).unwrap().as_deref(),
                Some(&want[..]),
                "the cached path must never change an answer"
            );
            lats.push(s.elapsed().as_nanos());
        }
        let c1 = db.cache_stats();
        let s1 = db.read_path_stats();
        assert!(
            c1.hits - c0.hits >= HOT_LOOKUPS as u64,
            "hot head must hit the cache"
        );
        assert_eq!(
            s1.blocks_read, s0.blocks_read,
            "a hot head performs no block read"
        );
        rows.push(row_from(
            "head get · hot".into(),
            lats,
            t0.elapsed().as_secs_f64() * 1000.0,
            s0,
            s1,
            c0,
            c1,
        ));
    }
    {
        let koid = b.koids[0];
        context(&db, &koid, sz); // pre-warm, uncounted
        let s0 = db.read_path_stats();
        let c0 = db.cache_stats();
        let t0 = Instant::now();
        let mut lats = Vec::with_capacity(HOT_CTX_LOOKUPS);
        for _ in 0..HOT_CTX_LOOKUPS {
            let s = Instant::now();
            context(&db, &koid, sz);
            lats.push(s.elapsed().as_nanos());
        }
        let c1 = db.cache_stats();
        let s1 = db.read_path_stats();
        assert_eq!(
            s1.blocks_read, s0.blocks_read,
            "a hot context reads no blocks"
        );
        assert!(
            c1.hits - c0.hits >= HOT_CTX_LOOKUPS as u64,
            "a hot context must be cache-served"
        );
        rows.push(row_from(
            "context · hot".into(),
            lats,
            t0.elapsed().as_secs_f64() * 1000.0,
            s0,
            s1,
            c0,
            c1,
        ));
    }
    {
        // W8 runs last: its write leg lands in the active memtable and
        // nothing after it reads the mutated heads. No warm pin — a mixed
        // row's working set is not a pure-read cache question; the cells
        // report hits/misses honestly.
        let mut rng = Xs(SEED ^ 0x88);
        let s0 = db.read_path_stats();
        let c0 = db.cache_stats();
        let t0 = Instant::now();
        let mut lats = Vec::with_capacity(targets.len());
        for &(koid, i) in &targets {
            let s = Instant::now();
            match rng.next() % 100 {
                0..=69 => {
                    let _ = db.get(&head_key(&koid)).unwrap();
                }
                70..=89 => {
                    let _ = db.scan(&rel_prefix(b"relo", &koid, "links")).unwrap();
                }
                _ => {
                    db.write(&[Op::Put(head_key(&koid), head_value(i))])
                        .unwrap();
                }
            }
            lats.push(s.elapsed().as_nanos());
        }
        rows.push(row_from(
            "mixed 70/20/10 · warm".into(),
            lats,
            t0.elapsed().as_secs_f64() * 1000.0,
            s0,
            db.read_path_stats(),
            c0,
            db.cache_stats(),
        ));
    }
    rows
}

/// redb parity rows — first pass / second pass on the same open (redb has
/// no block-cache knob). Parity reference only: the gates are v2's, redb
/// gets no verdict.
fn redb_rows(dir: &Path, b: &Built, sz: Size) -> Vec<Row> {
    use aikoql_kernel::storage::store::{StorageEngine, WriteBatch};
    use aikoql_kernel::storage::store_redb::RedbEngine;

    let engine = RedbEngine::open(dir).unwrap();
    for i in 0..sz.n {
        let mut batch = WriteBatch::new();
        for ts in 1..=sz.versions as u64 {
            batch.put(obj_key(&b.koids[i], ts), version_value(i, ts));
        }
        batch.put(head_key(&b.koids[i]), head_value(i));
        batch.put(type_key(i % N_TYPES, &b.koids[i]), b"1".to_vec());
        for r in 1..=RING {
            batch.put(
                rel_out_key(&b.koids[i], "links", &b.koids[(i + r) % sz.n]),
                b"1".to_vec(),
            );
            batch.put(
                rel_in_key(&b.koids[(i + r) % sz.n], "links", &b.koids[i]),
                b"1".to_vec(),
            );
        }
        engine.write_batch(&batch).unwrap();
    }
    for (h, f) in [(1usize, b.fans[1]), (2, b.fans[2])] {
        let mut batch = WriteBatch::new();
        for t in 0..f {
            let dst = &b.koids[(t * 7919 + 13) % sz.n];
            batch.put(rel_out_key(&b.hubs[h], "fan", dst), b"1".to_vec());
            batch.put(rel_in_key(dst, "fan", &b.hubs[h]), b"1".to_vec());
        }
        engine.write_batch(&batch).unwrap();
    }

    let mut rows = Vec::new();
    let sample = sample_indices(sz);
    for label in ["head get · cold", "head get · warm"] {
        let t0 = Instant::now();
        let mut lats = Vec::with_capacity(sample.len());
        for &i in &sample {
            let s = Instant::now();
            assert_eq!(
                engine.get(&head_key(&b.koids[i])).unwrap(),
                Some(head_value(i)),
                "redb head diverged"
            );
            lats.push(s.elapsed().as_nanos());
        }
        let ops = lats.len() as u64;
        let (p50, p95, p99) = percentiles(lats);
        rows.push(Row {
            label: label.into(),
            ops,
            wall_ms: t0.elapsed().as_secs_f64() * 1000.0,
            p50: p50 as u64,
            p95: p95 as u64,
            p99: p99 as u64,
            cells: Cells::default(),
        });
    }
    let f = b.fans[2];
    for label in ["cold", "warm"] {
        let t0 = Instant::now();
        let mut lats = Vec::with_capacity(4);
        for _ in 0..4 {
            let s = Instant::now();
            let out = engine
                .scan(&rel_prefix(b"relo", &b.hubs[2], "fan"))
                .unwrap();
            assert_eq!(out.len(), f, "redb fanout drifted");
            for (k, _) in &out {
                let dst: [u8; 16] = k[k.len() - 16..].try_into().unwrap();
                assert!(
                    engine.get(&head_key(&dst)).unwrap().is_some(),
                    "redb fanout target missing"
                );
            }
            lats.push(s.elapsed().as_nanos());
        }
        let ops = lats.len() as u64;
        let (p50, p95, p99) = percentiles(lats);
        rows.push(Row {
            label: format!("fanout F={f} · {label}"),
            ops,
            wall_ms: t0.elapsed().as_secs_f64() * 1000.0,
            p50: p50 as u64,
            p95: p95 as u64,
            p99: p99 as u64,
            cells: Cells::default(),
        });
    }
    rows
}

/// RSS: Windows WorkingSet64 poll on a loader child re-seeding the same
/// dataset (peak is a lower bound — the kse19 pattern).
fn measure_rss(sz: Size) -> Option<u64> {
    #[cfg(not(windows))]
    {
        let _ = sz;
        return None;
    }
    #[cfg(windows)]
    {
        let exe = std::env::current_exe().unwrap();
        let mut child = Command::new(&exe)
            .arg("--exact")
            .arg("ds_perf_loader")
            .env(LOADER_ENV, sz.label)
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
        assert!(status.success(), "v2 loader child failed");
        assert!(samples > 0, "v2 RSS sampler collected no samples");
        Some(peak)
    }
}

/// A sibling test binary from the same deps dir — `current_exe()` is THIS
/// binary; the workloads test lives in kse_m7_v2_workloads (the v2-adopt
/// harness, which this nightly re-runs).
fn sibling_test_binary(name: &str) -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let f = p.file_name().unwrap().to_string_lossy();
        if f.starts_with(name) && f.ends_with(std::env::consts::EXE_SUFFIX) {
            return p;
        }
    }
    panic!("test binary {name} not found next to {}", exe.display());
}

/// The adoption matrix re-runs on the same harness (the SE2-M14 plan line):
/// the V2-Adopt child regenerates workloads.md.
fn rerun_adoption_matrix() {
    let exe = sibling_test_binary("kse_m7_v2_workloads");
    let status = Command::new(&exe)
        .arg("--exact")
        .arg("v2_m7_workloads")
        .env("V2ADOPT_NIGHTLY", "1")
        .status()
        .unwrap();
    assert!(status.success(), "adoption matrix child failed");
}

/// Remove a seeded dataset once its run is complete (the kse_m7 lesson —
/// these used to accumulate in the OS temp dir).
fn cleanup(path: &Path) {
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

// ---- report generation ----------------------------------------------------

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

fn machine() -> String {
    format!(
        "{}/{}; {} logical cores; {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "processor NOT_REPORTED".into()),
    )
}

fn verdict(p50: u64, gate_ns: u128) -> &'static str {
    if (p50 as u128) <= gate_ns {
        "PASS"
    } else {
        "FAIL"
    }
}

fn find<'a>(rows: &'a [Row], label: &str) -> &'a Row {
    rows.iter()
        .find(|r| r.label == label)
        .unwrap_or_else(|| panic!("missing row {label}"))
}

fn gate_line(rows: &[Row], redb: &[Row], name: &str, label: &str, gate_ns: u128) -> String {
    let r = find(rows, label);
    let b = redb.iter().find(|x| x.label == label);
    format!(
        "| {name} | {label} | {:.1} µs | {} | {:.0} µs | {} |\n",
        r.p50 as f64 / 1000.0,
        b.map(|x| format!("{:.1} µs", x.p50 as f64 / 1000.0))
            .unwrap_or_else(|| "—".into()),
        gate_ns as f64 / 1000.0,
        verdict(r.p50, gate_ns),
    )
}

fn matrix_table(rows: &[Row]) -> String {
    let mut s = String::from(
        "| workload · state | ops | ops/s | P50 µs | P95 µs | P99 µs | bytes read | blocks | segs/op | hits | misses |\n\
         |---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    for r in rows {
        let has_cells =
            r.cells.blocks > 0 || r.cells.hits > 0 || r.cells.segs > 0 || r.cells.misses > 0;
        s.push_str(&format!(
            "| {} | {} | {:.0} | {:.1} | {:.1} | {:.1} | {} | {} | {} | {} | {} |\n",
            r.label,
            r.ops,
            r.ops as f64 / (r.wall_ms / 1000.0),
            r.p50 as f64 / 1000.0,
            r.p95 as f64 / 1000.0,
            r.p99 as f64 / 1000.0,
            if has_cells {
                fmt_bytes(r.cells.bytes)
            } else {
                "—".into()
            },
            if has_cells {
                r.cells.blocks.to_string()
            } else {
                "—".into()
            },
            if has_cells {
                format!("{:.2}", r.cells.segs as f64 / r.ops as f64)
            } else {
                "—".into()
            },
            if has_cells {
                r.cells.hits.to_string()
            } else {
                "—".into()
            },
            if has_cells {
                r.cells.misses.to_string()
            } else {
                "—".into()
            },
        ));
    }
    s
}

struct Section {
    sz: Size,
    built: Built,
    rows: Vec<Row>,
    redb: Vec<Row>,
    rss: Option<u64>,
    segs: usize,
}

fn run_dataset(dir: &Path, sz: Size) -> Section {
    let built = build_dataset(dir, sz);
    verify(dir, &built, sz);
    let rows = matrix(dir, &built, sz);
    let segs = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("SEGMENT-"))
        .count();
    let rss = measure_rss(sz);
    let redb_dir = tmp(&format!("m14-redb-{}", sz.label));
    let redb = redb_rows(&redb_dir, &built, sz);
    cleanup(&redb_dir);
    cleanup(dir);
    Section {
        sz,
        built,
        rows,
        redb,
        rss,
        segs,
    }
}

fn write_report(sections: &[Section], m: Mode) {
    let mode_label = if m == Mode::ML {
        "2 (DS-PERF-M + DS-PERF-L)"
    } else {
        "1 (DS-PERF-M)"
    };
    let mut s = String::new();
    let date = run_date();
    s.push_str(&format!(
        "# Scale Certification — SE2-M14\n\n\
         Generated only when `SE2M14_NIGHTLY=1|2` (strict opt-in — any other\n\
         value panics). Perf numbers are report cells, never asserts; the pins\n\
         are answer correctness and cache state.\n\n\
         - Test: `scale_certification`\n\
         - Date: {date} · Build mode: {}\n\
         - Machine: {}\n\
         - Mode: SE2M14_NIGHTLY={mode_label}\n\n",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        machine(),
    ));
    s.push_str("## Datasets\n\n");
    s.push_str(
        "| dataset | KOs | versions/KO | rows | batches | seed wall | fsyncs | disk | RSS peak | segments |\n\
         |---|---|---|---|---|---|---|---|---|---|\n",
    );
    for sec in sections {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {:.0} ms | {} | {} | {} | {} |\n",
            sec.sz.label,
            sec.sz.n,
            sec.sz.versions,
            sec.built.rows,
            sec.built.batches,
            sec.built.wall_ms,
            sec.built.fsyncs,
            fmt_bytes(sec.built.disk),
            sec.rss
                .map(fmt_bytes)
                .unwrap_or_else(|| "NOT_SAMPLED".into()),
            sec.segs,
        ));
    }
    let sec = &sections[0];
    s.push_str("\n## QA M8 gates — DS-PERF-M\n\n");
    s.push_str(
        "Gate verdicts are machine-relative (machine spec above) — the QA doc's\n\
         rule. Verdict on the v2 row; redb is the parity reference, no verdict.\n\n\
         | gate | row | v2 P50 | redb P50 | threshold | verdict |\n\
         |---|---|---|---|---|---|\n",
    );
    s.push_str(&gate_line(
        &sec.rows,
        &sec.redb,
        "cold point",
        "head get · cold",
        COLD_GATE_NS,
    ));
    s.push_str(&gate_line(
        &sec.rows,
        &sec.redb,
        "warm point",
        "head get · warm",
        WARM_GATE_NS,
    ));
    s.push_str(&gate_line(
        &sec.rows,
        &sec.redb,
        "hot head",
        "head get · hot",
        HOT_HEAD_GATE_NS,
    ));
    for (f, gate) in [
        (sec.built.fans[0], FANOUT_GATES_NS[0]),
        (sec.built.fans[1], FANOUT_GATES_NS[1]),
        (sec.built.fans[2], FANOUT_GATES_NS[2]),
    ] {
        s.push_str(&gate_line(
            &sec.rows,
            &sec.redb,
            &format!("fanout F={f}"),
            &format!("fanout F={f} · warm"),
            gate,
        ));
    }
    s.push_str(&gate_line(
        &sec.rows,
        &sec.redb,
        "hot context",
        "context · hot",
        HOT_CONTEXT_GATE_NS,
    ));
    s.push_str(
        "| group commit beats Sync where batching is possible | — | PASS (cited) | — | — | SE2-M13 matrix — artifacts/storage-engine-v2/group-commit.md: 8 writers × 25 wait=0 → 37 fsyncs, 0.23 ms/batch vs Sync 0.80 ms/batch |\n",
    );
    for sec in sections {
        s.push_str(&format!("\n## Matrix — {}\n\n", sec.sz.label));
        s.push_str(&matrix_table(&sec.rows));
        s.push_str("\nredb parity rows (— = redb exposes no block stats):\n\n");
        s.push_str(&matrix_table(&sec.redb));
    }
    s.push_str("\n## Adoption matrix re-run\n\n");
    s.push_str(
        "`v2_m7_workloads` child with `V2ADOPT_NIGHTLY=1` — the same harness;\n\
         `workloads.md` regenerated this run, child exit 0 (asserted). The §26\n\
         verdict stays per `adoption-decision.md`; the ≤2×-of-v1 bound stays\n\
         out of scope (RAM-vs-disk physics, priced in by the 2026-09-01\n\
         verdict) — the comparison that matters is the redb parity above.\n",
    );
    s.push_str("\n## Honest metric mapping\n\n");
    s.push_str(
        "- cold = the block-cache-miss path: a separate Db open with\n\
           cache_bytes=0 (the detached cache is pinned by assert) — every get\n\
           reads its block from disk. The OS page cache is NOT flushed (needs\n\
           admin tooling on Windows) — the same caveat the adoption matrix's\n\
           cold rows carry; first touches after the seed still benefit from it.\n\
         - warm = an uncounted pre-warm of the same ops, then the timed pass.\n\
           Point rows (head, version, fanout F ≤ 100) carry the exact pin\n\
           (asserted per row): zero cache misses and zero block reads during\n\
           the timed pass — the sample (400 evenly spaced KOs ≈ 6.4 MiB of\n\
           blocks) fits the 8 MiB default cache. Scan rows (history, type,\n\
           context) carry no cache pin: SE2-M12's k-way scan cursors walk one\n\
           block per overlapping segment (~5 at M scale), so a scan working\n\
           set is ~5× the cache by construction — warm there means the second\n\
           (page-cache) pass, and the hits/misses cells report the thrash\n\
           honestly.\n\
         - hot = repeated same-key reads; pins (asserted): cache hits ≥ lookups\n\
           and zero block reads during the run.\n\
         - fanout F=10/100 warm rows carry the warm pin; F=1000 does not — its\n\
           ~16 MiB head working set exceeds the 8 MiB default cache, so the\n\
           honest cells show the thrash (cache sizing for big fan-outs is an\n\
           M14 finding, not a hidden knob).\n\
         - W8 mixed = one warm row; its write leg lands in the active memtable\n\
           and runs last (nothing after it reads the mutated heads).\n\
         - redb parity = first pass / second pass on the same open (redb has no\n\
           block-cache knob).\n\
         - RSS = Windows WorkingSet64 poll on a loader child re-seeding the same\n\
           dataset (peak is a lower bound); NOT_SAMPLED elsewhere.\n\
         - CPU = seed wall, single-threaded (wall ≈ CPU); fsync count = the\n\
           seed's (Sync durability, one fsync per batch); disk = dataset dir at\n\
           seed end.\n\
         - group commit gate = cited, not re-measured (SE2-M13).\n\
         - gate verdicts on DS-PERF-M; DS-PERF-L cells are the scale check.\n\
         - regression fixed by this milestone's nightly: v2 restart points on\n\
           equal-key runs (the kernel's RMW restatements accumulate (key, seq)\n\
           versions in the memtable, and one flush publishes the whole run).\n\
           The writer now skips restarts on equal keys; intervals over a\n\
           multi-version run exceed RESTART_INTERVAL, so a head lookup inside\n\
           a long run decodes it (the honest version-lookup cost — hot-head\n\
           rows report it).\n",
    );
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("artifacts")
        .join("storage-engine-v2");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("scale-certification.md"), s).unwrap();
}

/// SE2-M17/M18 — tier-depth read probe: the same row shapes the QA matrix
/// runs, on the seeded database in its tiered steady state (up to ~17
/// L0 + L1 at L, vs 2 under count-only). Cold rows detach the cache like
/// the matrix (cache_bytes = 0); warm/hot rows carry the same pins. The
/// cells are report-only evidence against the QA read gates (cold point
/// <= 100 us, warm <= 50 us, hot head <= 20 us, fanout F=10/100/1000
/// <= 1/10/50 ms, hot context <= 100 us) — printed live to the
/// stderr handle (libtest captures eprintln on the test thread).
fn tier_read_probe(dir: &Path, b: &Built, sz: Size) -> Vec<Row> {
    let mut rows = Vec::new();
    let sample = sample_indices(sz);
    let targets: Vec<Target> = sample.iter().map(|&i| (b.koids[i], i)).collect();
    let absent = head_key(&[0xFF; 16]); // written by nobody: the pure fan-out walk

    let mut cfg = Config::new(dir.to_path_buf());
    cfg.cache_bytes = 0;
    let db = Db::open(cfg).unwrap();
    {
        let mut it = targets.iter();
        let mut r = timed(&db, targets.len(), |db| {
            let &(koid, i) = it.next().unwrap();
            assert_eq!(
                db.get(&head_key(&koid)).unwrap(),
                Some(head_value(i)),
                "tier cold head diverged"
            );
        });
        r.label = "head get · cold · tier".into();
        rows.push(r);
    }
    {
        let mut it = targets.iter();
        let mut r = timed(&db, targets.len(), |db| {
            let &(koid, i) = it.next().unwrap();
            assert_eq!(
                db.get(&obj_key(&koid, 1)).unwrap(),
                Some(version_value(i, 1)),
                "tier cold version diverged"
            );
        });
        r.label = "version get · cold · tier".into();
        rows.push(r);
    }
    {
        let mut r = timed(&db, targets.len(), |db| {
            assert_eq!(db.get(&absent).unwrap(), None, "tier absent must miss");
        });
        r.label = "absent get · cold · tier".into();
        rows.push(r);
    }
    {
        let mut t = 0usize;
        let mut r = timed(&db, TYPE_SCANS, |db| {
            let p = format!("type/m7t_{t}/");
            t += 1;
            assert_eq!(
                db.scan(p.as_bytes()).unwrap().len(),
                sz.n / N_TYPES,
                "tier type scan drifted"
            );
        });
        r.label = "type scan · cold · tier".into();
        rows.push(r);
    }
    for h in 0..3 {
        // report cells: the cold fan-out walk at tier depth
        let f = b.fans[h];
        let rel = if h == 0 { "links" } else { "fan" };
        let src = b.hubs[h];
        let ops = (2000 / f).max(4);
        let mut r = timed(&db, ops, |db| traverse(db, &src, rel, f));
        r.label = format!("fanout F={f} · cold · tier");
        rows.push(r);
    }
    drop(db);

    // warm/hot: default cache, one open — all reads, no mutation
    let db = Db::open(Config::new(dir.to_path_buf())).unwrap();
    {
        let mut r = warm_pinned(
            &db,
            |db| {
                for &(koid, _) in &targets {
                    let _ = db.get(&head_key(&koid)).unwrap();
                }
            },
            targets.len(),
            {
                let mut it = targets.iter();
                move |db| {
                    let &(koid, i) = it.next().unwrap();
                    assert_eq!(
                        db.get(&head_key(&koid)).unwrap(),
                        Some(head_value(i)),
                        "tier warm head diverged"
                    );
                }
            },
        );
        r.label = "head get · warm · tier".into();
        rows.push(r);
    }
    {
        let (koid, i) = targets[0];
        let key = head_key(&koid);
        let mut r = warm_pinned(
            &db,
            |db| {
                let _ = db.get(&key).unwrap();
            },
            100_000,
            |db| {
                assert_eq!(
                    db.get(&key).unwrap(),
                    Some(head_value(i)),
                    "tier hot head diverged"
                );
            },
        );
        r.label = "head get · hot · tier".into();
        rows.push(r);
    }
    for h in 0..3 {
        // the gated rows: warm fan-out at tier depth, same pin shape as
        // the matrix (F <= 100 zero-miss pin; F=1000's working set
        // exceeds the 8 MiB cache — thrash reported, not hidden)
        let f = b.fans[h];
        let rel = if h == 0 { "links" } else { "fan" };
        let src = b.hubs[h];
        let ops = (2000 / f).max(4);
        let mut r = if f <= 100 {
            warm_pinned(
                &db,
                |db| traverse(db, &src, rel, f),
                ops,
                move |db| traverse(db, &src, rel, f),
            )
        } else {
            traverse(&db, &src, rel, f); // pre-warm, uncounted
            timed(&db, ops, move |db| traverse(db, &src, rel, f))
        };
        r.label = format!("fanout F={f} · warm · tier");
        rows.push(r);
    }
    {
        // the hot-context gate row: pre-warm one KO's ring + history,
        // then repeat — cache-served, no block reads (the same pins as
        // the matrix's context · hot)
        let koid = b.koids[0];
        context(&db, &koid, sz); // pre-warm, uncounted
        let s0 = db.read_path_stats();
        let c0 = db.cache_stats();
        let t0 = Instant::now();
        let mut lats = Vec::with_capacity(HOT_CTX_LOOKUPS);
        for _ in 0..HOT_CTX_LOOKUPS {
            let s = Instant::now();
            context(&db, &koid, sz);
            lats.push(s.elapsed().as_nanos());
        }
        let c1 = db.cache_stats();
        let s1 = db.read_path_stats();
        assert_eq!(
            s1.blocks_read, s0.blocks_read,
            "a hot context reads no blocks"
        );
        assert!(
            c1.hits - c0.hits >= HOT_CTX_LOOKUPS as u64,
            "a hot context must be cache-served"
        );
        rows.push(row_from(
            "context · hot · tier".into(),
            lats,
            t0.elapsed().as_secs_f64() * 1000.0,
            s0,
            s1,
            c0,
            c1,
        ));
    }
    rows
}

// ---- the tests ------------------------------------------------------------

#[test]
fn ds_perf_loader() {
    let Ok(label) = std::env::var(LOADER_ENV) else {
        return; // parent run: nothing to do
    };
    let sz = match label.as_str() {
        "DS-PERF-L" => Size {
            n: 1_000_000,
            versions: 10,
            label: "DS-PERF-L",
        },
        // SE2-M15 — the mid size that crosses the L0 trigger (≈ 3.2M rows,
        // ~7 flushes): one merge of 4 L0s mid-seed. Same per-row shape as
        // L, 10× fewer KOs — the decomposition probe for the merge peak.
        "DS-PERF-S" => Size {
            n: 100_000,
            versions: 10,
            label: "DS-PERF-S",
        },
        _ => Size {
            n: 100_000,
            versions: 5,
            label: "DS-PERF-M",
        },
    };
    let p = tmp(&format!("m14-loader-{label}"));
    let built = build_dataset(&p, sz);
    // SE2-M17 — the tier-depth read probe: strict opt-in, report cells
    // printed live to the stderr handle (libtest captures eprintln on the
    // test thread, so the loader's own .err log carries the probe rows).
    if std::env::var_os("SE2M17_READS").is_some() {
        use std::io::Write;
        let mut out = std::io::stderr();
        let _ = writeln!(
            out,
            "tier read probe (SE2M17_READS) — {} ({} rows, seed wall {:.1}s)",
            sz.label,
            built.rows,
            built.wall_ms / 1000.0
        );
        for r in tier_read_probe(&p, &built, sz) {
            let _ = writeln!(
                out,
                "{:24} ops={:7} wall={:9.1}ms p50={:6} p95={:6} p99={:6} | blocks={} bytes={} segs={:2} hits={} misses={}",
                r.label,
                r.ops,
                r.wall_ms,
                r.p50,
                r.p95,
                r.p99,
                r.cells.blocks,
                r.cells.bytes,
                r.cells.segs,
                r.cells.hits,
                r.cells.misses
            );
        }
        let _ = out.flush();
    }
    cleanup(&p);
}

#[test]
fn ds_perf_smoke() {
    // The harness at 400 KOs — the same builder, pins, and rows the nightly
    // runs at 100K/1M. Nightly numbers are report cells; the pins fire here.
    let sz = Size {
        n: 400,
        versions: 5,
        label: "SMOKE",
    };
    let d = dir("m14-smoke");
    let built = build_dataset(&d, sz);
    verify(&d, &built, sz);
    let rows = matrix(&d, &built, sz);
    for r in &rows {
        assert!(r.ops > 0, "{} produced no ops", r.label);
    }
    cleanup(&d);
}

#[test]
fn scale_certification() {
    let m = mode();
    if m == Mode::Off {
        eprintln!("SKIPPED (set SE2M14_NIGHTLY=1 for DS-PERF-M, =2 for M + DS-PERF-L)");
        return;
    }
    rerun_adoption_matrix();
    let mut sections = Vec::new();
    for sz in datasets(m) {
        let d = dir(&format!("m14-{}", sz.label));
        sections.push(run_dataset(&d, sz));
    }
    write_report(&sections, m);
}
