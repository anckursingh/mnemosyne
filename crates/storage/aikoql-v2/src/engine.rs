//! V2-Adopt — the kernel `StorageEngine` adapter over the v2 Db.
//!
//! PR#2 review SE-05: the adapter holds the Db DIRECTLY (no outer
//! RwLock). The Db owns its synchronization — Sync/Async writes hold the
//! state lock across seq → append → sync → apply (frame order == seq
//! order), GroupCommit mode routes through the committer thread, and reads
//! share via the state read-lock. An adapter-side RwLock would serialize
//! write_batch calls BEFORE they reach the queue and defeat group commit
//! (every group of one). Defaults are the v2 defaults (Sync durability,
//! 64 MiB memtable, 8 MiB block cache) — one Config knob away for a caller
//! that wants others (V2-Adopt gate: memory limits configurable). Writes go
//! through `Db::write` — one frame per batch, durable before the ack,
//! all-or-nothing by construction (M2). An empty batch is a no-op
//! (KSE-005 — `Db::write` rejects empty frames, so the adapter never
//! forwards one). REC-002 snapshot/restore ride the trait defaults (full
//! scan + redb snapshot) — the adapter needs no override.

use crate::db::{Config, Db};
use crate::stats::ReadPathStats;
use crate::wal::Op;
use aikoql_kernel::knowledge::kom::{KError, KResult};
use aikoql_kernel::storage::store::{StorageEngine, WriteBatch};
use std::path::Path;

fn se(e: impl std::fmt::Display) -> KError {
    KError::Store(format!("aikoql-v2: {e}"))
}

/// AIKOQL v2 engine: bounded WAL → memtable → immutable segments, served
/// through the kernel's storage contract. NOT the production default —
/// the V2-Adopt gate (KSE-20 conformance + §26 matrix) decides that.
pub struct AikoqlStorageEngineV2 {
    db: Db,
}

impl AikoqlStorageEngineV2 {
    /// Open (or create) a durable database at `path` with the v2 defaults.
    pub fn open(path: impl AsRef<Path>) -> KResult<Self> {
        let db = Db::open(Config::new(path.as_ref().to_path_buf())).map_err(se)?;
        Ok(AikoqlStorageEngineV2 { db })
    }

    /// Open with an explicit Config — the memory-limit knobs (memtable
    /// bytes, block cache bytes) live here (§26: configurable memory).
    pub fn open_with_config(config: Config) -> KResult<Self> {
        let db = Db::open(config).map_err(se)?;
        Ok(AikoqlStorageEngineV2 { db })
    }

    /// SE2-M21 — the Db's cumulative read-path counters, reachable through
    /// the adapter: the attribution probe measures the kernel leg (kernel
    /// op → engine gets) against the engine leg (this engine's gets) on
    /// one dataset.
    pub fn read_path_stats(&self) -> KResult<ReadPathStats> {
        Ok(self.db.read_path_stats())
    }

    /// SE-05 regression hook: commit fsyncs so far — the grouping evidence
    /// (fsyncs strictly below batch count proves the adapter no longer
    /// serializes writers before the commit queue).
    pub fn fsync_count(&self) -> u64 {
        self.db.fsync_count()
    }
}

impl StorageEngine for AikoqlStorageEngineV2 {
    fn get(&self, key: &[u8]) -> KResult<Option<Vec<u8>>> {
        self.db.get(key).map_err(se)
    }

    fn get_many(&self, keys: &[&[u8]]) -> KResult<Vec<Option<Vec<u8>>>> {
        self.db.get_many(keys).map_err(se)
    }

    fn scan(&self, prefix: &[u8]) -> KResult<Vec<(Vec<u8>, Vec<u8>)>> {
        self.db.scan(prefix).map_err(se)
    }

    fn write_batch(&self, batch: &WriteBatch) -> KResult<()> {
        if batch.is_empty() {
            return Ok(()); // KSE-005: no state change
        }
        // Puts before dels — the shared contract order (KSE-006).
        let mut ops = Vec::with_capacity(batch.puts.len() + batch.dels.len());
        for (k, v) in &batch.puts {
            ops.push(Op::Put(k.clone(), v.clone()));
        }
        for k in &batch.dels {
            ops.push(Op::Delete(k.clone()));
        }
        self.db.write(&ops).map_err(se)?;
        Ok(())
    }
}
