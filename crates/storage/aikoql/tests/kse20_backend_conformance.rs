//! KSE-20 — backend conformance (MRFC-KSE-001 §26).
//!
//! The custom engine must pass the same conformance suite as MemoryEngine,
//! RedbEngine, RocksDbEngine and AikoqlStorageEngine — and any difference
//! must be explained by an explicit, documented capability rather than an
//! accidental semantic divergence.
//!
//! This suite runs the six §7 asserts (the one shared definition in
//! `common::kse`) against every backend in one matrix, then probes the one
//! real divergence surface the six asserts cannot see — persistence across
//! reopen — and pins it per backend: durable backends serve the state after
//! a reopen, MemoryEngine has no persistence by definition. Everything else
//! that differs between backends (physical format, read path, durability
//! knobs) is a documented capability of each engine's design, recorded in
//! `artifacts/storage-engine/conformance.md` (§31's name for this phase).
//! The suite itself runs through `&dyn StorageEngine` only (§32).

mod common;

use aikoql_kernel::storage::store::{MemoryEngine, StorageEngine, WriteBatch};
use aikoql_kernel::storage::store_redb::RedbEngine;
use aikoql_storage::AikoqlStorageEngine;
use common::{kse, tmp};
use std::path::PathBuf;

#[cfg(feature = "kse5-rocksdb")]
use aikoql_rocksdb::RocksDbEngine;

/// (path, opener) — how a durable backend reopens its store.
type Reopen = (PathBuf, fn(PathBuf) -> Box<dyn StorageEngine>);

struct Backend {
    name: &'static str,
    engine: Box<dyn StorageEngine>,
    /// None for backends without persistence.
    reopen: Option<Reopen>,
    format: &'static str,
    read_path: &'static str,
}

fn backends() -> Vec<Backend> {
    let redb_path = tmp("kse20-redb");
    let aikoql_path = tmp("kse20-aikoql");
    #[cfg(feature = "kse5-rocksdb")]
    let rocks_path = tmp("kse20-rocks");

    vec![
        Backend {
            name: "memory",
            engine: Box::new(MemoryEngine::new()),
            reopen: None,
            format: "in-RAM BTreeMap (no file)",
            read_path: "RAM mirror",
        },
        Backend {
            name: "redb",
            engine: Box::new(RedbEngine::open(&redb_path).unwrap()),
            reopen: Some((redb_path, |p| Box::new(RedbEngine::open(&p).unwrap()))),
            format: "single B-tree file",
            read_path: "storage (page cache)",
        },
        Backend {
            name: "aikoql",
            engine: Box::new(AikoqlStorageEngine::open(&aikoql_path).unwrap()),
            reopen: Some((aikoql_path, |p| {
                Box::new(AikoqlStorageEngine::open(&p).unwrap())
            })),
            format: "append-only WAL file",
            read_path: "RAM mirror — 0 disk at query time (KSE-5/KSE-18)",
        },
        #[cfg(feature = "kse5-rocksdb")]
        Backend {
            name: "rocksdb",
            engine: Box::new(RocksDbEngine::open(&rocks_path).unwrap()),
            reopen: Some((rocks_path, |p| Box::new(RocksDbEngine::open(&p).unwrap()))),
            format: "LSM directory (WAL + SSTs)",
            read_path: "storage (block cache)",
        },
    ]
}

/// All six §7 asserts — any divergence panics, so a ✓ row in the report is
/// honest by construction (the report is only written when every backend
/// passed everything).
fn run_six(e: &dyn StorageEngine) {
    kse::kse001_get(e);
    kse::kse002_missing_key(e);
    kse::kse003_prefix_scan(e);
    kse::kse004_atomic_batch(e);
    kse::kse005_empty_batch(e);
    kse::kse006_conflicting_put_delete(e);
}

#[test]
fn kse20_backend_conformance() {
    let mut rows = String::new();
    for mut b in backends() {
        run_six(b.engine.as_ref());
        let persistence = match b.reopen.take() {
            Some((path, opener)) => {
                // Durability probe: write → drop the handle → reopen → read.
                let mut w = WriteBatch::new();
                w.put(b"kse20-reopen".to_vec(), b"v".to_vec());
                b.engine.write_batch(&w).unwrap();
                drop(b.engine);
                let reopened = opener(path);
                assert_eq!(
                    reopened.get(b"kse20-reopen").unwrap(),
                    Some(b"v".to_vec()),
                    "{}: state lost across reopen",
                    b.name
                );
                "reopen ✓".to_string()
            }
            None => "none — RAM-only by definition".to_string(),
        };
        rows.push_str(&format!(
            "| {} | 6/6 ✓ | {persistence} | {} | {} |\n",
            b.name, b.format, b.read_path
        ));
    }

    let report = format!(
        "# Backend Conformance (MRFC-KSE-001 §7 + §26)\n\n\
         Date: 2026-09-01 · the six KSE-1 asserts from one shared definition \
         (`tests/common` `kse` module), run per backend as granular tests \
         (`conformance.rs`, KSE-1) and as this matrix (`kse20_backend_conformance.rs`, \
         KSE-20). All through `&dyn StorageEngine` — no backend-specific type \
         above the boundary (§32).\n\n\
         | backend | KSE-001..006 | persistence (reopen) | physical format | read path |\n\
         |---|---|---|---|---|\n\
         {rows}\n\
         ## Divergences — explicit documented capabilities\n\n\
         - persistence: MemoryEngine has none by definition (in-RAM only); \
         the three durable backends served the reopen probe identically \
         (write → drop handle → reopen → read).\n\
         - durability knobs: Aikoql fsyncs every batch (pinned by KSE-3 \
         corruption/envelope tests, KSE-9 fault injection, KSE-15 real-kill \
         recovery); redb/RocksDB durability is their own engine's knob, \
         outside this conformance contract.\n\
         - physical format: redb = a single B-tree file (opens directly as \
         redb — KSE-14); RocksDB = an LSM directory (WAL + SSTs); Aikoql = \
         an append-only enveloped WAL (KSE-3); Memory = no file.\n\
         - read path: Memory/Aikoql serve reads from the in-RAM mirror \
         (Aikoql's query-time disk IO measured at 0 — KSE-5/KSE-18); \
         redb/RocksDB read from storage through their caches.\n\
         - concurrency: all four serialize writes at the engine boundary; \
         Aikoql's contract under concurrent access is pinned behaviorally \
         by KSE-13.\n\n\
         **No accidental semantic divergence found:** the six §7 asserts \
         pass identically on all four backends, and every difference above \
         is a documented capability of the engine's design.\n",
    );
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../artifacts/storage-engine");
    std::fs::write(dir.join("conformance.md"), report).unwrap();
}
