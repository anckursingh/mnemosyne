//! SE2-M12 — the k-way merged scan iterator (QA M4 + M5): correctness vs a
//! reference oracle over 50 000 randomized states, prefix-isolation
//! evidence (unrelated rows decoded ≈ 0), and the W4/W5 scan-amplification
//! report (`SE2M12_NIGHTLY=1` strict opt-in). The V2-Adopt `db_scan` suite
//! remains the byte-exact scan regression.

mod common;

use aikoql_storage_v2::db::{Config, Db};
use common::dir;
use std::collections::BTreeMap;
use std::path::Path;

/// The reference model of the scan contract: newest layer wins per key,
/// a tombstone suppresses the key, output is key-ascending, restricted to
/// [prefix, prefix+∞).
#[derive(Default)]
struct Oracle {
    heads: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
}

impl Oracle {
    fn put(&mut self, k: &[u8], v: &[u8]) {
        self.heads.insert(k.to_vec(), Some(v.to_vec()));
    }
    fn delete(&mut self, k: &[u8]) {
        self.heads.insert(k.to_vec(), None);
    }
    fn scan(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.heads
            .range(prefix.to_vec()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .filter_map(|(k, v)| v.clone().map(|v| (k.clone(), v)))
            .collect()
    }
}

/// xorshift64 — deterministic across runs.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// The kernel's relationship out-edge key: `relo/` + 16-byte src koid +
/// `/` + rel type + `/` + 16-byte dst koid (semantic_equivalence shape).
fn rel_out_key(src: &[u8; 16], rel: &str, dst: &[u8; 16]) -> Vec<u8> {
    let mut k = Vec::with_capacity(5 + 16 + 1 + rel.len() + 1 + 16);
    k.extend_from_slice(b"relo/");
    k.extend_from_slice(src);
    k.push(b'/');
    k.extend_from_slice(rel.as_bytes());
    k.push(b'/');
    k.extend_from_slice(dst);
    k
}

/// 50 000 randomized steps — puts, deletes, deterministic flushes and
/// compactions over a versioned key space, every scan byte-exact vs the
/// oracle (QA TC-PERF-0501). The merged iterator is the regression
/// surface: newest-layer-wins heads, tombstone suppression, prefix
/// bounds, ascending order, across churning L0/L1 layer stacks.
#[test]
fn merged_iterator_correctness() {
    const STEPS: usize = 50_000;
    const KEYS: usize = 40;
    const FLUSH_EVERY: usize = 200;
    const COMPACT_EVERY: usize = 800;

    let mut cfg = Config::new(dir("merged-correctness"));
    cfg.memtable_bytes = usize::MAX; // layer churn is deterministic below
    let db = Db::open(cfg).unwrap();
    let mut oracle = Oracle::default();
    let mut rng = Rng(0x51ed_2701_4adf_05e5);
    let prefixes: [&[u8]; 3] = [b"key-0", b"key-1", b"key-2"];

    for step in 0..STEPS {
        let r = rng.next();
        let key = format!("key-{:02}-{:02}", r as usize % KEYS, (r >> 8) as usize % 7).into_bytes();
        let value = format!("v-{step}").into_bytes();
        let tombstone = r % 100 < 14;
        if tombstone {
            db.delete(&key).unwrap();
            oracle.delete(&key);
        } else {
            db.put(&key, &value).unwrap();
            oracle.put(&key, &value);
        }
        if step > 0 && step % FLUSH_EVERY == 0 {
            db.flush().unwrap();
        }
        if step > 0 && step % COMPACT_EVERY == 0 {
            db.compact().unwrap();
        }
        // A full-merge check every 100 steps; prefix-shaped otherwise.
        let prefix = if step % 100 == 0 {
            b"".as_slice()
        } else {
            prefixes[(r >> 16) as usize % prefixes.len()]
        };
        let got = db.scan(prefix).unwrap();
        let want = oracle.scan(prefix);
        assert_eq!(got, want, "step {step}: scan({prefix:?}) diverged");
    }
}

/// Relationship-shaped: one segment, A's 1000 relos sandwiched between two
/// other entities' rows (~350 before, ~200 after, so A's range lands
/// mid-block at both ends). A scan of A's prefix decodes A's rows only —
/// the restart seek skips the pre-start rows, the end bound stops before
/// the post-end rows (QA TC-PERF-0402). The whole-block path decodes
/// every entry in the touched blocks, so this is RED on it.
#[test]
fn prefix_isolation() {
    const A_ROWS: usize = 1000;
    const PRE_ROWS: usize = 350;
    const POST_ROWS: usize = 200;

    let a = [0xAAu8; 16];
    let before_entity = [0x00u8; 16];
    let after_entity = [0xFFu8; 16];

    let mut cfg = Config::new(dir("prefix-isolation"));
    cfg.memtable_bytes = usize::MAX;
    let db = Db::open(cfg).unwrap();
    let put = |src: &[u8; 16], i: usize| {
        let mut dst = [0u8; 16];
        dst[..2].copy_from_slice(&(i as u16).to_be_bytes());
        db.put(&rel_out_key(src, "related", &dst), b"1").unwrap();
    };
    for i in 0..PRE_ROWS {
        put(&before_entity, i);
    }
    for i in 0..A_ROWS {
        put(&a, i);
    }
    for i in 0..POST_ROWS {
        put(&after_entity, i);
    }
    db.flush().unwrap();

    let mut prefix = Vec::with_capacity(5 + 16);
    prefix.extend_from_slice(b"relo/");
    prefix.extend_from_slice(&a);

    let before = db.read_path_stats();
    let rows = db.scan(&prefix).unwrap();
    let after = db.read_path_stats();

    assert_eq!(rows.len(), A_ROWS);
    for (i, (k, v)) in rows.iter().enumerate() {
        assert!(k.starts_with(&prefix), "row {i} escapes the prefix");
        assert_eq!(v, b"1");
        if i > 0 {
            assert!(rows[i - 1].0 < *k, "output not ascending at {i}");
        }
    }
    let decoded = after.entries_decoded - before.entries_decoded;
    // Both sides of QA TC-PERF-0402: the scan's decode work is visible and
    // counts A's rows, and unrelated rows are ≈ 0 (the restart seek lands
    // at most one interval — ≤16 entries — before the prefix, the end
    // bound stops within one entry past it).
    assert!(
        decoded >= A_ROWS as u64,
        "the scan decoded {decoded} entries — less than A's {A_ROWS} rows: decode work is invisible"
    );
    assert!(
        decoded <= (A_ROWS + 64) as u64,
        "the scan decoded {decoded} entries for {A_ROWS} rows — unrelated rows were decoded"
    );
}

const NIGHTLY: &str = "SE2M12_NIGHTLY";

fn nightly_on() -> bool {
    match std::env::var(NIGHTLY) {
        Err(_) => false,
        Ok(v) if v == "1" => true,
        Ok(v) => panic!("{NIGHTLY} must be unset or \"1\", got {v:?} (strict opt-in)"),
    }
}

/// One measurement: scan `prefix` twice (engine-cold then engine-warm),
/// answer pins byte-exact, per-scan report cells for I/O and decode
/// amplification (the stats delta of each scan alone).
fn measure(db: &Db, prefix: &[u8], rows_want: usize) -> (String, String) {
    let before = db.read_path_stats();
    let t0 = std::time::Instant::now();
    let cold = db.scan(prefix).unwrap();
    let cold_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let mid = db.read_path_stats();
    let t1 = std::time::Instant::now();
    let warm = db.scan(prefix).unwrap();
    let warm_ms = t1.elapsed().as_secs_f64() * 1000.0;
    let after = db.read_path_stats();

    for (i, (k, v)) in cold.iter().enumerate() {
        assert!(k.starts_with(prefix), "row {i} escapes the prefix");
        assert_eq!(*v, warm[i].1);
        if i > 0 {
            assert!(cold[i - 1].0 < *k, "output not ascending at {i}");
        }
    }
    assert_eq!(cold.len(), rows_want);
    assert_eq!(warm, cold, "warm scan diverges from cold");

    let bytes_returned: u64 = cold.iter().map(|(k, v)| (k.len() + v.len()) as u64).sum();
    let cold_cells = (
        mid.entries_decoded - before.entries_decoded,
        mid.blocks_read - before.blocks_read,
        mid.bytes_read - before.bytes_read,
    );
    let warm_cells = (
        after.entries_decoded - mid.entries_decoded,
        after.blocks_read - mid.blocks_read,
        after.bytes_read - mid.bytes_read,
    );
    (
        format!(
            "  rows {rows_want}, bytes_returned {bytes_returned}\n\
             - cold: decoded {}, blocks {}, bytes_read {}, wall {cold_ms:.1} ms\n\
             - warm: decoded {}, blocks {}, bytes_read {}, wall {warm_ms:.1} ms",
            cold_cells.0, cold_cells.1, cold_cells.2, warm_cells.0, warm_cells.1, warm_cells.2,
        ),
        format!(
            "  per-scan decode amp {:.2}x, cold I/O amp {:.2}x",
            cold_cells.0 as f64 / rows_want as f64,
            cold_cells.2 as f64 / bytes_returned.max(1) as f64,
        ),
    )
}

/// W4- and W5-shaped scan amplification, `SE2M12_NIGHTLY=1` strict opt-in
/// (M11 pattern): report cells only, answer pins as asserts, artifact at
/// artifacts/storage-engine-v2/scan-amplification.md.
#[test]
fn scan_amplification_report() {
    if !nightly_on() {
        eprintln!("SKIPPED (set SE2M12_NIGHTLY=1 to run the scan amplification report)");
        return;
    }
    let db = Db::open(Config::new(dir("scan-amp"))).unwrap();
    db.put(b"seed", b"seed").unwrap();

    // W4 — one entity's out-edges among 10 entities × 500 relos: rows are
    // entity-contiguous, the scanned entity spans ~2-3 blocks.
    let w4 = Db::open(Config::new(dir("scan-amp-w4"))).unwrap();
    for entity in 0..10u8 {
        let src = [entity; 16];
        for i in 0..500 {
            let mut dst = [0u8; 16];
            dst[..2].copy_from_slice(&(i as u16).to_be_bytes());
            w4.put(&rel_out_key(&src, "related", &dst), b"w4").unwrap();
        }
    }
    w4.flush().unwrap();
    let w4_prefix = {
        let mut p = Vec::with_capacity(5 + 16);
        p.extend_from_slice(b"relo/");
        p.extend_from_slice(&[5u8; 16]);
        p
    };

    // W5 — one type's rows among 4 types × 500 koids. The kernel's
    // type/<name>/<koid> shape sorts types contiguous, so the W5 question
    // is whether a type scan touches only its own rows' blocks.
    let w5 = Db::open(Config::new(dir("scan-amp-w5"))).unwrap();
    for i in 0..500u32 {
        let mut koid = [0u8; 16];
        koid[..4].copy_from_slice(&i.to_be_bytes());
        for ty in ["Note", "Task", "Link", "Idea"] {
            let mut k = Vec::with_capacity(5 + ty.len() + 1 + 16);
            k.extend_from_slice(b"type/");
            k.extend_from_slice(ty.as_bytes());
            k.push(b'/');
            k.extend_from_slice(&koid);
            w5.put(&k, b"w5").unwrap();
        }
    }
    w5.flush().unwrap();

    let machine = format!(
        "{}/{}; {} logical cores; {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "processor NOT_REPORTED".into()),
    );
    let (w4_cold, w4_warm) = measure(&w4, &w4_prefix, 500);
    let (w5_cold, w5_warm) = measure(&w5, b"type/Note/", 500);

    let report = format!(
        "# Scan Amplification Report — SE2-M12\n\n\
         Generated only when `SE2M12_NIGHTLY=1` (strict opt-in). Perf numbers are\n\
         report cells, never asserts — the report regenerates only with the env set.\n\n\
         - Test: `scan_amplification_report`\n\
         - Build mode: {build}\n\
         - Machine: {machine}\n\
         - Scanner: the SE2-M12 k-way merged iterator — per-segment lazy cursors\n\
           decode one entry at a time from cache-served raw blocks (restart-table\n\
           seek), memtable heads merge in layer order (newest wins, tombstones\n\
           suppress). No whole-block Vec, no BTreeMap of every prefix key.\n\
         - Answers pinned byte-exact on the cold scan (prefix, ascending, warm ==\n\
           cold).\n\n\
         ## W4 shape — entity out-edges (relo/<src>/..., 10 entities × 500 rows)\n\
         {w4_cold}\n\
         {w4_warm}\n\n\
         ## W5 shape — type rows (type/<name>/<koid>, 4 types × 500 koids)\n\
         {w5_cold}\n\
         {w5_warm}\n",
        build = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
    );
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("artifacts")
        .join("storage-engine-v2");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("scan-amplification.md"), report).unwrap();
}
