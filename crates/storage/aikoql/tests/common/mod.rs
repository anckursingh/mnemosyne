//! Shared harness pieces for the KSE measurement suites (kse5, kse6, …).
#![allow(dead_code)] // each suite uses only the pieces it needs

use aikoql_kernel::storage::store::{StorageEngine, WriteBatch};
use aikoql_kernel::{Direction, Kernel, KnowledgeContext, Subject, KOID};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Pass-through engine that counts every kernel→engine request.
pub struct CountingEngine {
    pub inner: Arc<dyn StorageEngine>,
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
    pub fn new(inner: Arc<dyn StorageEngine>) -> Arc<Self> {
        Arc::new(CountingEngine {
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

impl std::fmt::Display for LogicalCounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} gets + {} scans ({} pairs, {} B returned)",
            self.gets, self.scans, self.pairs, self.bytes
        )
    }
}

pub fn percentiles(mut xs: Vec<u128>) -> (u128, u128, u128) {
    if xs.is_empty() {
        return (0, 0, 0); // a scenario with no samples (e.g. zero readers)
    }
    xs.sort_unstable();
    let p = |q: f64| xs[((xs.len() - 1) as f64 * q).round() as usize];
    (p(0.50), p(0.95), p(0.99))
}

// Temp paths created by THIS thread, swept when the thread exits (the main
// thread's destructor runs at process exit — statics are NOT dropped on
// Windows MSVC, TLS is). Per-thread on purpose: the KSE-141/SE2 kill-harness
// children reopen paths the parent passed them via env and must never delete
// the parent's evidence — a child only ever registers paths it created
// itself, and a hard-killed child never runs TLS destructors at all.
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

pub fn tmp(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("aikoql_kse_unit_{}_{}", tag, std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_dir_all(&p);
    TEMP_PATHS.with(|t| t.borrow_mut().paths.push(p.clone()));
    p
}

// ---------------------------------------------------------------------------
// Sized-WAL generator (kse142, kse143): a deterministic store-level workload
// whose live model is rebuilt byte-exact in a child process by re-running the
// same seeded sequence for B batches — no model serialization crosses the
// process boundary. `Gen::step` is pure (no engine, no IO, no HashMap
// iteration), so one seed reproduces one model in any process; the parent
// pins this by re-running the sequence once and comparing models.
// ---------------------------------------------------------------------------

pub mod walgen {
    use aikoql_kernel::storage::store::{StorageEngine, WriteBatch};
    use aikoql_storage::AikoqlStorageEngine;
    use std::collections::BTreeMap;

    #[derive(Clone, Copy, Debug)]
    pub struct Config {
        pub seed: u64,
        pub keys: u64,        // unique keyspace
        pub families: u64,    // key prefix families (keys round-robin)
        pub value_len: usize, // fixed value size
        pub puts_per_batch: usize,
        pub dels_per_batch: usize,
    }

    pub fn key(cfg: &Config, idx: u64) -> Vec<u8> {
        format!("{}/{idx:08}", idx % cfg.families).into_bytes()
    }

    fn value(idx: u64, wc: u64, len: usize) -> Vec<u8> {
        (0..len)
            .map(|j| (idx.wrapping_mul(31).wrapping_add(wc).wrapping_add(j as u64)) & 0xFF)
            .map(|x| x as u8)
            .collect()
    }

    /// Live model + per-run stats. Key order is BTreeMap order (sorted) — the
    /// engine serves scans sorted too, so model slices compare byte-exact.
    pub struct Gen {
        cfg: Config,
        state: u64,
        model: BTreeMap<Vec<u8>, Vec<u8>>,
        written: Vec<u64>,     // keys ever written — delete pool
        write_count: Vec<u32>, // per key: puts ever
        pub batches: u64,
        pub puts: u64,
        pub dels: u64,
        pub overwrites: u64, // puts on a live key
        pub recreates: u64,  // puts on a deleted key
        pub last_del: Option<Vec<u8>>,
        pub overwrite_pin: Option<(Vec<u8>, Vec<u8>)>, // a key put >= 2x + final value
    }

    impl Gen {
        pub fn new(cfg: Config) -> Gen {
            Gen {
                cfg,
                state: cfg.seed,
                model: BTreeMap::new(),
                written: Vec::new(),
                write_count: vec![0; cfg.keys as usize],
                batches: 0,
                puts: 0,
                dels: 0,
                overwrites: 0,
                recreates: 0,
                last_del: None,
                overwrite_pin: None,
            }
        }

        fn next(&mut self) -> u64 {
            self.state = self
                .state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.state >> 17
        }

        /// One batch: puts then dels, applied to the model in the engine's
        /// own order (puts before dels — KSE-006), so a same-batch put+del
        /// resolves identically on both sides. Put #0 (once any key exists)
        /// overwrites a recently written key — pins a stable overwrite mix.
        pub fn step(&mut self) -> WriteBatch {
            let mut b = WriteBatch::new();
            for p in 0..self.cfg.puts_per_batch {
                let idx = if p == 0 && !self.written.is_empty() {
                    let n = self.next();
                    self.written[n as usize % self.written.len()]
                } else {
                    self.next() % self.cfg.keys
                };
                let k = key(&self.cfg, idx);
                let was_live = self.model.contains_key(&k);
                let wc_before = self.write_count[idx as usize];
                let wc = wc_before + 1;
                self.write_count[idx as usize] = wc;
                if was_live {
                    self.overwrites += 1;
                } else if wc_before > 0 {
                    self.recreates += 1;
                } else {
                    self.written.push(idx);
                }
                let v = value(idx, wc as u64, self.cfg.value_len);
                if wc >= 2 {
                    self.overwrite_pin = Some((k.clone(), v.clone()));
                }
                self.model.insert(k.clone(), v.clone());
                self.puts += 1;
                b.put(k, v);
            }
            for _ in 0..self.cfg.dels_per_batch {
                let n = self.next();
                let idx = self.written[n as usize % self.written.len()];
                let k = key(&self.cfg, idx);
                self.model.remove(&k);
                self.last_del = Some(k.clone());
                self.dels += 1;
                b.del(k);
            }
            self.batches += 1;
            b
        }

        pub fn model(&self) -> &BTreeMap<Vec<u8>, Vec<u8>> {
            &self.model
        }

        /// Keys ever written (unique keys — the spec's "unique keys" cell).
        pub fn unique_keys(&self) -> usize {
            self.written.len()
        }
    }

    /// Write batches until the WAL reaches `target_bytes`; returns the
    /// generator (model + stats) and the exact WAL size reached. Drops the
    /// engine — the measured open happens later, in a child process.
    pub fn generate(path: &std::path::Path, cfg: Config, target_bytes: u64) -> (Gen, u64) {
        let e = AikoqlStorageEngine::open(path).unwrap();
        let mut g = Gen::new(cfg);
        let mut wal = 0;
        while wal < target_bytes {
            e.write_batch(&g.step()).unwrap();
            wal = std::fs::metadata(path).unwrap().len();
        }
        drop(e);
        (g, wal)
    }
}

// ---------------------------------------------------------------------------
// RSS sampling (kse142, kse143): phase-anchored self-reports from inside the
// measured child (precise — read at the exact phase) + a parent-side peak
// poll at interval granularity (a lower bound — spikes between samples are
// missed). Windows-only (PowerShell WorkingSet64); non-Windows callers get
// None/0 and report an honest NOT_SAMPLED row (kse19 convention).
// ---------------------------------------------------------------------------

/// The calling process's own WorkingSet64 at this exact phase.
#[cfg(windows)]
pub fn self_rss() -> Option<u64> {
    let pid = std::process::id();
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {pid}).WorkingSet64"),
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Poll `child`'s WorkingSet64 every `interval_ms` until it exits. Returns
/// (peak, sample count). One PowerShell process does the loop (kse19 shape);
/// the child's exit ends both it and, within one interval, the sampler. A
/// fast child (smoke scale) can outrun the sampler's own startup — zero
/// samples then, reported honestly rather than asserted.
#[cfg(windows)]
pub fn sample_child_peak(child: &mut std::process::Child, interval_ms: u64) -> (u64, usize) {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let pid = child.id();
    let script = format!(
        "while (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ Write-Output (Get-Process -Id {pid}).WorkingSet64; Start-Sleep -Milliseconds {interval_ms} }}"
    );
    let mut sampler = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut peak = 0u64;
    let mut samples = 0usize;
    if let Some(out) = sampler.stdout.take() {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if let Ok(v) = line.trim().parse::<u64>() {
                peak = peak.max(v);
                samples += 1;
            }
        }
    }
    let _ = sampler.wait();
    (peak, samples)
}

// ---------------------------------------------------------------------------
// The six KSE-1 contract asserts (MRFC-KSE-001 §7), shared verbatim by the
// per-backend granular tests (conformance.rs) and the KSE-20 matrix
// (kse20_backend_conformance.rs) — "the same conformance suite" by
// construction: one definition, every backend runs it.
// ---------------------------------------------------------------------------

pub mod kse {
    use super::{StorageEngine, WriteBatch};

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

// ---------------------------------------------------------------------------
// Model-free structural sweep (kse14, kse15): every invariant that must hold
// at ANY batch boundary, computed from the store's own rows — no reference
// model, so it applies wherever the capture/crash point is unknown.
// ---------------------------------------------------------------------------

pub fn ctx() -> KnowledgeContext {
    KnowledgeContext::new(Subject::new("alice"))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<String>()
}

fn derived_keys(engine: &dyn StorageEngine) -> BTreeSet<Vec<u8>> {
    [
        b"relo/".as_slice(),
        b"reli/".as_slice(),
        b"type/".as_slice(),
    ]
    .into_iter()
    .flat_map(|p| engine.scan(p).unwrap())
    .map(|(k, _v)| k)
    .collect()
}

pub fn structural_sweep(k: &Kernel, engine: &dyn StorageEngine, label: &str) {
    let heads: Vec<Vec<u8>> = engine
        .scan(b"head/")
        .unwrap()
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(
        heads.len(),
        heads.iter().collect::<BTreeSet<_>>().len(),
        "{label}: duplicate KOID in head/"
    );
    let head_koids: BTreeSet<Vec<u8>> = heads.iter().map(|key| key[5..].to_vec()).collect();

    // Version rows: exactly one per (koid, ts); every row's KOID has a head.
    let mut version_rows = BTreeSet::new();
    for (key, _v) in engine.scan(b"ko/").unwrap() {
        assert_eq!(key.len(), 3 + 16 + 8, "{label}: malformed version key");
        assert!(
            head_koids.contains(&key[3..19]),
            "{label}: version row {} for a KOID with no head",
            hex(&key)
        );
        assert!(
            version_rows.insert(key.clone()),
            "{label}: duplicate (koid, ts) row {}",
            hex(&key)
        );
    }

    // One journal event per version (QA2-PROP invariant), seqs exactly 1..=n.
    let seqs: Vec<u64> = engine
        .scan(b"ke/")
        .unwrap()
        .into_iter()
        .map(|(key, _)| {
            assert_eq!(key.len(), 3 + 8, "{label}: malformed event key");
            u64::from_be_bytes(key[3..].try_into().unwrap())
        })
        .collect();
    assert_eq!(
        seqs,
        (1..=seqs.len() as u64).collect::<Vec<_>>(),
        "{label}: journal seqs not exactly 1..=n"
    );
    assert_eq!(
        seqs.len(),
        version_rows.len(),
        "{label}: journal events != version rows"
    );

    // Every head: coherent provenance, contiguous lineage, sane interval —
    // and the derived-set image is computed FROM these heads below.
    let mut image = BTreeSet::new();
    for key in &heads {
        let koid = KOID::from_hex(&hex(&key[5..])).unwrap();
        let head = k
            .get(ctx(), &koid)
            .unwrap_or_else(|e| panic!("{label}: get head {} failed: {e:?}", koid.to_hex()));
        assert_eq!(
            head.event_refs.len(),
            head.version as usize,
            "{label}: half-committed head {}",
            koid.to_hex()
        );
        assert!(
            head.event_refs.windows(2).all(|w| w[0].seq < w[1].seq),
            "{label}: event seqs not increasing on {}",
            koid.to_hex()
        );
        if let (Some(f), Some(t)) = (head.valid_from(), head.valid_to()) {
            assert!(f <= t, "{label}: inverted interval on {}", koid.to_hex());
        }
        // Lineage from the version rows themselves — decoded straight off the
        // engine (O(lineage) prefix scan per head). NOT k.trace: trace's
        // full scan_events per KO makes this loop O(N²) at scale; and NOT
        // k.history: it skips supersede-transition rows, which kse14's
        // lineages legitimately contain. Decoding here also fails closed on
        // any version row the codec cannot read.
        let mut ver = Vec::new();
        let mut cts = Vec::new();
        let mut prefix = b"ko/".to_vec();
        prefix.extend_from_slice(&key[5..]); // koid bytes — version rows are
                                             // ko/<koid><ts8>, no separator
        for (_, val) in engine.scan(&prefix).unwrap() {
            // decode_ko_wire — what the repository itself uses for version
            // rows (storage/repository.rs scan_object_versions).
            let ko = aikoql_kernel::codec::decode_ko_wire(&val)
                .unwrap_or_else(|e| panic!("{label}: version row decode failed: {e:?}"));
            ver.push(ko.version);
            cts.push(ko.commit_ts);
        }
        assert_eq!(
            ver.len(),
            head.version as usize,
            "{label}: lineage length != version on {}",
            koid.to_hex()
        );
        assert!(
            ver.windows(2).all(|w| w[0] + 1 == w[1]),
            "{label}: gapped lineage on {}",
            koid.to_hex()
        );
        assert!(
            cts.windows(2).all(|w| w[0] <= w[1]),
            "{label}: commit_ts ran backwards on {}",
            koid.to_hex()
        );
        for r in &head.relationships {
            let (src, dst) = match r.direction {
                Direction::Outbound => (&koid, &r.target),
                Direction::Inbound => (&r.target, &koid),
            };
            let mut relo = b"relo/".to_vec();
            relo.extend_from_slice(src.as_bytes());
            relo.push(b'/');
            relo.extend_from_slice(r.rel_type.as_bytes());
            relo.push(b'/');
            relo.extend_from_slice(dst.as_bytes());
            image.insert(relo);
            let mut reli = b"reli/".to_vec();
            reli.extend_from_slice(dst.as_bytes());
            reli.push(b'/');
            reli.extend_from_slice(r.rel_type.as_bytes());
            reli.push(b'/');
            reli.extend_from_slice(src.as_bytes());
            image.insert(reli);
        }
        let mut tk = b"type/".to_vec();
        tk.extend_from_slice(head.metadata.type_name.as_bytes());
        tk.push(b'/');
        tk.extend_from_slice(koid.as_bytes());
        image.insert(tk);
    }
    assert_eq!(
        derived_keys(engine),
        image,
        "{label}: derived indexes drifted from their own heads"
    );
    let report = k.rebuild_derived_indexes().unwrap();
    assert_eq!(
        (report.removed_stale, report.removed_invalid),
        (0, 0),
        "{label}: rebuild found drift the sweep missed"
    );
    assert_eq!(
        derived_keys(engine),
        image,
        "{label}: rebuild changed the derived set"
    );
}
