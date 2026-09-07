//! V2-Adopt — KSE-20 backend conformance (MRFC-KSE-001 §26).
//!
//! The v2 engine must pass the same conformance suite as MemoryEngine,
//! RedbEngine and AikoqlStorageEngine — and any difference must be
//! explained by an explicit, documented capability rather than an
//! accidental semantic divergence. Mirrors v1's
//! `kse20_backend_conformance.rs`: the six §7 asserts (the one shared
//! definition in `common::kse`, copied verbatim from v1's harness) run
//! against every backend in one matrix, then the one real divergence
//! surface the six asserts cannot see — persistence across reopen — is
//! pinned per backend: durable backends serve the state after a reopen,
//! MemoryEngine has no persistence by definition. Everything else that
//! differs between backends (physical format, read path, durability
//! knobs) is a documented capability of each engine's design, recorded
//! in `artifacts/storage-engine-v2/conformance.md`. The suite itself runs
//! through `&dyn StorageEngine` only (§32).

mod common;

use aikoql_kernel::storage::store::{MemoryEngine, StorageEngine, WriteBatch};
use aikoql_kernel::storage::store_redb::RedbEngine;
use aikoql_storage::AikoqlStorageEngine;
use aikoql_storage_v2::AikoqlStorageEngineV2;
use common::{kse, run_date, tmp};
use std::path::PathBuf;

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
    let redb_path = tmp("kse20v2-redb");
    let aikoql_path = tmp("kse20v2-aikoql");
    let v2_path = tmp("kse20v2-v2");

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
        Backend {
            name: "aikoql-v2",
            engine: Box::new(AikoqlStorageEngineV2::open(&v2_path).unwrap()),
            reopen: Some((v2_path, |p| {
                Box::new(AikoqlStorageEngineV2::open(&p).unwrap())
            })),
            format: "bounded WAL + immutable segments + manifest (dir)",
            read_path: "memtable + segment readers (bloom-skipped, block-cached)",
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
fn kse20_backend_conformance_v2() {
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

    let date = run_date();
    let report = format!(
        "# Backend Conformance — v2 (MRFC-KSE-001 §7 + §26)\n\n\
         Date: {date} · the six KSE-1 asserts from one shared definition \
         (`tests/common` `kse` module, copied verbatim from v1's harness), run \
         per backend as granular tests (`tests/engine.rs`, V2-Adopt) and as \
         this matrix (`kse20_backend_conformance.rs`, KSE-20). All through \
         `&dyn StorageEngine` — no backend-specific type above the boundary \
         (§32).\n\n\
         | backend | KSE-001..006 | persistence (reopen) | physical format | read path |\n\
         |---|---|---|---|---|\n\
         {rows}\n\
         ## Divergences — explicit documented capabilities\n\n\
         - persistence: MemoryEngine has none by definition (in-RAM only); \
         the three durable backends served the reopen probe identically \
         (write → drop handle → reopen → read).\n\
         - durability knobs: aikoql-v2 fsyncs every Sync batch (pinned by \
         the SE2-M2 WAL goldens, the M3/M4/M6 child-kill recovery suites); \
         redb/RocksDB durability is their own engine's knob; v1 aikoql \
         fsyncs every batch (KSE-9/KSE-15).\n\
         - physical format: redb = a single B-tree file (opens directly as \
         redb — KSE-14); aikoql = an append-only enveloped WAL (KSE-3); \
         aikoql-v2 = a database directory — bounded WAL, immutable \
         segments, manifest/CURRENT (SE2-M0..M5); Memory = no file.\n\
         - read path: Memory/aikoql serve reads from the in-RAM mirror; \
         aikoql-v2 reads the memtable and, per segment, seeks by index, \
         skips via the bloom pre-check, and caches decoded blocks within \
         `cache_bytes` (SE2-M7); redb/RocksDB read from storage through \
         their caches.\n\
         - concurrency: all four serialize writes at the engine boundary; \
         aikoql-v2 additionally offers GroupCommit mode (committer thread, \
         one fsync per group — SE2-M6) behind the same Sync baseline.\n\n\
         **No accidental semantic divergence found:** the six §7 asserts \
         pass identically on all four backends, and every difference above \
         is a documented capability of the engine's design.\n",
    );
    let dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../artifacts/storage-engine-v2");
    std::fs::write(dir.join("conformance.md"), report).unwrap();
}
