//! KSE-15 — startup and recovery (MRFC-KSE-001 §21, KSE-140..141).
//!
//! §21 asks for two measurements:
//!
//! * KSE-140 cold startup — open / metadata initialization / index
//!   initialization / ready. Measured as engine open (WAL replay) → kernel
//!   open (metadata init: type-index marker check + schema reload) → first
//!   query. The "index initialization" stage costs nothing here by
//!   construction: derived indexes (relo/reli/type) are store rows replayed
//!   with the WAL, not built at open — the one-time R9 backfill runs only
//!   for pre-R9 databases and is a no-op on a fresh marker.
//! * KSE-141 crash recovery — crash / restart / recovery / first successful
//!   query, recovery time. A REAL kill: a child process fsyncs ≥300 commits,
//!   signals via a marker file, keeps committing while the parent hard-kills
//!   it mid-write (TerminateProcess/SIGKILL — no cleanup runs). The parent
//!   then times engine reopen (replay incl. torn-tail truncation) → kernel
//!   open → first query. Pins: every durable commit recovered (seqs exactly
//!   1..=n contiguous — no lost or phantom middle commits), unique KOIDs,
//!   the seq-1 KO whole, the structural sweep, rebuild (0,0).
//!
//! The torn-tail truncation itself is additionally pinned deterministically
//! by KSE-9 (WAL fault injection); here the kill lands at an arbitrary
//! instant and replay must simply handle whatever was durable.

mod common;

use aikoql_kernel::storage::store::StorageEngine;
use aikoql_kernel::transaction::kernel::ManualClock;
use aikoql_kernel::{
    Kernel, KnowledgeObject, Metadata, PropertyMap, RememberRequest, Schema, Subject, Value, KOID,
};
use aikoql_storage::AikoqlStorageEngine;
use common::{ctx, structural_sweep, tmp};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SEED: u64 = 0x15_0000;
const TYPE: &str = "kse15_ko";
const CHILD_ENV: &str = "KSE141_CHILD";
const PATH_ENV: &str = "KSE141_PATH";
const MARKER_ENV: &str = "KSE141_MARKER";

// ---------------------------------------------------------------------------
// Small deterministic helpers (KSE-12/13/14 pattern).
// ---------------------------------------------------------------------------

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
    p.insert("subject".into(), Value::Text(format!("s{seq}")));
    p
}

fn open_kernel(engine: Arc<AikoqlStorageEngine>, label: &str) -> Arc<Kernel> {
    let clock = Arc::new(ManualClock::new(10_000));
    Arc::new(
        Kernel::open(engine, clock, SEED)
            .unwrap_or_else(|e| panic!("{label}: kernel open failed: {e:?}")),
    )
}

fn create(k: &Kernel, t: &str, seq: u64) -> KOID {
    let mut r = RememberRequest::create(ctx(), meta(t));
    r.properties = props(seq);
    k.remember(r).unwrap().koid
}

fn seq_of(ko: &KnowledgeObject) -> u64 {
    match ko.properties.get("seq") {
        Some(Value::Int(n)) => *n as u64,
        other => panic!("KO missing seq prop: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// KSE-140 — cold startup. The measurement runs twice per suite: once as the
// test, once for the report (kse13 pattern — the report always carries
// numbers from the same run, never a stale file).
// ---------------------------------------------------------------------------

struct Kse140 {
    replay_ms: f64,
    kernel_ms: f64,
    ready_ms: f64,
    wal_bytes: u64,
    version_rows: usize,
    events: usize,
}

fn measure_kse140(label: &str) -> Kse140 {
    const KOS: u64 = 2_000;

    // Build phase (warm): the dataset a cold open will replay — 2,000
    // creates, an update on every 10th (2,200 version rows total), one
    // schema row with a constraint (exercises the schema reload at open).
    let path = tmp(&format!("kse140-{label}"));
    let engine = Arc::new(AikoqlStorageEngine::open(&path).unwrap());
    let k = open_kernel(engine.clone(), &format!("kse140-build-{label}"));
    let mut ids: Vec<KOID> = Vec::new();
    for i in 1..=KOS {
        ids.push(create(&k, TYPE, i));
        if i % 10 == 0 {
            let koid = ids[(i - 1) as usize];
            let mut r = RememberRequest::update(ctx(), koid, meta(TYPE));
            r.properties = props(i);
            let _ = k.remember(r).unwrap();
        }
    }
    k.register_schema(Schema::new(TYPE, 1).required_property("subject", "Text"))
        .unwrap();
    let pre: BTreeMap<Vec<u8>, Vec<u8>> = engine.scan(b"").unwrap().into_iter().collect();
    drop(k);
    drop(engine);

    // Cold open: engine open (WAL replay) → kernel open (metadata init:
    // type-index marker check + schema reload) → ready (first query).
    let t0 = Instant::now();
    let cold = Arc::new(AikoqlStorageEngine::open(&path).unwrap());
    let replay_ms = t0.elapsed().as_secs_f64() * 1e3;

    let t1 = Instant::now();
    let k2 = open_kernel(cold.clone(), &format!("kse140-cold-{label}"));
    let kernel_ms = t1.elapsed().as_secs_f64() * 1e3;

    let t2 = Instant::now();
    let first = k2.get(ctx(), &ids[0]).unwrap();
    let ready_ms = t2.elapsed().as_secs_f64() * 1e3;

    // Pins: the cold open serves exactly what was served pre-close.
    let post: BTreeMap<Vec<u8>, Vec<u8>> = cold.scan(b"").unwrap().into_iter().collect();
    assert_eq!(
        post, pre,
        "kse140: cold open serves a different store than pre-close"
    );
    assert_eq!(
        first.version, 1,
        "kse140: first KO's head wrong after cold open"
    );
    assert_eq!(
        first.event_refs.len(),
        1,
        "kse140: first KO half-committed after cold open"
    );
    assert_eq!(
        k2.scan_by_type(&Subject::new("alice"), TYPE).unwrap().len() as u64,
        KOS,
        "kse140: type scan wrong after cold open"
    );
    // The 10th KO was updated once: its lineage survived the reopen.
    assert_eq!(
        k2.get(ctx(), &ids[9]).unwrap().version,
        2,
        "kse140: update lost across the cold open"
    );
    structural_sweep(&k2, cold.as_ref(), &format!("kse140-{label}"));

    let version_rows = cold.scan(b"ko/").unwrap().len();
    let events = cold.scan(b"ke/").unwrap().len();
    assert_eq!(
        (version_rows, events),
        ((KOS + KOS / 10) as usize, (KOS + KOS / 10) as usize),
        "kse140: unexpected dataset shape after cold open"
    );
    Kse140 {
        replay_ms,
        kernel_ms,
        ready_ms,
        wal_bytes: std::fs::metadata(&path).unwrap().len(),
        version_rows,
        events,
    }
}

#[test]
fn kse140_cold_startup_timing() {
    let _ = measure_kse140("test");
}

// ---------------------------------------------------------------------------
// KSE-141 — crash recovery. Child creates KOs with seq = i, fsyncs a marker
// after commit 300 (each commit fsyncs the WAL, so marker ⇒ ≥300 durable),
// keeps committing while the parent hard-kills it; the parent times reopen →
// kernel open → first query and pins that every durable commit survived.
// ---------------------------------------------------------------------------

const COMMITTED: u64 = 300;
const CAP: u64 = 2_000;

fn child_main() {
    let path = PathBuf::from(std::env::var(PATH_ENV).unwrap());
    let marker = PathBuf::from(std::env::var(MARKER_ENV).unwrap());
    let engine = Arc::new(AikoqlStorageEngine::open(&path).unwrap());
    let k = open_kernel(engine, "kse141-child");
    for i in 1..=CAP {
        let _ = create(&k, TYPE, i);
        if i == COMMITTED {
            // Durable BEFORE the marker: every write_batch fsyncs the WAL.
            let f = std::fs::File::create(&marker).unwrap();
            f.sync_all().unwrap();
        }
    }
    // If the parent never kills us we exit normally; the kill window is CAP
    // commits wide, so in practice the kill lands mid-loop.
}

struct Kse141 {
    replay_ms: f64,
    kernel_ms: f64,
    first_query_ms: f64,
    recovered: usize,
    wal_bytes: u64,
}

fn measure_kse141(label: &str) -> Kse141 {
    let path = tmp(&format!("kse141-{label}"));
    let marker = tmp(&format!("kse141-marker-{label}"));
    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(&exe)
        .arg("--exact")
        .arg("kse141_crash_recovery")
        .env(CHILD_ENV, "1")
        .env(PATH_ENV, &path)
        .env(MARKER_ENV, &marker)
        .spawn()
        .unwrap();

    // The marker lands after commit COMMITTED is durable. The child keeps
    // committing past it, so the kill lands at an arbitrary instant —
    // usually mid-loop, possibly mid-append.
    let deadline = Instant::now() + Duration::from_secs(120);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "kse141: child never wrote the committed marker"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // Real kill: no cleanup, no graceful shutdown.
    let _ = child.kill();
    let _ = child.wait();

    // Recovery: engine reopen (replay + torn-tail truncation) → kernel open
    // → first successful query.
    let t0 = Instant::now();
    let engine = Arc::new(AikoqlStorageEngine::open(&path).unwrap());
    let replay_ms = t0.elapsed().as_secs_f64() * 1e3;

    let t1 = Instant::now();
    let k = open_kernel(engine.clone(), &format!("kse141-recover-{label}"));
    let kernel_ms = t1.elapsed().as_secs_f64() * 1e3;

    let t2 = Instant::now();
    let rows = k.scan_by_type(&Subject::new("alice"), TYPE).unwrap();
    let first_query_ms = t2.elapsed().as_secs_f64() * 1e3;

    // Pins: no lost or phantom middle commits (durable seqs contiguous from
    // 1 — append-only WAL + replay can only drop a torn tail, never a
    // middle record).
    assert!(
        rows.len() as u64 >= COMMITTED,
        "kse141: recovered {} KOs, expected >= {COMMITTED}",
        rows.len()
    );
    let mut seqs: Vec<u64> = rows.iter().map(seq_of).collect();
    seqs.sort_unstable();
    assert_eq!(
        seqs,
        (1..=seqs.len() as u64).collect::<Vec<_>>(),
        "kse141: recovered seqs not exactly 1..=n — lost or phantom commits"
    );
    let koids: BTreeSet<KOID> = rows.iter().map(|ko| ko.koid).collect();
    assert_eq!(
        koids.len(),
        rows.len(),
        "kse141: duplicate KOIDs in the type scan"
    );
    // The seq-1 KO is whole: one version, one event.
    let seq1 = rows
        .iter()
        .find(|ko| seq_of(ko) == 1)
        .expect("kse141: seq-1 KO missing");
    let head1 = k.get(ctx(), &seq1.koid).unwrap();
    assert_eq!(
        head1.version, 1,
        "kse141: seq-1 KO version != 1 after recovery"
    );
    assert_eq!(
        head1.event_refs.len(),
        1,
        "kse141: seq-1 KO half-committed after recovery"
    );
    structural_sweep(&k, engine.as_ref(), &format!("kse141-{label}"));

    let version_rows = engine.scan(b"ko/").unwrap().len();
    let events = engine.scan(b"ke/").unwrap().len();
    assert_eq!(
        (version_rows, events),
        (rows.len(), rows.len()),
        "kse141: version rows / events != recovered KOs"
    );
    Kse141 {
        replay_ms,
        kernel_ms,
        first_query_ms,
        recovered: rows.len(),
        wal_bytes: std::fs::metadata(&path).unwrap().len(),
    }
}

#[test]
fn kse141_crash_recovery() {
    if std::env::var(CHILD_ENV).is_ok() {
        child_main();
        return;
    }
    let _ = measure_kse141("test");
}

// ---------------------------------------------------------------------------
// Report: artifacts/storage-engine/crash-recovery.md
// ---------------------------------------------------------------------------

#[test]
fn kse15_report() {
    let a = measure_kse140("report");
    let b = measure_kse141("report");
    let report = format!(
        "# KSE-15 — Startup and Recovery (KSE-140..141)\n\n\
         Date: 2026-08-31 · seed {SEED:#x} · engine: AikoqlStorageEngine · \
         debug build, measurements from this suite run\n\n\
         ## KSE-140 — cold startup (2,000 KOs + 200 updates + 1 schema; \
         {} version rows, {} journal events, {:.0} B WAL)\n\n\
         | stage | time |\n\
         |---|---:|\n\
         | open (WAL replay) | {:.2} ms ({:.1} µs/row) |\n\
         | metadata initialization (kernel open: type-index marker check + \
         schema reload) | {:.2} ms |\n\
         | index initialization | 0 — derived indexes (relo/reli/type) are \
         WAL rows, replayed with open; nothing is built at startup |\n\
         | ready (first query) | {:.1} µs |\n\n\
         ## KSE-141 — crash recovery (real kill after ≥300 durable commits; \
         recovered {}, {:.0} B WAL at recovery)\n\n\
         | stage | time |\n\
         |---|---:|\n\
         | crash → restart | process kill, instant |\n\
         | recovery (open: WAL replay + torn-tail truncation) | {:.2} ms |\n\
         | kernel open | {:.2} ms |\n\
         | first successful query | {:.1} µs |\n\n\
         ## Pins\n\n\
         - KSE-140: cold open serves the byte-exact pre-close store; type \
         scan == 2,000; the updated KO kept its 2-version lineage; structural \
         sweep + rebuild (0,0)\n\
         - KSE-141: recovered seqs exactly 1..=n (no lost or phantom middle \
         commits — append-only replay can only drop a torn tail); KOIDs \
         unique; seq-1 KO whole (version 1, one event); structural sweep + \
         rebuild (0,0)\n\n\
         ## Honest limits\n\n\
         - \"index initialization\" has no cost for this engine by \
         construction — derived indexes are store rows; the only startup \
         index work is the one-time R9 backfill for pre-R9 databases, a \
         no-op on a fresh marker and not part of this measurement\n\
         - the kill lands at an arbitrary instant — the torn-tail truncation \
         path is pinned deterministically by KSE-9 (fault injection); here \
         the replay handled a real kill\n\
         - the two first-query numbers are not directly comparable: \
         KSE-141's is the full type scan (all recovered KOs decoded), \
         KSE-140's is a point get\n\
         - child is single-writer (the kernel pipeline is single-writer by \
         design)\n",
        a.version_rows,
        a.events,
        a.wal_bytes,
        a.replay_ms,
        a.replay_ms / a.version_rows as f64 * 1e3,
        a.kernel_ms,
        a.ready_ms * 1e3,
        b.recovered,
        b.wal_bytes,
        b.replay_ms,
        b.kernel_ms,
        b.first_query_ms * 1e3,
    );
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../artifacts/storage-engine");
    std::fs::write(dir.join("crash-recovery.md"), report).unwrap();
}
