//! KSE-16..18 — storage / write / read amplification (MRFC-KSE-001 §22-24).
//!
//! * KSE-16 storage amplification: logical bytes (Σ of the packed ko/ version
//!   row values — payload + versions + provenance ride the same codec) vs
//!   physical bytes (the WAL file), broken down at store-prefix granularity
//!   (ko/ head/ ke/ relo/ reli/ type/ sys/ … — bucketed from what the store
//!   actually has, never assumed). The §22 sub-rows (relationships /
//!   provenance / evidence) are INSIDE the ko/ value (packed codec) — the
//!   store-level split is ko/ payload vs relo/reli index rows; the report
//!   maps this honestly. The encryption row is measured: the same dataset
//!   through `EncryptedStore` (the KSE-11 wrapper) — overhead is the
//!   envelope per record.
//! * KSE-17 write amplification: physical bytes written by one op of each
//!   class (create / update / relationship update / temporal version /
//!   provenance update / evidence update), measured as the durable-file
//!   delta around ONE op. API mapping (honest — the request surface has no
//!   evidence/temporal fields): temporal = update committed at an advanced
//!   clock (temporal state IS the version row), provenance = update whose
//!   origin+note changed, evidence = one supersede carrying Evidence (the
//!   only evidence-minting update path — 2 write batches, pinned). Redb /
//!   RocksDB deltas are file-LENGTH deltas — they under-report in-page
//!   writes and jump on page/B-tree growth (noted in the report).
//!   MemoryEngine writes nothing durable by definition (0 physical).
//! * KSE-18 read amplification: per workload (get KO / get KO + facts /
//!   get KO + neighbors / get history) the logical objects requested vs
//!   the engine-level records touched + bytes returned, counted by
//!   CountingEngine. The counts are PINNED equal across Memory/redb/
//!   RocksDB/Aikoql (same kernel, same trait — §32: a divergence would
//!   mean the kernel behaves differently per backend). Physical IO per
//!   backend: Aikoql reads 0 bytes at query time (all state in RAM; its
//!   durable cost is the open-time WAL replay — KSE-15); redb/RocksDB
//!   block reads NOT_MEASURED (no tracing wired — KSE-5 precedent).
//!   "Compile context" is the compiler crate's leg (QA2-CONC-001) — its
//!   storage footprint is exactly the get+facts+neighbors workloads here.
//!
//! Timing-free phase: assertions are consistency/equality pins only, so
//! the suite stays green at any machine speed.

mod common;

use aikoql_kernel::security::crypto::{Aes256Gcm, Crypto};
use aikoql_kernel::storage::encrypted::EncryptedStore;
use aikoql_kernel::storage::store::{MemoryEngine, StorageEngine};
use aikoql_kernel::transaction::kernel::ManualClock;
use aikoql_kernel::RedbEngine;
use aikoql_kernel::{
    Direction, Evidence, EvidenceMethod, Kernel, Metadata, Origin, RelationshipRef,
    RememberRequest, SupersedeRequest, Value, KOID,
};
use aikoql_storage::AikoqlStorageEngine;
use common::{ctx, tmp, CountingEngine, LogicalCounts};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(feature = "kse5-rocksdb")]
use aikoql_rocksdb::RocksDbEngine;

const SEED: u64 = 0x16_0000;
const N_KOS: usize = 100;
const TYPE: &str = "kse16_ko";

// ---------------------------------------------------------------------------
// Dataset (KSE-5 shape): 100 KOs, each = create (3 fact props + provenance
// prop) + update carrying 3 outbound links (ring) + 2 plain updates.
// Updates RESTATE caller-owned state wholesale (kernel semantics — only
// supersedes/derived_from/contradicts edges are carried automatically), so
// every update here is a read-modify-write off the head: properties and
// relationships ride along unless the op changes them.
// ---------------------------------------------------------------------------

fn meta(t: &str) -> Metadata {
    Metadata {
        type_name: t.into(),
        tenant: None,
        schema_version: 1,
        tags: vec![],
    }
}

fn open_kernel(engine: Arc<dyn StorageEngine>, label: &str) -> (Arc<Kernel>, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(10_000));
    let k = Arc::new(
        Kernel::open(engine, clock.clone(), SEED)
            .unwrap_or_else(|e| panic!("{label}: kernel open failed: {e:?}")),
    );
    (k, clock)
}

/// Update request with the head's props + rels restated (read-modify-write).
fn rmv(k: &Kernel, koid: KOID) -> RememberRequest {
    let head = k.get(ctx(), &koid).unwrap();
    let mut req = RememberRequest::update(ctx(), koid, meta(TYPE));
    req.properties = head.properties;
    req.relationships = head.relationships;
    req
}

fn seed(k: &Kernel) -> Vec<KOID> {
    let mut koids = Vec::with_capacity(N_KOS);
    for i in 0..N_KOS {
        let mut req = RememberRequest::create(ctx(), meta(TYPE));
        for f in 0..3 {
            req.properties.insert(
                format!("fact-{f}"),
                Value::Text(format!(
                    "kse16 fact #{i} item {f}: payload bytes that make the amplification real"
                )),
            );
        }
        req.properties
            .insert("provenance".into(), Value::Text(format!("kse16-src:{i}")));
        koids.push(k.remember(req).unwrap().koid);
    }
    for i in 0..N_KOS {
        let mut req = rmv(k, koids[i]);
        for r in 1..=3 {
            req.relationships.push(RelationshipRef {
                rel_type: "links".into(),
                target: koids[(i + r) % N_KOS],
                direction: Direction::Outbound,
            });
        }
        k.remember(req).unwrap();
    }
    for koid in &koids {
        for _ in 0..2 {
            k.remember(rmv(k, *koid)).unwrap();
        }
    }
    koids
}

// ---------------------------------------------------------------------------
// KSE-16 — storage amplification.
// ---------------------------------------------------------------------------

struct Kse16 {
    logical: u64, // Σ ko/ value bytes (the packed knowledge content)
    disk: u64,    // WAL file bytes
    live: u64,    // Σ(k+v) of every row
    rows: usize,
    buckets: Vec<(String, u64, usize)>, // (prefix, Σ(k+v), row count)
    enc_disk: u64,                      // same dataset through EncryptedStore
}

fn bucket(key: &[u8]) -> String {
    match key.iter().position(|&b| b == b'/') {
        Some(pos) => String::from_utf8_lossy(&key[..pos]).into_owned(),
        None => "(no slash)".into(),
    }
}

fn measure_kse16(label: &str) -> Kse16 {
    let path = tmp(&format!("kse16-{label}"));
    let engine = Arc::new(AikoqlStorageEngine::open(&path).unwrap());
    let (k, _clock) = open_kernel(engine.clone(), &format!("kse16-{label}"));
    let _ = seed(&k);

    // One full scan, bucketed by prefix: the breakdown is what the store
    // actually contains, never what a model assumes.
    let mut buckets: Vec<(String, u64, usize)> = Vec::new();
    let mut logical = 0u64;
    let mut live = 0u64;
    let mut rows = 0usize;
    for (key, val) in engine.scan(b"").unwrap() {
        live += (key.len() + val.len()) as u64;
        rows += 1;
        let b = bucket(&key);
        match buckets.iter_mut().find(|(name, _, _)| *name == b) {
            Some((_, bytes, n)) => {
                *bytes += (key.len() + val.len()) as u64;
                *n += 1;
            }
            None => buckets.push((b, (key.len() + val.len()) as u64, 1)),
        }
        if key.starts_with(b"ko/") {
            logical += val.len() as u64;
        }
    }
    buckets.sort();
    let sum: u64 = buckets.iter().map(|(_, b, _)| *b).sum();
    assert_eq!(sum, live, "kse16: bucket sum != live total");
    assert!(logical > 0, "kse16: no ko/ payload bytes");

    // Encryption row: the same dataset through EncryptedStore — the delta
    // is the envelope overhead (the engine stays byte-opaque, KSE-11).
    let enc_path = tmp(&format!("kse16-enc-{label}"));
    let enc_inner = Arc::new(AikoqlStorageEngine::open(&enc_path).unwrap());
    let crypto = Arc::new(Crypto::new(Box::new(Aes256Gcm::new())));
    let key = crypto.generate_key();
    let enc = Arc::new(EncryptedStore::new(enc_inner, crypto, key));
    let (enc_k, _) = open_kernel(enc, &format!("kse16-enc-{label}"));
    let _ = seed(&enc_k);
    let enc_disk = std::fs::metadata(&enc_path).unwrap().len();
    assert!(
        enc_disk >= std::fs::metadata(&path).unwrap().len(),
        "kse16: encrypted store smaller than plaintext"
    );

    Kse16 {
        logical,
        disk: std::fs::metadata(&path).unwrap().len(),
        live,
        rows,
        buckets,
        enc_disk,
    }
}

#[test]
fn kse16_storage_amplification() {
    let _ = measure_kse16("test");
}

// ---------------------------------------------------------------------------
// KSE-17 — write amplification: the durable-file delta around ONE op of each
// class, plus the logical bytes written (the new ko/ value bytes).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Op {
    disk: u64,
    logical: u64,
}

struct Kse17 {
    aikoql: [Op; 6],
    aikoql_batches: [u64; 6], // WAL records appended per op (supersede pinned at 2)
    redb: [Op; 6],
    #[cfg(feature = "kse5-rocksdb")]
    rocks: [Op; 6],
}

const OPS: [&str; 6] = [
    "create",
    "update",
    "relationship update",
    "temporal version",
    "provenance update",
    "evidence update (supersede)",
];

fn file_len(path: &PathBuf) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

#[cfg(feature = "kse5-rocksdb")]
fn dir_len(path: &PathBuf) -> u64 {
    std::fs::read_dir(path)
        .unwrap()
        .flatten()
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .sum()
}

/// One run of op class `op` — a fresh request every call, so the op can be
/// executed repeatedly for the logical diff and the disk delta.
fn op_runner<'a>(
    k: &'a Kernel,
    ids: &'a [KOID],
    clock: &'a ManualClock,
    op: usize,
) -> Box<dyn FnMut() + 'a> {
    match op {
        0 => Box::new(move || {
            let mut req = RememberRequest::create(ctx(), meta(TYPE));
            req.properties
                .insert("p".into(), Value::Text("op payload".into()));
            let _ = k.remember(req).unwrap();
        }),
        1 => Box::new(move || {
            let mut up = rmv(k, ids[0]);
            up.properties
                .insert("p".into(), Value::Text("op payload".into()));
            let _ = k.remember(up).unwrap();
        }),
        2 => Box::new(move || {
            let mut up = rmv(k, ids[0]);
            // ids[4] is outside the ring of ids[0] (targets ids[1..3]) — a
            // real edge ADD, deduped so the second run finds it present.
            let rel = RelationshipRef {
                rel_type: "links".into(),
                target: ids[4],
                direction: Direction::Outbound,
            };
            if !up.relationships.contains(&rel) {
                up.relationships.push(rel);
            }
            let _ = k.remember(up).unwrap();
        }),
        3 => Box::new(move || {
            clock.tick(10_000);
            let _ = k.remember(rmv(k, ids[0])).unwrap();
        }),
        4 => Box::new(move || {
            let mut up = rmv(k, ids[0]);
            up.origin = Origin::System;
            up.note = Some("kse17-provenance".into());
            let _ = k.remember(up).unwrap();
        }),
        5 => {
            let mut current = ids[1];
            Box::new(move || {
                // Each run lands on the CURRENT head — after the first run
                // that is the successor itself (the engine refuses to
                // supersede an already-superseded head).
                let mut s = SupersedeRequest::new(ctx(), current, TYPE);
                s.properties
                    .insert("p".into(), Value::Text("op payload".into()));
                s.evidence = vec![Evidence::new("kse17-evid", EvidenceMethod::DocExtraction)];
                let remembered = k.supersede(s).unwrap();
                current = remembered.new;
            })
        }
        _ => unreachable!(),
    }
}

/// The six §23 op classes on a settled store. `delta` is called with the
/// op and returns the durable-file byte delta; the logical column is the
/// new ko/ value bytes (key-set diff around the op).
fn run_ops(
    k: &Kernel,
    engine: &dyn StorageEngine,
    clock: &ManualClock,
    ids: &[KOID],
    mut delta: impl FnMut(&mut dyn FnMut()) -> u64,
) -> [Op; 6] {
    let logical_of_new = |f: &mut dyn FnMut()| -> u64 {
        let before: BTreeSet<Vec<u8>> = engine
            .scan(b"ko/")
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        f();
        engine
            .scan(b"ko/")
            .unwrap()
            .into_iter()
            .filter(|(key, _)| !before.contains(key))
            .map(|(_, val)| val.len() as u64)
            .sum()
    };
    let mut out = [Op {
        disk: 0,
        logical: 0,
    }; 6];
    for (op, slot) in out.iter_mut().enumerate() {
        // One runner per op class, executed twice — state (e.g. the
        // supersede successor) carries from the logical diff to the disk
        // delta, exactly as one logical op hitting the store twice would.
        let mut runner = op_runner(k, ids, clock, op);
        let logical = logical_of_new(&mut *runner);
        let disk = delta(&mut *runner);
        *slot = Op { disk, logical };
    }
    out
}

fn measure_kse17(label: &str) -> Kse17 {
    // Aikoql: exact WAL appends; batch counts pinned per op via CountingEngine.
    let path = tmp(&format!("kse17-aikoql-{label}"));
    let inner = Arc::new(AikoqlStorageEngine::open(&path).unwrap());
    let counting = CountingEngine::new(inner.clone());
    let (k, clock) = open_kernel(counting.clone(), &format!("kse17-aikoql-{label}"));
    let ids = seed(&k);
    let aikoql = run_ops(&k, inner.as_ref(), &clock, &ids, |f| {
        let a = file_len(&path);
        f();
        file_len(&path) - a
    });

    // Batch counts per op on a fresh store (cumulative deltas around each op).
    let fresh_path = tmp(&format!("kse17-aikoql-count-{label}"));
    let fresh_inner = Arc::new(AikoqlStorageEngine::open(&fresh_path).unwrap());
    let fresh_counting = CountingEngine::new(fresh_inner.clone());
    let (fk, fclock) = open_kernel(
        fresh_counting.clone(),
        &format!("kse17-aikoql-count-{label}"),
    );
    let fids = seed(&fk);
    let mut counts = [0u64; 6];
    for (op, slot) in counts.iter_mut().enumerate() {
        let before = LogicalCounts::writes(&fresh_counting).0;
        op_runner(&fk, &fids, &fclock, op)();
        *slot = LogicalCounts::writes(&fresh_counting).0 - before;
    }
    // The supersede composite commits twice (KSE-12 pin) — every other op is
    // one atomic batch.
    assert_eq!(
        counts,
        [1, 1, 1, 1, 1, 2],
        "kse17: unexpected per-op batch counts (supersede must be 2)"
    );

    // Redb: file-length deltas (under-report in-page writes; honest rows).
    let redb_path = tmp(&format!("kse17-redb-{label}"));
    let redb: Arc<dyn StorageEngine> = Arc::new(RedbEngine::open(&redb_path).unwrap());
    let (rk, rclock) = open_kernel(redb.clone(), &format!("kse17-redb-{label}"));
    let rids = seed(&rk);
    let redb = run_ops(&rk, redb.as_ref(), &rclock, &rids, |f| {
        let a = file_len(&redb_path);
        f();
        file_len(&redb_path) - a
    });

    // RocksDB (strict opt-in via the kse5-rocksdb feature): dir-size deltas.
    #[cfg(feature = "kse5-rocksdb")]
    let rocks = {
        let rocks_path = tmp(&format!("kse17-rocks-{label}"));
        let rocks: Arc<dyn StorageEngine> = Arc::new(RocksDbEngine::open(&rocks_path).unwrap());
        let (rok, roclock) = open_kernel(rocks.clone(), &format!("kse17-rocks-{label}"));
        let roids = seed(&rok);
        run_ops(&rok, rocks.as_ref(), &roclock, &roids, |f| {
            let a = dir_len(&rocks_path);
            f();
            dir_len(&rocks_path) - a
        })
    };

    #[cfg(feature = "kse5-rocksdb")]
    {
        Kse17 {
            aikoql,
            aikoql_batches: counts,
            redb,
            rocks,
        }
    }
    #[cfg(not(feature = "kse5-rocksdb"))]
    {
        Kse17 {
            aikoql,
            aikoql_batches: counts,
            redb,
        }
    }
}

#[test]
fn kse17_write_amplification() {
    let _ = measure_kse17("test");
}

// ---------------------------------------------------------------------------
// KSE-18 — read amplification: logical objects requested vs engine records
// touched + bytes returned, pinned equal across backends.
// ---------------------------------------------------------------------------

struct Kse18 {
    get: LogicalCounts,
    facts: LogicalCounts,
    neighbors: LogicalCounts,
    history: LogicalCounts,
    logical: [usize; 4], // objects requested: 1 / 1+facts / 3 (KO + 2 edge sets) / versions
}

fn counted(k: &Kernel, c: &CountingEngine, ids: &[KOID]) -> [LogicalCounts; 4] {
    let before = LogicalCounts::snapshot(c);
    let _ = k.get(ctx(), &ids[0]).unwrap();
    let get = LogicalCounts::snapshot(c).delta(before);

    let before = LogicalCounts::snapshot(c);
    let edges = k.outbound_edges(&ids[0], None).unwrap();
    for (_, tgt) in &edges {
        let _ = k.get(ctx(), tgt).unwrap();
    }
    let facts = LogicalCounts::snapshot(c).delta(before);

    let before = LogicalCounts::snapshot(c);
    let _ = k.get(ctx(), &ids[0]).unwrap();
    let _ = k.outbound_edges(&ids[0], None).unwrap();
    let _ = k.inbound_edges(&ids[0], None).unwrap();
    let neighbors = LogicalCounts::snapshot(c).delta(before);

    let before = LogicalCounts::snapshot(c);
    let _ = k.history(ctx(), &ids[0]).unwrap();
    let history = LogicalCounts::snapshot(c).delta(before);

    [get, facts, neighbors, history]
}

fn measure_kse18(label: &str) -> Kse18 {
    // Every backend gets the same seeded graph (KOIDs mint deterministically
    // from the seed) — the counts must then be identical per workload (§32).
    let run = |tag: &str| -> ([LogicalCounts; 4], usize, usize) {
        let engine: Arc<dyn StorageEngine> = match tag {
            "mem" => Arc::new(MemoryEngine::new()),
            "redb" => {
                let p = tmp(&format!("kse18-redb-{tag}-{label}"));
                Arc::new(RedbEngine::open(&p).unwrap())
            }
            #[cfg(feature = "kse5-rocksdb")]
            "rocks" => {
                let p = tmp(&format!("kse18-rocks-{tag}-{label}"));
                Arc::new(RocksDbEngine::open(&p).unwrap())
            }
            "aikoql" => {
                let p = tmp(&format!("kse18-aikoql-{tag}-{label}"));
                Arc::new(AikoqlStorageEngine::open(&p).unwrap())
            }
            _ => unreachable!(),
        };
        let counting = CountingEngine::new(engine);
        let (k, _clock) = open_kernel(counting.clone(), &format!("kse18-{tag}-{label}"));
        let ids = seed(&k);
        let counts = counted(&k, &counting, &ids);
        let facts_n = k.outbound_edges(&ids[0], None).unwrap().len();
        let hist_n = k.history(ctx(), &ids[0]).unwrap().len();
        (counts, facts_n, hist_n)
    };

    let mem = run("mem");
    let redb = run("redb");
    #[cfg(feature = "kse5-rocksdb")]
    let rocks = run("rocks");
    let aikoql = run("aikoql");

    assert_eq!(mem.0, redb.0, "kse18: memory vs redb divergence");
    #[cfg(feature = "kse5-rocksdb")]
    assert_eq!(mem.0, rocks.0, "kse18: memory vs rocksdb divergence");
    assert_eq!(mem.0, aikoql.0, "kse18: memory vs aikoql divergence");

    Kse18 {
        get: aikoql.0[0],
        facts: aikoql.0[1],
        neighbors: aikoql.0[2],
        history: aikoql.0[3],
        logical: [1, 1 + aikoql.1, 3, aikoql.2],
    }
}

#[test]
fn kse18_read_amplification() {
    let _ = measure_kse18("test");
}

// ---------------------------------------------------------------------------
// Report: artifacts/storage-engine/amplification.md
// ---------------------------------------------------------------------------

#[test]
fn kse161718_report() {
    let s = measure_kse16("report");
    let w = measure_kse17("report");
    let r = measure_kse18("report");

    let s_log = s.logical;
    let s_disk = s.disk;
    let s_live = s.live;
    let s_rows = s.rows;
    let s_amp = format!("{:.2}×", s.disk as f64 / s.logical as f64);
    let l_amp = format!("{:.2}×", s.live as f64 / s.logical as f64);
    let s_enc = s.enc_disk;
    let e_amp = format!(
        "{:.2}%",
        (s.enc_disk - s.disk) as f64 / s.disk as f64 * 100.0
    );
    let buckets = s
        .buckets
        .iter()
        .map(|(name, bytes, n)| format!("{name}: {bytes} B, {n} rows"))
        .collect::<Vec<_>>()
        .join("; ");

    let row17 = |op: &str, a: &Op, ab: u64, r: &Op| {
        format!(
            "| {op} | {} | {} | {ab} | {} |\n",
            a.disk, a.logical, r.disk
        )
    };
    let mut t17 = String::from(
        "| op class | aikoql disk B | aikoql logical B | aikoql batches | redb disk B |",
    );
    #[cfg(feature = "kse5-rocksdb")]
    {
        t17.push_str(" rocksdb disk B |");
    }
    t17.push_str("\n|---|---|---:|---:|---:|");
    #[cfg(feature = "kse5-rocksdb")]
    {
        t17.push_str("---:|");
    }
    t17.push('\n');
    for (i, name) in OPS.iter().enumerate() {
        let a = &w.aikoql[i];
        let r = &w.redb[i];
        #[cfg(feature = "kse5-rocksdb")]
        {
            let mut line = row17(name, a, w.aikoql_batches[i], r);
            let cell = line.trim_end().to_string();
            line = format!("{cell} {} |\n", w.rocks[i].disk);
            t17.push_str(&line);
        }
        #[cfg(not(feature = "kse5-rocksdb"))]
        {
            t17.push_str(&row17(name, a, w.aikoql_batches[i], r));
        }
    }
    #[cfg(not(feature = "kse5-rocksdb"))]
    {
        t17.push_str("| rocksdb | NOT_MEASURED (kse5-rocksdb feature off) |\n");
    }

    let row18 = |name: &str, l: usize, c: &LogicalCounts| {
        format!(
            "| {name} | {l} | {} | {} | {} | {} |\n",
            c.gets, c.scans, c.pairs, c.bytes
        )
    };
    let t18 = format!(
        "| workload | logical | gets | scans | pairs | bytes returned |\n\
         |---|---|---:|---:|---:|---:|\n\
         {}{}{}{}",
        row18("get KO", r.logical[0], &r.get),
        row18("get KO + facts", r.logical[1], &r.facts),
        row18("get KO + neighbors", r.logical[2], &r.neighbors),
        row18("get history", r.logical[3], &r.history),
    );

    let report = format!(
        "# KSE-16..18 — Amplification (MRFC-KSE-001 §22-24)\n\n\
         Date: 2026-09-01 · seed {SEED:#x} · dataset: {N_KOS} KOs (create + rel \
         update + 2 updates each) · debug build, numbers from this suite run\n\n\
         ## KSE-16 — storage amplification\n\n\
         | metric | bytes |\n|---|---:|\n\
         | logical (Σ ko/ value bytes — payload/versions/provenance packed) | {s_log} |\n\
         | physical (WAL file) | {s_disk} |\n\
         | live store (Σ k+v of every row) | {s_live} |\n\
         | rows | {s_rows} |\n\
         | space amplification (disk/logical) | {s_amp} |\n\
         | in-memory amplification (live/logical) | {l_amp} |\n\
         | encrypted physical (same dataset, EncryptedStore) | {s_enc} |\n\
         | encryption overhead | +{e_amp} |\n\n\
         Breakdown (prefix-level Σ(k+v)): {buckets}\n\n\
         Honest mapping: the §22 sub-rows relationships/provenance/evidence \
         are INSIDE the packed ko/ value (codec-level) — the store-level \
         split is ko/ payload vs the relo/reli index rows; a finer \
         decomposition would need codec-level decoding. Evidence enters \
         only via supersede, which is not part of this dataset.\n\n\
         ## KSE-17 — write amplification (durable bytes around ONE op)\n\n\
         {t17}\n\
         MemoryEngine: 0 physical by definition (no durability). Honest \
         rows: redb/rocksdb deltas are file-LENGTH deltas — they \
         under-report in-page writes and jump on page/B-tree growth; \
         aikoql deltas are exact WAL appends. The evidence-minting update \
         path is supersede (the request surface has no evidence field) — 2 \
         batches, pinned above.\n\n\
         ## KSE-18 — read amplification (per workload: logical objects → \
         engine records, bytes)\n\n\
         {t18}\n\
         The record counts are PINNED equal across Memory/redb/RocksDB/Aikoql \
         (§32 — the kernel makes the same requests on every backend). \
         Physical IO per backend: Aikoql reads 0 bytes at query time (all \
         state in RAM; its durable cost is the open-time WAL replay — \
         KSE-15); redb/RocksDB block reads NOT_MEASURED (no tracing wired, \
         KSE-5 precedent). \"Compile context\" is the compiler crate's \
         workload (QA2-CONC-001) — its storage footprint is exactly the \
         get+facts+neighbors workloads above.\n",
    );
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../artifacts/storage-engine");
    std::fs::write(dir.join("amplification.md"), report).unwrap();
}
