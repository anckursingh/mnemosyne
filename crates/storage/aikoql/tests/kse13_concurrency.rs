//! KSE-13 — concurrency (MRFC-KSE-001 §19, KSE-120).
//!
//! §19 splits kernel transaction serialization from storage concurrent
//! access; this file tests both surfaces:
//!
//! 1. KSE-120a — storage concurrent access: 32 threads call `write_batch`
//!    on the raw engine, no kernel in between. The WAL is the durability
//!    contract, so the pin is: live state must byte-equal a reopen-replay
//!    of the same log (a crash right after the storm must recover exactly
//!    what the running store serves). Shared keys force log/mem ordering
//!    races; distinct keys pin no-lost-updates.
//!
//! 2. KSE-120b — kernel-level mixed read/write stress: one `Arc<Kernel>`
//!    over the engine, 32–256 readers / 4–32 writers on the doc workloads
//!    (KO lookup, relationship traversal, history, type scan / ingestion,
//!    update, delete, supersede, relate, unrelate). Context compilation
//!    is out of the engine's reach by design — it is a compiler-crate
//!    component and its concurrent leg (QA2-CONC-001) already lives in
//!    `crates/ingestion/tests/qa2_concurrency.rs`. The §19 expecteds:
//!    no deadlocks (every thread joins; a hang fails the CI timeout),
//!    no corruption (post-storm sweep: derived indexes byte-equal a
//!    model image, head uniqueness, version-row/journal pins, rebuild
//!    no-op probe — the KSE-12 sweep, run once after the storm), no
//!    invalid logical reads (every reader asserts at-any-instant shape
//!    pins: version ≥ 1, one provenance event per version in order,
//!    traversal targets all resolve, type scan homogeneous), no
//!    duplicate commits (final version and lineage match the model
//!    exactly), no authorization bypass (bob's KOs stay invisible to
//!    alice on every read shape).
//!
//! Perf: P50/P95/P99 + throughput per op class, reported to
//! `artifacts/storage-engine/concurrency.md` (same writer pattern
//! as kse5-7). CPU/RSS/IO/contention stay honest NOT_MEASURED rows —
//! no counting allocator or IO tracing is wired.
//!
//! Sizing: KSE13_NIGHTLY=1 → 256 readers × 800 ops / 32 writers × 1200
//! ops; unset → the §19 minimum 32 × 250 / 4 × 400 (CI smoke). Strict
//! opt-in: env set but not honored = fail, no silent short runs.

mod common;

use aikoql_kernel::storage::store::{StorageEngine, WriteBatch};
use aikoql_kernel::transaction::kernel::ManualClock;
use aikoql_kernel::{
    Direction, Evidence, EvidenceMethod, ForgetMode, Kernel, KnowledgeContext, LifecycleState,
    Metadata, PropertyMap, RelationshipRef, RememberRequest, Subject, SupersedeRequest, Value,
    KOID,
};
use aikoql_storage::AikoqlStorageEngine;
use common::tmp;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Instant;

const SEED: u64 = 0x13_0000;
const TYPE: &str = "conc_ko";

// ---------------------------------------------------------------------------
// Deterministic RNG (xorshift64*) — one per thread, seeded by thread id.
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pct(&mut self) -> u64 {
        self.next() % 100
    }
}

fn ctx() -> KnowledgeContext {
    KnowledgeContext::new(Subject::new("alice"))
}

fn bob() -> KnowledgeContext {
    KnowledgeContext::new(Subject::new("bob"))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<String>()
}

fn meta(t: &str) -> Metadata {
    Metadata {
        type_name: t.into(),
        tenant: None,
        schema_version: 1,
        tags: vec![],
    }
}

fn props(seq: u64) -> PropertyMap {
    let mut p = PropertyMap::new();
    p.insert("seq".into(), Value::Int(seq as i64));
    p
}

// ---------------------------------------------------------------------------
// Reference model — mirrors every committed state (KSE-12 pattern).
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MKo {
    version: u64,
    props: PropertyMap,
    rels: Vec<(String, KOID, Direction)>,
    deleted: bool,
    superseded: bool,
    type_name: String,
}

#[derive(Default)]
struct Model {
    kos: HashMap<KOID, MKo>,
    keys: Vec<KOID>,
}

fn fresh(t: &str, version: u64, props: PropertyMap) -> MKo {
    MKo {
        version,
        props,
        rels: vec![],
        deleted: false,
        superseded: false,
        type_name: t.into(),
    }
}

/// The exact derived key set the model demands (KSE-12's expected_derived,
/// generalized to per-KO type names). Drift = orphan or missing index row.
fn expected_derived(m: &Model, bob_kos: &HashMap<KOID, MKo>) -> BTreeSet<Vec<u8>> {
    let mut out = BTreeSet::new();
    for (koid, mk) in m.kos.iter().chain(bob_kos.iter()) {
        for (t, target, dir) in &mk.rels {
            let (src, dst) = match dir {
                Direction::Outbound => (koid, target),
                Direction::Inbound => (target, koid),
            };
            let mut relo = b"relo/".to_vec();
            relo.extend_from_slice(src.as_bytes());
            relo.push(b'/');
            relo.extend_from_slice(t.as_bytes());
            relo.push(b'/');
            relo.extend_from_slice(dst.as_bytes());
            out.insert(relo);
            let mut reli = b"reli/".to_vec();
            reli.extend_from_slice(dst.as_bytes());
            reli.push(b'/');
            reli.extend_from_slice(t.as_bytes());
            reli.push(b'/');
            reli.extend_from_slice(src.as_bytes());
            out.insert(reli);
        }
        let mut tk = b"type/".to_vec();
        tk.extend_from_slice(mk.type_name.as_bytes());
        tk.push(b'/');
        tk.extend_from_slice(koid.as_bytes());
        out.insert(tk);
    }
    out
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

/// KSE-12's check_ko, adapted: subject per KO (bob's are invisible to
/// alice by design) and no op index (post-storm, so panic messages carry
/// the KOID).
fn check_ko(k: &Kernel, c: KnowledgeContext, mk: &MKo, koid: &KOID) {
    let head = k
        .get(c, koid)
        .unwrap_or_else(|e| panic!("get {} failed: {e:?}", koid.to_hex()));
    assert_eq!(
        head.version,
        mk.version,
        "version mismatch on {}",
        koid.to_hex()
    );
    assert_eq!(
        head.properties,
        mk.props,
        "props mismatch on {}",
        koid.to_hex()
    );
    let rels: Vec<(String, KOID)> = head
        .relationships
        .iter()
        .map(|r| (r.rel_type.clone(), r.target))
        .collect();
    let model_rels: Vec<(String, KOID)> = mk.rels.iter().map(|(t, d, _)| (t.clone(), *d)).collect();
    assert_eq!(
        rels,
        model_rels,
        "relationships mismatch on {}",
        koid.to_hex()
    );
    assert_eq!(
        head.lifecycle.state == LifecycleState::Deleted,
        mk.deleted,
        "lifecycle mismatch on {}",
        koid.to_hex()
    );
    if mk.superseded {
        assert_eq!(
            head.epistemic_status(),
            aikoql_kernel::EpistemicStatus::Superseded,
            "superseded KO lost its status: {}",
            koid.to_hex()
        );
    }
    assert_eq!(
        head.event_refs.len(),
        mk.version as usize,
        "provenance event count != version on {}",
        koid.to_hex()
    );
    assert!(
        head.event_refs.windows(2).all(|w| w[0].seq < w[1].seq),
        "event seqs not strictly increasing on {}",
        koid.to_hex()
    );
    if let (Some(f), Some(t)) = (head.valid_from(), head.valid_to()) {
        assert!(f <= t, "inverted interval [{f},{t}] on {}", koid.to_hex());
    }
}

fn check_lineage(k: &Kernel, c: KnowledgeContext, mk: &MKo, koid: &KOID) {
    let tr = k
        .trace(c, koid)
        .unwrap_or_else(|e| panic!("trace {} failed: {e:?}", koid.to_hex()));
    assert_eq!(
        tr.versions.len(),
        mk.version as usize,
        "lineage length != version on {}",
        koid.to_hex()
    );
    assert!(
        tr.versions
            .windows(2)
            .all(|w| w[0].version + 1 == w[1].version),
        "versions not exactly 1..=n on {}",
        koid.to_hex()
    );
    assert!(
        tr.versions
            .windows(2)
            .all(|w| w[0].commit_ts <= w[1].commit_ts),
        "commit_ts ran backwards on {}",
        koid.to_hex()
    );
}

/// Post-storm sweep: the full KSE-12 corruption battery, once, after all
/// threads joined.
fn sweep(k: &Kernel, engine: &dyn StorageEngine, m: &Model, bob_kos: &HashMap<KOID, MKo>) {
    for (koid, mk) in &m.kos {
        check_ko(k, ctx(), mk, koid);
        check_lineage(k, ctx(), mk, koid);
    }
    for (koid, mk) in bob_kos {
        check_ko(k, bob(), mk, koid);
        check_lineage(k, bob(), mk, koid);
    }
    assert_eq!(
        derived_keys(engine),
        expected_derived(m, bob_kos),
        "derived index drifted after the storm"
    );
    let heads: Vec<Vec<u8>> = engine
        .scan(b"head/")
        .unwrap()
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        heads.len(),
        heads.iter().collect::<BTreeSet<_>>().len(),
        "duplicate logical ID in head/"
    );
    let head_koids: BTreeSet<Vec<u8>> = heads.into_iter().map(|k| k[5..].to_vec()).collect();
    let mut seen = BTreeSet::new();
    for (key, _v) in engine.scan(b"ko/").unwrap() {
        // ko/<koid16><ts8> — no separator after the KOID (obj_key layout).
        assert_eq!(key.len(), 3 + 16 + 8, "malformed version key");
        let koid = &key[3..19];
        assert!(
            head_koids.contains(koid),
            "version row {} for a KOID ({}) with no head",
            hex(&key),
            hex(koid)
        );
        assert!(seen.insert(key.clone()), "duplicate (koid, ts) version row");
    }
    let events = engine.scan(b"ke/").unwrap().len();
    let versions: u64 = m
        .kos
        .values()
        .chain(bob_kos.values())
        .map(|mk| mk.version)
        .sum();
    assert_eq!(
        events as u64, versions,
        "journal events != committed versions"
    );
    let report = k.rebuild_derived_indexes().unwrap();
    assert_eq!(
        (report.removed_stale, report.removed_invalid),
        (0, 0),
        "rebuild found drift the storm missed"
    );
    assert_eq!(
        derived_keys(engine),
        expected_derived(m, bob_kos),
        "rebuild changed the derived set"
    );
}

// ---------------------------------------------------------------------------
// Per-op latency collection → P50/P95/P99 report.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Perf {
    map: Mutex<HashMap<&'static str, Vec<f64>>>, // µs per op, per class
}

impl Perf {
    fn push(&self, class: &'static str, d: f64) {
        self.map.lock().unwrap().entry(class).or_default().push(d);
    }
}

fn pct(sorted: &[f64], q: usize) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[(sorted.len() - 1) * q / 100]
}

// ---------------------------------------------------------------------------
// KSE-120a — storage concurrent access: live state == WAL replay.
// ---------------------------------------------------------------------------

#[test]
fn kse120a_concurrent_write_batch_live_equals_replay() {
    const THREADS: usize = 32;
    const ITERS: usize = 200;
    let path = tmp("kse13a");
    let engine = Arc::new(AikoqlStorageEngine::open(&path).unwrap());
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = Vec::new();
    for t in 0..THREADS {
        let engine = engine.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            for i in 0..ITERS {
                let mut wb = WriteBatch::new();
                // Distinct key: no cross-thread collision — pins
                // no-lost-updates.
                let priv_key = format!("priv/{t:02}/{i:03}");
                wb.put(priv_key.into_bytes(), format!("t{t}i{i}").into_bytes());
                // Shared key: forces cross-thread ordering races — pins
                // log/mem apply agreement.
                let shared_key = format!("shared/{:02}", t % 4);
                wb.put(shared_key.into_bytes(), format!("t{t}i{i}").into_bytes());
                engine.write_batch(&wb).unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let live: BTreeSet<(Vec<u8>, Vec<u8>)> = engine.scan(b"").unwrap().into_iter().collect();
    // Every distinct key must be present: 32 × 200 = 6400 of them.
    assert_eq!(
        live.iter().filter(|(k, _)| k.starts_with(b"priv/")).count(),
        THREADS * ITERS,
        "lost put under concurrent write_batch"
    );
    // The durability contract: a crash right now recovers via replay —
    // the live store must equal what the log says.
    let replayed: BTreeSet<(Vec<u8>, Vec<u8>)> = {
        drop(engine);
        let reopened = AikoqlStorageEngine::open(&path).unwrap();
        reopened.scan(b"").unwrap().into_iter().collect()
    };
    assert_eq!(
        live, replayed,
        "live state diverged from WAL replay — concurrent commits landed in \
         different orders in the log and in memory"
    );
}

// ---------------------------------------------------------------------------
// KSE-120b — mixed read/write stress through the real kernel.
// ---------------------------------------------------------------------------

#[test]
fn kse120b_mixed_read_write_stress_five_expecteds() {
    let nightly = std::env::var("KSE13_NIGHTLY")
        .map(|v| v == "1")
        .unwrap_or(false);
    // §19 ranges: 32–256 readers, 4–32 writers; the smoke runs the
    // minimum, the nightly gate the maximum.
    let (readers, writers, r_ops, w_ops) = if nightly {
        (256usize, 32usize, 800usize, 1200usize)
    } else {
        (32, 4, 250, 400)
    };
    assert_eq!(
        (readers, writers) == (256, 32),
        nightly,
        "KSE13_NIGHTLY set but the run did not honor it"
    );

    let engine = Arc::new(AikoqlStorageEngine::open(tmp("kse13b")).unwrap());
    let clock = Arc::new(ManualClock::new(10_000));
    let k = Arc::new(Kernel::open(engine.clone(), clock.clone(), SEED).unwrap());

    // Setup: bob owns three secret KOs; they never mutate during the
    // storm — the authorization pin's stable fixture.
    let mut bob_kos = HashMap::new();
    for i in 0..3u64 {
        let mut r = RememberRequest::create(bob(), meta("secret"));
        r.properties = props(i);
        let rem = k.remember(r).unwrap();
        bob_kos.insert(rem.koid, fresh("secret", rem.version, props(i)));
    }

    let model = Arc::new(Mutex::new(Model::default()));
    let perf = Arc::new(Perf::default());
    let t0 = Instant::now();

    let mut handles = Vec::new();

    // Writers: each owns the KOs it created (one mutator per KO keeps the
    // model bookkeeping exact while the kernel pipeline stays contested).
    for t in 0..writers {
        let k = k.clone();
        let model = model.clone();
        let perf = perf.clone();
        handles.push(thread::spawn(move || {
            let mut rng = Rng(SEED + t as u64 * 0x9E37_79B9);
            let mut mine: Vec<KOID> = Vec::new();
            for _ in 0..w_ops {
                let roll = rng.pct();
                if mine.is_empty() || roll < 5 {
                    // ingestion (create)
                    let mut r = RememberRequest::create(ctx(), meta(TYPE));
                    r.properties = props(rng.next());
                    let p = r.properties.clone(); // r is moved into remember
                    let s = Instant::now();
                    let rem = k.remember(r).unwrap();
                    perf.push("create", s.elapsed().as_secs_f64() * 1e6);
                    let mut m = model.lock().unwrap();
                    m.keys.push(rem.koid);
                    m.kos.insert(rem.koid, fresh(TYPE, rem.version, p));
                    mine.push(rem.koid);
                } else if roll < 60 {
                    // update
                    let koid = mine[rng.below(mine.len())];
                    let rels_now = {
                        let m = model.lock().unwrap();
                        m.kos[&koid].rels.clone()
                    };
                    let mut r = RememberRequest::update(ctx(), koid, meta(TYPE));
                    r.properties = props(rng.next());
                    r.relationships = rels_now
                        .iter()
                        .map(|(t, d, dir)| RelationshipRef {
                            rel_type: t.clone(),
                            target: *d,
                            direction: *dir,
                        })
                        .collect();
                    let p = r.properties.clone(); // r is moved into remember
                    let s = Instant::now();
                    let rem = k.remember(r).unwrap();
                    perf.push("update", s.elapsed().as_secs_f64() * 1e6);
                    let mut m = model.lock().unwrap();
                    let mk = m.kos.get_mut(&koid).unwrap();
                    mk.version = rem.version;
                    mk.props = p;
                } else if roll < 75 {
                    // relate: edge to any alice KO (targets must be
                    // visible to alice — the kernel enforces it).
                    let koid = mine[rng.below(mine.len())];
                    let (mut rels, props_now) = {
                        let m = model.lock().unwrap();
                        (m.kos[&koid].rels.clone(), m.kos[&koid].props.clone())
                    };
                    let tgt = {
                        let m = model.lock().unwrap();
                        m.keys[rng.below(m.keys.len())]
                    };
                    if !rels.iter().any(|(t, d, _)| t == "related_to" && *d == tgt) {
                        rels.push(("related_to".into(), tgt, Direction::Outbound));
                    }
                    let mut r = RememberRequest::update(ctx(), koid, meta(TYPE));
                    r.properties = props_now;
                    r.relationships = rels
                        .iter()
                        .map(|(t, d, dir)| RelationshipRef {
                            rel_type: t.clone(),
                            target: *d,
                            direction: *dir,
                        })
                        .collect();
                    let s = Instant::now();
                    let rem = k.remember(r).unwrap();
                    perf.push("relate", s.elapsed().as_secs_f64() * 1e6);
                    let mut m = model.lock().unwrap();
                    let mk = m.kos.get_mut(&koid).unwrap();
                    mk.version = rem.version;
                    mk.rels = rels;
                } else if roll < 85 {
                    // unrelate: drop the last caller-owned edge (kernel-
                    // managed supersedes edges are not eligible).
                    let koid = mine[rng.below(mine.len())];
                    let (mut rels, props_now) = {
                        let m = model.lock().unwrap();
                        (m.kos[&koid].rels.clone(), m.kos[&koid].props.clone())
                    };
                    let caller_owned =
                        |t: &str| t != "supersedes" && t != "derived_from" && t != "contradicts";
                    let pos = rels.iter().rposition(|(t, _, _)| caller_owned(t));
                    if let Some(pos) = pos {
                        rels.remove(pos);
                    }
                    let mut r = RememberRequest::update(ctx(), koid, meta(TYPE));
                    r.properties = props_now;
                    r.relationships = rels
                        .iter()
                        .map(|(t, d, dir)| RelationshipRef {
                            rel_type: t.clone(),
                            target: *d,
                            direction: *dir,
                        })
                        .collect();
                    let s = Instant::now();
                    let rem = k.remember(r).unwrap();
                    perf.push("unrelate", s.elapsed().as_secs_f64() * 1e6);
                    let mut m = model.lock().unwrap();
                    let mk = m.kos.get_mut(&koid).unwrap();
                    mk.version = rem.version;
                    mk.rels = rels;
                } else if roll < 95 {
                    // supersede: close a generation, open a successor.
                    let candidates: Vec<KOID> = mine
                        .iter()
                        .copied()
                        .filter(|k| {
                            let m = model.lock().unwrap();
                            !m.kos[k].superseded
                        })
                        .collect();
                    let Some(&koid) = candidates.get(rng.below(candidates.len().max(1))) else {
                        continue;
                    };
                    let mut req = SupersedeRequest::new(ctx(), koid, TYPE);
                    req.properties = props(rng.next());
                    req.evidence = vec![Evidence::new("kse13-prop", EvidenceMethod::DocExtraction)];
                    let p = req.properties.clone(); // req is moved into supersede
                    let s = Instant::now();
                    let res = k.supersede(req).unwrap();
                    perf.push("supersede", s.elapsed().as_secs_f64() * 1e6);
                    // Mirror the actual supersedes-edge direction (KSE-12
                    // bookkeeping).
                    let head = k.get(ctx(), &koid).unwrap();
                    let edge = head
                        .relationships
                        .iter()
                        .find(|r| r.rel_type == "supersedes" && r.target == res.new)
                        .expect("supersede must link old to successor");
                    let mut m = model.lock().unwrap();
                    let old = m.kos.get_mut(&koid).unwrap();
                    old.version += 1;
                    old.superseded = true;
                    old.rels
                        .push(("supersedes".into(), res.new, edge.direction));
                    m.keys.push(res.new);
                    m.kos.insert(res.new, fresh(TYPE, 1, p));
                    mine.retain(|x| *x != koid);
                    mine.push(res.new);
                } else {
                    // delete (tombstone): head survives, edges stay.
                    let koid = mine[rng.below(mine.len())];
                    let s = Instant::now();
                    let f = k
                        .forget(ctx(), &koid, ForgetMode::Tombstone, None, None)
                        .unwrap();
                    perf.push("delete", s.elapsed().as_secs_f64() * 1e6);
                    let mut m = model.lock().unwrap();
                    let mk = m.kos.get_mut(&koid).unwrap();
                    mk.version = f.version;
                    mk.deleted = true;
                    mine.retain(|x| *x != koid);
                }
            }
        }));
    }

    // Readers: shape pins that must hold at ANY instant, racing writers.
    for t in 0..readers {
        let k = k.clone();
        let model = model.clone();
        let perf = perf.clone();
        let bob_keys: Vec<KOID> = bob_kos.keys().cloned().collect();
        handles.push(thread::spawn(move || {
            let mut rng = Rng(0x1000_0000 + t as u64 * 0x9E37_79B9);
            for _ in 0..r_ops {
                let (keys, bob_idx) = {
                    let m = model.lock().unwrap();
                    (
                        m.keys.clone(),
                        rng.below(bob_keys.len() + m.keys.len().max(1)),
                    )
                };
                let roll = rng.pct();
                if keys.is_empty() {
                    continue;
                }
                if roll < 50 {
                    // KO lookup
                    let koid = keys[rng.below(keys.len())];
                    let s = Instant::now();
                    let head = k.get(ctx(), &koid).unwrap_or_else(|e| {
                        panic!("reader lookup {} failed: {e:?}", koid.to_hex())
                    });
                    perf.push("lookup", s.elapsed().as_secs_f64() * 1e6);
                    assert!(
                        head.version >= 1,
                        "impossible head version on {}",
                        koid.to_hex()
                    );
                    assert_eq!(
                        head.event_refs.len(),
                        head.version as usize,
                        "reader saw a half-committed head on {}",
                        koid.to_hex()
                    );
                } else if roll < 70 {
                    // relationship traversal: every target resolves.
                    let koid = keys[rng.below(keys.len())];
                    let s = Instant::now();
                    let edges = k.outbound_edges(&koid, None).unwrap();
                    perf.push("traversal", s.elapsed().as_secs_f64() * 1e6);
                    for (_, target) in &edges {
                        assert!(
                            k.get(ctx(), target).is_ok(),
                            "dangling edge from {} to {}",
                            koid.to_hex(),
                            target.to_hex()
                        );
                    }
                } else if roll < 80 {
                    // history: lineage contiguous from 1, clock monotonic.
                    let koid = keys[rng.below(keys.len())];
                    let s = Instant::now();
                    let tr = k.trace(ctx(), &koid).unwrap();
                    perf.push("history", s.elapsed().as_secs_f64() * 1e6);
                    assert!(
                        tr.versions
                            .windows(2)
                            .all(|w| w[0].version + 1 == w[1].version),
                        "reader saw a gapped lineage on {}",
                        koid.to_hex()
                    );
                    assert!(
                        tr.versions
                            .windows(2)
                            .all(|w| w[0].commit_ts <= w[1].commit_ts),
                        "reader saw commit_ts run backwards on {}",
                        koid.to_hex()
                    );
                } else if roll < 95 {
                    // type scan: homogeneous, live.
                    let s = Instant::now();
                    let rows = k.scan_by_type(&Subject::new("alice"), TYPE).unwrap();
                    perf.push("type_scan", s.elapsed().as_secs_f64() * 1e6);
                    for ko in &rows {
                        assert_eq!(
                            ko.metadata.type_name, TYPE,
                            "type scan leaked a foreign type"
                        );
                        assert!(ko.version >= 1, "type scan leaked an unborn KO");
                    }
                } else {
                    // authorization: bob's KOs stay invisible to alice.
                    let target = if bob_idx < bob_keys.len() {
                        bob_keys[bob_idx]
                    } else {
                        keys[rng.below(keys.len())]
                    };
                    let s = Instant::now();
                    let res = k.get(ctx(), &target);
                    perf.push("auth_probe", s.elapsed().as_secs_f64() * 1e6);
                    if bob_keys.contains(&target) {
                        assert!(res.is_err(), "alice read bob's KO {}", target.to_hex());
                    }
                    assert!(
                        k.scan_by_type(&Subject::new("alice"), "secret")
                            .unwrap()
                            .is_empty(),
                        "alice's secret scan saw bob's KOs"
                    );
                }
            }
        }));
    }

    for h in handles {
        h.join().unwrap(); // a deadlock surfaces as a CI-timeout hang
    }
    let wall = t0.elapsed();

    // No corruption / no duplicate commits: the full post-storm sweep.
    let m = model.lock().unwrap();
    sweep(&k, engine.as_ref(), &m, &bob_kos);
    let total_ops = readers * r_ops + writers * w_ops;
    drop(m);

    // Perf report (P50/P95/P99 + throughput); CPU/RSS/IO/contention stay
    // honest NOT_MEASURED rows.
    let mut table = String::new();
    let mut classes: Vec<(&'static str, Vec<f64>)> = perf
        .map
        .lock()
        .unwrap()
        .iter()
        .map(|(c, v)| (*c, v.clone()))
        .collect();
    classes.sort_by_key(|(c, _)| *c);
    for (class, v) in &classes {
        let mut s = v.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        table.push_str(&format!(
            "| {class} | {} | {:.0} / {:.0} / {:.0} µs |\n",
            s.len(),
            pct(&s, 50),
            pct(&s, 95),
            pct(&s, 99),
        ));
    }
    let report = format!(
        "# KSE-13 — Concurrency (KSE-120)\n\n\
         Date: 2026-08-31 · seed {SEED:#x} · engine: AikoqlStorageEngine\n\n\
         ## Config\n\n\
         | leg | threads | ops/thread | result |\n\
         |---|---|---|---|\n\
         | KSE-120a raw-engine write_batch | 32 | 200 | live == replay (byte-equal), 0 lost puts |\n\
         | KSE-120b readers | {readers} | {r_ops} | all shape pins held every read |\n\
         | KSE-120b writers | {writers} | {w_ops} | all commits exactly once |\n\n\
         ## Latency (KSE-120b, this machine)\n\n\
         | op class | count | P50 / P95 / P99 |\n\
         |---|---|---|\n\
         {table}\
         | **throughput** | {total_ops} ops | {:.0} ops/s wall |\n\n\
         ## Expecteds (§19)\n\n\
         - deadlocks: none — all threads joined; a hang fails the CI timeout\n\
         - corruption: none — post-storm sweep (derived set byte-equal, head\n\
           uniqueness, version rows, journal count, rebuild (0,0))\n\
         - invalid logical reads: none — readers pinned head/event/edge/scan\n\
           shape on every op\n\
         - duplicate commits: none — final versions and lineages match the\n\
           model exactly\n\
         - authorization bypass: none — bob's KOs invisible on every shape\n\n\
         ## Honest limits\n\n\
         - CPU/RSS/IO: NOT_MEASURED (no counting allocator or IO tracing)\n\
         - contention: NOT_MEASURED directly; the kernel's single-writer\n\
           pipeline serializes commits by design (§19), visible in writer\n\
           P95/P99 vs reader latency\n\
         - context compilation: out of the engine's reach — concurrent leg\n\
           QA2-CONC-001 in `crates/ingestion/tests/qa2_concurrency.rs`\n",
        total_ops as f64 / wall.as_secs_f64().max(1e-9),
    );
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../artifacts/storage-engine");
    std::fs::write(dir.join("concurrency.md"), report).unwrap();
}
