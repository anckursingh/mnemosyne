//! SE2-M7 — bounded block cache + bloom wiring REDs. A get() must never
//! change its answer because of the cache (neutrality), the cache must
//! hold at most `cache_bytes` decoded entry bytes (bounded) with live
//! metrics (hit/miss/evictions), and the segment bloom (built in SE2-M1)
//! must never miss an inserted key — the read path may skip a segment only
//! when the bloom proves the key absent (false positives cost a wasted
//! probe, false negatives are forbidden).

mod common;

use aikoql_storage_v2::db::{Config, Db};
use aikoql_storage_v2::identity::ReplicaId;
use aikoql_storage_v2::segment::{SegmentEntry, SegmentReader, SegmentWriter};
use common::dir;
use std::path::Path;

fn cfg_with_cache(dir: std::path::PathBuf, cache_bytes: usize) -> Config {
    let mut c = Config::new(dir);
    c.cache_bytes = cache_bytes;
    c
}

/// xorshift64 — deterministic across the two cache configs.
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

#[test]
fn cache_is_bounded_and_evicts_under_load() {
    let d = dir("cache-bounded");
    let mut cfg = cfg_with_cache(d.clone(), 4096); // small cap: churn forced
    cfg.block_target = 1024; // many small blocks, ~8 entries each
    let db = Db::open(cfg).unwrap();

    let db = db;
    const KEYS: u64 = 40;
    for i in 0..KEYS {
        db.put(&format!("key-{i:03}").into_bytes(), &[b'x'; 128])
            .unwrap();
    }
    db.flush().unwrap(); // everything reads through segments now

    // Three full rounds: round 1 all misses, rounds 2-3 hit whatever the
    // 4 KiB cap kept (each block ~1.1 KiB decoded → ~3-4 blocks cached).
    for _ in 0..3 {
        for i in 0..KEYS {
            assert_eq!(
                db.get(&format!("key-{i:03}").into_bytes()).unwrap(),
                Some(vec![b'x'; 128]),
                "cache must never change an answer"
            );
        }
    }

    let stats = db.cache_stats();
    assert_eq!(
        stats.hits + stats.misses,
        3 * KEYS,
        "every get is exactly one cache lookup (single segment, one block per key)"
    );
    assert!(stats.hits > 0, "re-reads must hit the cache");
    assert!(
        stats.evictions > 0,
        "a 4 KiB cap over ~45 KiB of distinct blocks must evict"
    );
    assert!(
        stats.bytes <= 4096,
        "cache holds {} bytes — the cap is a hard bound",
        stats.bytes
    );
}

#[test]
fn cache_never_changes_answers() {
    const WORKLOAD: usize = 300;
    const KEYS: u64 = 500;

    // The same seeded workload under two cache configs: every get answer
    // must be byte-identical (None included). Interleaved flushes force
    // the reads to span many segment blocks.
    let run = |cache_bytes: usize| -> Vec<Option<Vec<u8>>> {
        let mut cfg = cfg_with_cache(dir("cache-neutral"), cache_bytes);
        cfg.block_target = 2048;
        let db = Db::open(cfg).unwrap();
        let mut rng = Rng(0x5eed);
        let mut answers = Vec::new();
        for step in 0..WORKLOAD {
            if step % 100 == 0 {
                db.flush().unwrap();
            }
            let r = rng.next();
            let key = format!("key-{:03}", r % KEYS).into_bytes();
            match (r / KEYS) % 4 {
                0 => {
                    let v = format!("v{step}").into_bytes();
                    db.put(&key, &v).unwrap();
                    answers.push(None); // write: no get
                }
                1 => {
                    db.delete(&key).unwrap();
                    answers.push(None); // write: no get
                }
                _ => answers.push(db.get(&key).unwrap()),
            }
        }
        answers
    };

    let off = run(0);
    let on = run(64 * 1024);
    assert_eq!(off.len(), on.len());
    assert_eq!(off, on, "cache on vs off must answer byte-identically");
}

#[test]
fn bloom_never_misses_and_false_positive_rate_is_sane() {
    let path = dir("cache-bloom").join("bloom.seg");
    const N: u64 = 1000;
    let mut rng = Rng(0xb100);
    let mut writer = SegmentWriter::new(4096);
    for i in 0..N {
        let key = format!("k{:08}-{:016x}", i, rng.next()).into_bytes();
        writer.push(SegmentEntry {
            key,
            value: vec![b'v'; 32],
            seq: i,
            flags: 1, // FLAG_PUT
            replica_id: ReplicaId(0),
        });
    }
    writer.publish(&path).unwrap();
    let reader = SegmentReader::open(&path).unwrap();

    // False negatives are forbidden: every inserted key is reported.
    let mut rng = Rng(0xb100);
    for i in 0..N {
        let key = format!("k{:08}-{:016x}", i, rng.next()).into_bytes();
        assert!(
            reader.bloom_may_contain(&key),
            "bloom missed inserted key {i} — a false negative"
        );
    }

    // Random non-inserted keys: the bloom may say yes (a wasted probe),
    // but a sane bloom says no most of the time — m = 10n, k = 4 sits
    // near 1%; anything above 25% means the filter is not a filter.
    const PROBES: u64 = 10_000;
    let mut rng = Rng(0xdeed);
    let mut false_positives = 0u64;
    for _ in 0..PROBES {
        let key = format!("absent-{:016x}", rng.next()).into_bytes();
        if reader.bloom_may_contain(&key) {
            false_positives += 1;
        }
    }
    let fpr = false_positives as f64 / PROBES as f64;
    assert!(
        fpr < 0.25,
        "false-positive rate {fpr:.3} — the bloom is degenerate"
    );
}

// ---------------------------------------------------------------------------
// SE2M7_NIGHTLY — warm vs cold random reads. Strict opt-in: unset skips,
// any value other than "1" panics. Perf numbers are report cells, never
// asserts; the report regenerates only with the env set.
// ---------------------------------------------------------------------------

const GATE: &str = "SE2M7_NIGHTLY";

fn nightly_on() -> bool {
    match std::env::var(GATE) {
        Err(_) => false,
        Ok(v) if v == "1" => true,
        Ok(v) => panic!("{GATE} must be unset or \"1\", got {v:?} (strict opt-in)"),
    }
}

#[test]
fn warm_block_cache_speedup() {
    if !nightly_on() {
        eprintln!("SKIPPED (set SE2M7_NIGHTLY=1 to run the warm/cold matrix)");
        return;
    }
    const N: u64 = 2000;
    let keys: Vec<Vec<u8>> = (0..N).map(|i| format!("key-{i:06}").into_bytes()).collect();
    let mut rng = Rng(0xc0ffee);

    // Cold: cache off — every get re-reads its block from disk.
    let d = dir("cache-perf-cold");
    let mut cfg = cfg_with_cache(d, 0);
    cfg.block_target = 64 * 1024;
    let db = Db::open(cfg).unwrap();
    for k in &keys {
        db.put(k, &[b'z'; 200]).unwrap();
    }
    db.flush().unwrap();
    let order: Vec<u64> = (0..N).map(|_| rng.next() % N).collect();
    let t0 = std::time::Instant::now();
    for &i in &order {
        assert_eq!(db.get(&keys[i as usize]).unwrap(), Some(vec![b'z'; 200]));
    }
    let cold_ms = t0.elapsed().as_millis();
    drop(db);

    // Warm: cache on — first pass fills, second pass is the measurement.
    let d = dir("cache-perf-warm");
    let mut cfg = cfg_with_cache(d, 64 * 1024 * 1024);
    cfg.block_target = 64 * 1024;
    let db = Db::open(cfg).unwrap();
    for k in &keys {
        db.put(k, &[b'z'; 200]).unwrap();
    }
    db.flush().unwrap();
    for &i in &order {
        db.get(&keys[i as usize]).unwrap();
    }
    let t0 = std::time::Instant::now();
    for &i in &order {
        assert_eq!(db.get(&keys[i as usize]).unwrap(), Some(vec![b'z'; 200]));
    }
    let warm_ms = t0.elapsed().as_millis();
    let stats = db.cache_stats();

    // Same pass order, same answers — the timing delta is the cache.
    let report = format!(
        "# Block Cache Warm/Cold Matrix — SE2-M7\n\n\
         Generated only when `SE2M7_NIGHTLY=1` (strict opt-in). Perf numbers are\n\
         report cells, never asserts — the report regenerates only with the env set.\n\n\
         - Test: `warm_block_cache_speedup`\n\
         - Build mode: {}\n\
         - Workload: {N} keys × 200-byte values, one segment (64 KiB blocks),\n\
           {N} random-order gets per pass, answers pinned identical across passes\n\n\
         - Cold (cache off), 1 pass: {cold_ms} ms\n\
         - Warm (64 MiB cache), 2nd pass: {warm_ms} ms\n\
         - Warm hits/misses/evictions/bytes: {}/{}/{}/{}\n",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        stats.hits,
        stats.misses,
        stats.evictions,
        stats.bytes,
    );
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("artifacts")
        .join("storage-engine-v2");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("cache-bloom.md"), report).unwrap();
}
