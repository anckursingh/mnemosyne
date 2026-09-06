//! Shared SE2 test helpers (docs/TESTING-PLAN-V2.md).
// Each test binary compiles this module but uses a subset — the unused
// helper would be dead code there, so the module opts out wholesale.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use aikoql_storage_v2::segment::SegmentEntry;
use aikoql_storage_v2::stats::ReadPathStats;

/// Parallel tests in one binary share a pid, so a plain tag+pid path would
/// collide between tests using the same tag — every call gets its own
/// counter suffix instead.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The 3-entry fixture segment (keys "a1"/"a2"/"a3", PUT/VERSION/DELETE).
pub fn entry(key: &str, value: &str, seq: u64, flags: u8) -> SegmentEntry {
    SegmentEntry {
        key: key.as_bytes().to_vec(),
        value: value.as_bytes().to_vec(),
        seq,
        flags,
        replica_id: aikoql_storage_v2::identity::ReplicaId(0),
    }
}

// Temp paths created by THIS thread, swept when the thread exits (the main
// thread's destructor runs at process exit — statics are NOT dropped on
// Windows MSVC, TLS is). Per-thread on purpose: the SE2 kill-harness children
// reopen paths the parent passed them via env and must never delete the
// parent's evidence — a child only ever registers paths it created itself,
// and a hard-killed child never runs TLS destructors at all.
thread_local! {
    static TEMP_PATHS: std::cell::RefCell<TempSweeper> =
        const { std::cell::RefCell::new(TempSweeper { paths: Vec::new() }) };
}

struct TempSweeper {
    paths: Vec<PathBuf>,
}
impl Drop for TempSweeper {
    fn drop(&mut self) {
        for p in &self.paths {
            let _ = std::fs::remove_file(p);
            let _ = std::fs::remove_dir_all(p);
            // Sidecars the engine creates NEXT TO the registered stem
            // (`{stem}.kse`, `{stem}.redb.artifacts`): the stem is
            // pid-unique, so a `{stem}.` prefix match is own-files only.
            let Some(name) = p.file_name() else { continue };
            if let Ok(rd) = std::fs::read_dir(p.parent().unwrap_or(std::path::Path::new("."))) {
                let prefix = format!("{}.", name.to_string_lossy());
                for e in rd.flatten() {
                    if e.file_name().to_string_lossy().starts_with(&prefix) {
                        let _ = std::fs::remove_file(e.path());
                        let _ = std::fs::remove_dir_all(e.path());
                    }
                }
            }
        }
    }
}

/// A unique scratch FILE path under the OS temp dir: tag + pid so parallel
/// test binaries never collide; any stale file is removed so reruns are clean.
pub fn tmp(tag: &str) -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("aikoql-v2-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    TEMP_PATHS.with(|t| t.borrow_mut().paths.push(path.clone()));
    path
}

/// Per-window read-path counter delta (SE2-M21). Saturating: a background
/// committer flush can move segment-level counters between snapshots.
pub fn stats_delta(after: ReadPathStats, before: ReadPathStats) -> ReadPathStats {
    ReadPathStats {
        lookups: after.lookups.saturating_sub(before.lookups),
        memtable_lookup_ns: after
            .memtable_lookup_ns
            .saturating_sub(before.memtable_lookup_ns),
        memtable_hits: after.memtable_hits.saturating_sub(before.memtable_hits),
        segments_considered: after
            .segments_considered
            .saturating_sub(before.segments_considered),
        segments_range_skipped: after
            .segments_range_skipped
            .saturating_sub(before.segments_range_skipped),
        segments_bloom_skipped: after
            .segments_bloom_skipped
            .saturating_sub(before.segments_bloom_skipped),
        segments_index_searched: after
            .segments_index_searched
            .saturating_sub(before.segments_index_searched),
        index_lookup_ns: after.index_lookup_ns.saturating_sub(before.index_lookup_ns),
        block_cache_lookup_ns: after
            .block_cache_lookup_ns
            .saturating_sub(before.block_cache_lookup_ns),
        block_cache_hits: after
            .block_cache_hits
            .saturating_sub(before.block_cache_hits),
        block_cache_misses: after
            .block_cache_misses
            .saturating_sub(before.block_cache_misses),
        block_io_ns: after.block_io_ns.saturating_sub(before.block_io_ns),
        block_decode_ns: after.block_decode_ns.saturating_sub(before.block_decode_ns),
        blocks_read: after.blocks_read.saturating_sub(before.blocks_read),
        bytes_read: after.bytes_read.saturating_sub(before.bytes_read),
        entries_decoded: after.entries_decoded.saturating_sub(before.entries_decoded),
        lock_wait_ns: after.lock_wait_ns.saturating_sub(before.lock_wait_ns),
        bloom_probe_ns: after.bloom_probe_ns.saturating_sub(before.bloom_probe_ns),
        get_wall_ns: after.get_wall_ns.saturating_sub(before.get_wall_ns),
    }
}

/// A fresh, empty scratch DIRECTORY under the OS temp dir (same tag+pid
/// scheme); any stale directory is wiped first.
pub fn dir(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("aikoql-v2-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    TEMP_PATHS.with(|t| t.borrow_mut().paths.push(path.clone()));
    path
}

/// Golden fixtures are hex — this is the only format-drift surface left to
/// eyeballs.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Kernel measurement harness (CountingEngine / LogicalCounts / percentiles /
// ctx), copied VERBATIM from `crates/storage/aikoql/tests/common/mod.rs` —
// the W1..W8 rows in the V2-Adopt matrix must count the same logical bytes
// as v1's M7 matrix did, and one definition is what makes that honest.
// ---------------------------------------------------------------------------

use aikoql_kernel::storage::store::{StorageEngine, WriteBatch};

/// Pass-through engine that counts every kernel→engine request.
pub struct CountingEngine {
    pub inner: std::sync::Arc<dyn StorageEngine>,
    gets: AtomicU64,
    scan_calls: AtomicU64,
    scan_pairs: AtomicU64,
    bytes_returned: AtomicU64,
    write_batches: AtomicU64,
    puts: AtomicU64,
    dels: AtomicU64,
    bytes_written: AtomicU64,
}

impl CountingEngine {
    pub fn new(inner: std::sync::Arc<dyn StorageEngine>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(CountingEngine {
            inner,
            gets: AtomicU64::new(0),
            scan_calls: AtomicU64::new(0),
            scan_pairs: AtomicU64::new(0),
            bytes_returned: AtomicU64::new(0),
            write_batches: AtomicU64::new(0),
            puts: AtomicU64::new(0),
            dels: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
        })
    }
}

impl StorageEngine for CountingEngine {
    fn get(&self, key: &[u8]) -> aikoql_kernel::KResult<Option<Vec<u8>>> {
        self.gets.fetch_add(1, Ordering::Relaxed);
        let v = self.inner.get(key)?;
        if let Some(v) = &v {
            self.bytes_returned
                .fetch_add(v.len() as u64, Ordering::Relaxed);
        }
        Ok(v)
    }

    fn scan(&self, prefix: &[u8]) -> aikoql_kernel::KResult<Vec<(Vec<u8>, Vec<u8>)>> {
        self.scan_calls.fetch_add(1, Ordering::Relaxed);
        let rows = self.inner.scan(prefix)?;
        self.scan_pairs
            .fetch_add(rows.len() as u64, Ordering::Relaxed);
        let bytes: u64 = rows.iter().map(|(k, v)| (k.len() + v.len()) as u64).sum();
        self.bytes_returned.fetch_add(bytes, Ordering::Relaxed);
        Ok(rows)
    }

    fn write_batch(&self, batch: &WriteBatch) -> aikoql_kernel::KResult<()> {
        self.write_batches.fetch_add(1, Ordering::Relaxed);
        self.puts
            .fetch_add(batch.puts.len() as u64, Ordering::Relaxed);
        self.dels
            .fetch_add(batch.dels.len() as u64, Ordering::Relaxed);
        let wb: u64 = batch
            .puts
            .iter()
            .map(|(k, v)| (k.len() + v.len()) as u64)
            .sum();
        self.bytes_written.fetch_add(wb, Ordering::Relaxed);
        self.inner.write_batch(batch)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogicalCounts {
    pub gets: u64,
    pub scans: u64,
    pub pairs: u64,
    pub bytes: u64,
}

impl LogicalCounts {
    pub fn snapshot(c: &CountingEngine) -> LogicalCounts {
        LogicalCounts {
            gets: c.gets.load(Ordering::Relaxed),
            scans: c.scan_calls.load(Ordering::Relaxed),
            pairs: c.scan_pairs.load(Ordering::Relaxed),
            bytes: c.bytes_returned.load(Ordering::Relaxed),
        }
    }

    pub fn delta(&self, before: LogicalCounts) -> LogicalCounts {
        LogicalCounts {
            gets: self.gets - before.gets,
            scans: self.scans - before.scans,
            pairs: self.pairs - before.pairs,
            bytes: self.bytes - before.bytes,
        }
    }

    pub fn writes(c: &CountingEngine) -> (u64, u64, u64) {
        (
            c.write_batches.load(Ordering::Relaxed),
            c.puts.load(Ordering::Relaxed),
            c.dels.load(Ordering::Relaxed),
        )
    }
}

/// Σ put key+value bytes across all batches — the logical bytes written.
pub fn bytes_written(c: &CountingEngine) -> u64 {
    c.bytes_written.load(Ordering::Relaxed)
}

pub fn percentiles(mut xs: Vec<u128>) -> (u128, u128, u128) {
    if xs.is_empty() {
        return (0, 0, 0); // a scenario with no samples (e.g. zero readers)
    }
    xs.sort_unstable();
    let p = |q: f64| xs[((xs.len() - 1) as f64 * q).round() as usize];
    (p(0.50), p(0.95), p(0.99))
}

/// A kernel read context (alice).
pub fn ctx() -> aikoql_kernel::KnowledgeContext {
    aikoql_kernel::KnowledgeContext::new(aikoql_kernel::Subject::new("alice"))
}

// ---------------------------------------------------------------------------
// The six KSE-1 contract asserts (MRFC-KSE-001 §7), copied VERBATIM from
// `crates/storage/aikoql/tests/common/mod.rs` — the one shared definition
// every backend runs (v1's conformance.rs + KSE-20 matrix use the same
// text). V2-Adopt: v2 runs them unchanged, so a green row in the KSE-20
// matrix is honest by construction.
// ---------------------------------------------------------------------------

pub mod kse {
    use aikoql_kernel::storage::store::{StorageEngine, WriteBatch};

    /// KSE-001: get returns the written value.
    pub fn kse001_get(e: &dyn StorageEngine) {
        let mut b = WriteBatch::new();
        b.put(b"k1".to_vec(), b"v1".to_vec());
        e.write_batch(&b).unwrap();
        assert_eq!(e.get(b"k1").unwrap(), Some(b"v1".to_vec()));
    }

    /// KSE-002: a missing key reads as None.
    pub fn kse002_missing_key(e: &dyn StorageEngine) {
        assert_eq!(e.get(b"missing").unwrap(), None);
    }

    /// KSE-003: prefix scan returns exactly the prefix's keys, sorted ascending.
    pub fn kse003_prefix_scan(e: &dyn StorageEngine) {
        let mut b = WriteBatch::new();
        for k in [&b"a/3"[..], &b"a/1"[..], &b"a/2"[..], &b"b/1"[..]] {
            b.put(k.to_vec(), vec![0]);
        }
        e.write_batch(&b).unwrap();
        let got: Vec<Vec<u8>> = e.scan(b"a/").unwrap().into_iter().map(|(k, _)| k).collect();
        assert_eq!(got, vec![b"a/1".to_vec(), b"a/2".to_vec(), b"a/3".to_vec()]);
    }

    /// KSE-004: puts and deletes in one batch become visible atomically.
    pub fn kse004_atomic_batch(e: &dyn StorageEngine) {
        let mut b = WriteBatch::new();
        b.put(b"x".to_vec(), vec![1]);
        b.put(b"y".to_vec(), vec![2]);
        e.write_batch(&b).unwrap();
        let mut d = WriteBatch::new();
        d.del(b"x".to_vec());
        d.put(b"z".to_vec(), vec![3]);
        e.write_batch(&d).unwrap();
        assert_eq!(e.get(b"x").unwrap(), None);
        assert_eq!(e.get(b"y").unwrap(), Some(vec![2]));
        assert_eq!(e.get(b"z").unwrap(), Some(vec![3]));
    }

    /// KSE-005: an empty batch produces no state change.
    pub fn kse005_empty_batch(e: &dyn StorageEngine) {
        let mut b = WriteBatch::new();
        b.put(b"k".to_vec(), vec![1]);
        e.write_batch(&b).unwrap();
        e.write_batch(&WriteBatch::new()).unwrap();
        assert_eq!(e.get(b"k").unwrap(), Some(vec![1]));
    }

    /// KSE-006: deterministic semantics for a key in both puts and deletes.
    ///
    /// All backends apply puts before dels (documented invariant in
    /// `store.rs`), so a put+del of the same key deletes it; duplicate puts
    /// resolve to the last value.
    pub fn kse006_conflicting_put_delete(e: &dyn StorageEngine) {
        let mut b = WriteBatch::new();
        b.put(b"c".to_vec(), vec![1]);
        b.del(b"c".to_vec());
        b.put(b"d".to_vec(), vec![1]);
        b.put(b"d".to_vec(), vec![2]);
        e.write_batch(&b).unwrap();
        assert_eq!(e.get(b"c").unwrap(), None); // put then del: deleted
        assert_eq!(e.get(b"d").unwrap(), Some(vec![2])); // last put wins
    }
}

/// Today's date as `yyyy-MM-dd` from the system clock — the civil-date
/// algorithm is eight lines, so no chrono; the artifact harnesses must
/// stamp the run's actual date, not a hardcoded one.
pub fn run_date() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        / 86_400;
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    format!("{y:04}-{m:02}-{d:02}")
}
