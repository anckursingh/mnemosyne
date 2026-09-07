//! Storage Engine abstraction (review §4.2: build-vs-use decision).
//!
//! The kernel depends on this trait ONLY — never on a concrete engine.
//! Increment 1 ships `MemoryEngine` (deterministic, drives the conformance suite).
//! Increment 2 adds durable backends (`redb` / RocksDB) implementing the same
//! contract; the conformance suite runs unchanged against every backend.
//!
//! Contract:
//! - `write_batch` MUST be atomic (all-or-nothing) — the commit pipeline's
//!   KO-version + Knowledge-Event + journal-head commit depends on it (MRFC-0008).
//! - `scan` MUST return entries sorted by key, restricted to `prefix`.
//! - Engines MUST be `Send + Sync` and safe for concurrent readers.

use crate::knowledge::kom::{KError, KResult};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::RwLock;

/// An atomic unit of work against the engine.
#[derive(Default, Debug)]
pub struct WriteBatch {
    pub puts: Vec<(Vec<u8>, Vec<u8>)>,
    pub dels: Vec<Vec<u8>>,
}

impl WriteBatch {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn put(&mut self, key: Vec<u8>, val: Vec<u8>) {
        self.puts.push((key, val));
    }
    pub fn del(&mut self, key: Vec<u8>) {
        self.dels.push(key);
    }
    pub fn is_empty(&self) -> bool {
        self.puts.is_empty() && self.dels.is_empty()
    }
}

/// Backend-native constraint support (MRFC-0060 Phase C7).
///
/// A backend declares `true` ONLY for constraints it enforces natively with
/// semantics equivalent to the kernel evaluator; the kernel then skips its
/// in-process check for that class. All-false (the default) = kernel
/// enforcement — current behavior of every backend.
///
/// `check` covers domain + check constraints. `unique` covers uniqueness.
/// `not_null` covers required + nullable property checks.
/// `foreign_key` is omitted — no FK constraint exists in the KOM today.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConstraintCapabilities {
    pub unique: bool,
    pub check: bool,
    pub not_null: bool,
}

pub trait StorageEngine: Send + Sync {
    fn get(&self, key: &[u8]) -> KResult<Option<Vec<u8>>>;
    /// Point lookups for a batch of keys, answers parallel to the input.
    ///
    /// Default loops `get` — one lock/fetch per key. Engines whose per-call
    /// overhead dominates (state locks, block fetches) override with a
    /// batched implementation. Encrypting wrappers inherit the default and
    /// route through their own transformed `get`.
    fn get_many(&self, keys: &[&[u8]]) -> KResult<Vec<Option<Vec<u8>>>> {
        keys.iter().map(|k| self.get(k)).collect()
    }
    /// Prefix scan, sorted ascending by key.
    ///
    /// Implementations MUST seek directly to the prefix range (O(log n) +
    /// O(prefix-range)) rather than iterating from the beginning of the key
    /// space (O(all-keys)). Backends that cannot support this (e.g. external
    /// stores without native prefix iteration) must document the limitation.
    fn scan(&self, prefix: &[u8]) -> KResult<Vec<(Vec<u8>, Vec<u8>)>>;
    /// Atomically apply the batch (all-or-nothing).
    fn write_batch(&self, batch: &WriteBatch) -> KResult<()>;
    /// Native constraint support declared by this backend.
    /// Default: none — kernel enforces all constraints in-process.
    fn constraint_capabilities(&self) -> ConstraintCapabilities {
        ConstraintCapabilities::default()
    }
    /// REC-002: copy every row into a fresh durable database at `dest`.
    ///
    /// Reads through this engine's own handle — the live file may be
    /// region-locked (Windows redb), so a file-level copy cannot work.
    /// Default copies via scan/write_batch; encrypting wrappers override to
    /// delegate to their inner engine so ciphertext moves verbatim.
    /// QA2-PROP-001: the copy is FAITHFUL — `dest` equals the source
    /// afterward. Rows that survived in `dest` from a previous snapshot are
    /// deleted (reusing a snapshot path must replace, never merge states —
    /// a merge resurrects deleted versions on restore). Symmetric with
    /// `restore_from`.
    /// ponytail: O(n) full scan, no streaming; fine at MVP store sizes.
    fn snapshot_to(&self, dest: &Path) -> KResult<()> {
        let out = crate::storage::store_redb::RedbEngine::open(dest)?;
        let rows = self.scan(b"")?;
        let src_keys: std::collections::HashSet<Vec<u8>> =
            rows.iter().map(|(k, _)| k.clone()).collect();
        let mut batch = WriteBatch::new();
        // puts apply before dels, so keys present in the snapshot must not
        // also be deleted — only stale dest keys are.
        for (k, _) in out.scan(b"")? {
            if !src_keys.contains(&k) {
                batch.del(k);
            }
        }
        for (k, v) in rows {
            batch.put(k, v);
        }
        out.write_batch(&batch)
    }
    /// REC-002: replace this engine's contents with the database at `src`
    /// (point-in-time restore) in one atomic write batch. Rows are copied
    /// verbatim — encrypting wrappers delegate to their inner engine so
    /// already-encrypted rows are never re-encrypted.
    /// ponytail: O(n) full scan + single batch; readers see old-or-new, never
    /// a mix.
    fn restore_from(&self, src: &Path) -> KResult<()> {
        if !src.is_file() {
            return Err(KError::Store(format!(
                "restore source is not a file: {}",
                src.display()
            )));
        }
        let snapshot = crate::storage::store_redb::RedbEngine::open(src)?;
        let rows = snapshot.scan(b"")?;
        let mut batch = WriteBatch::new();
        // write_batch applies puts before dels, so keys present in the
        // snapshot must not also be deleted — only live keys absent from the
        // snapshot are.
        let src_keys: std::collections::HashSet<Vec<u8>> =
            rows.iter().map(|(k, _)| k.clone()).collect();
        for (k, _) in self.scan(b"")? {
            if !src_keys.contains(&k) {
                batch.del(k);
            }
        }
        for (k, v) in rows {
            batch.put(k, v);
        }
        self.write_batch(&batch)
    }
}

/// In-memory engine: deterministic, fast, and the reference implementation
/// for the conformance suite. NOT durable — see milestone doc for the
/// durability roadmap.
pub struct MemoryEngine {
    map: RwLock<BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl MemoryEngine {
    pub fn new() -> Self {
        MemoryEngine {
            map: RwLock::new(BTreeMap::new()),
        }
    }

    /// Test/debug helper: number of live keys.
    pub fn len(&self) -> usize {
        self.map.read().map(|m| m.len()).unwrap_or(0)
    }
}

impl Default for MemoryEngine {
    fn default() -> Self {
        Self::new()
    }
}

fn poisoned() -> KError {
    KError::Store("engine lock poisoned".into())
}

impl StorageEngine for MemoryEngine {
    fn get(&self, key: &[u8]) -> KResult<Option<Vec<u8>>> {
        let m = self.map.read().map_err(|_| poisoned())?;
        Ok(m.get(key).cloned())
    }

    fn scan(&self, prefix: &[u8]) -> KResult<Vec<(Vec<u8>, Vec<u8>)>> {
        let m = self.map.read().map_err(|_| poisoned())?;
        Ok(m.range(prefix.to_vec()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }

    fn write_batch(&self, batch: &WriteBatch) -> KResult<()> {
        // Single write lock => atomic application; no failure modes mid-apply.
        let mut m = self.map.write().map_err(|_| poisoned())?;
        for (k, v) in &batch.puts {
            m.insert(k.clone(), v.clone());
        }
        for k in &batch.dels {
            m.remove(k);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_is_atomic_and_ordered() {
        let e = MemoryEngine::new();
        let mut b = WriteBatch::new();
        b.put(b"a/1".to_vec(), vec![1]);
        b.put(b"a/2".to_vec(), vec![2]);
        b.put(b"b/1".to_vec(), vec![3]);
        e.write_batch(&b).unwrap();
        assert_eq!(e.len(), 3);

        let mut b2 = WriteBatch::new();
        b2.del(b"a/1".to_vec());
        b2.put(b"a/3".to_vec(), vec![9]);
        e.write_batch(&b2).unwrap();

        assert_eq!(e.get(b"a/1").unwrap(), None);
        assert_eq!(e.get(b"a/2").unwrap(), Some(vec![2]));
        assert_eq!(e.get(b"a/3").unwrap(), Some(vec![9]));
    }

    #[test]
    fn scan_is_prefix_limited_and_sorted() {
        let e = MemoryEngine::new();
        let mut b = WriteBatch::new();
        for k in [&b"p/c"[..], &b"p/a"[..], &b"p/b"[..], &b"q/a"[..]] {
            b.put(k.to_vec(), vec![0]);
        }
        e.write_batch(&b).unwrap();
        let got: Vec<Vec<u8>> = e.scan(b"p/").unwrap().into_iter().map(|(k, _)| k).collect();
        assert_eq!(got, vec![b"p/a".to_vec(), b"p/b".to_vec(), b"p/c".to_vec()]);
    }

    #[test]
    fn get_missing_returns_none() {
        let e = MemoryEngine::new();
        assert_eq!(e.get(b"nope").unwrap(), None);
    }
}
