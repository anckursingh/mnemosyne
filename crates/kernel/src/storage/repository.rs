//! Knowledge Repository — the kernel's storage boundary.
//!
//! Owns the persisted key schema and all low-level encode/decode details so the
//! transaction orchestrator in `transaction::kernel` does not know how keys are
//! laid out.
//!
//! ## Key layout (R6 remediation)
//!
//! All persistent state lives in a flat keyspace with namespace prefixes.
//! BTree `range()` scans are naturally prefix-bounded — O(log N + matches).
//!
//! | Prefix    | Key format                          | Purpose                  |
//! |-----------|-------------------------------------|--------------------------|
//! | `ko/`     | `ko/<koid(16)>/<ts(8)>`            | Object versions (MVCC)   |
//! | `head/`   | `head/<koid(16)>`                   | Current head pointer     |
//! | `ke/`     | `ke/<seq(8)>`                       | Knowledge event journal  |
//! | `tomb/`   | `tomb/<koid(16)>`                   | Tombstone markers        |
//! | `idem/`   | `idem/<key>`                        | Idempotency dedup        |
//! | `sub/`    | `sub/<sub_id>`                      | Event subscriptions      |
//! | `relo/`   | `relo/<src(16)>/<rel>/<dst(16)>`   | Outbound relationship idx|
//! | `reli/`   | `reli/<dst(16)>/<rel>/<src(16)>`   | Inbound relationship idx |
//! | `type/`   | `type/<type_name>/<koid(16)>`      | Type-scoped secondary idx|
//! | `meta/`   | `meta/journal`                      | Journal head counter     |
//! | `meta/`   | `meta/type_index`                   | Type-index backfill marker|
//!
//! R9: `scan_by_type` walks `type/<type_name>/` instead of the whole `head/`
//! space (O(log N + per-type) instead of O(N)). The index is a candidate set —
//! readers still verify the payload's `type_name` so stale entries from type
//! changes are harmless.

use crate::knowledge::codec::{self, Dec, Enc};
use crate::knowledge::kom::*;
use crate::knowledge::notify::{EventFilter, SubscriptionRecord};
use crate::storage::cache::KnowledgeCache;
use crate::storage::store::{StorageEngine, WriteBatch};
use std::collections::HashSet;
use std::sync::Arc;

const P_OBJ: &[u8] = b"ko/";
const P_HEAD: &[u8] = b"head/";
const P_KE: &[u8] = b"ke/";
const P_TOMB: &[u8] = b"tomb/";
const P_IDEM: &[u8] = b"idem/";
const P_SUB: &[u8] = b"sub/";
const P_REL_OUT: &[u8] = b"relo/";
const P_REL_IN: &[u8] = b"reli/";
const P_TYPE: &[u8] = b"type/";
const K_JOURNAL: &[u8] = b"meta/journal";
const K_TYPE_INDEX: &[u8] = b"meta/type_index";

fn obj_key(k: &KOID, commit_ts: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(3 + KOID_LEN + 8);
    v.extend_from_slice(P_OBJ);
    v.extend_from_slice(k.as_bytes());
    v.extend_from_slice(&commit_ts.to_be_bytes());
    v
}
fn obj_prefix(k: &KOID) -> Vec<u8> {
    let mut v = Vec::with_capacity(3 + KOID_LEN);
    v.extend_from_slice(P_OBJ);
    v.extend_from_slice(k.as_bytes());
    v
}
fn head_key(k: &KOID) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + KOID_LEN);
    v.extend_from_slice(P_HEAD);
    v.extend_from_slice(k.as_bytes());
    v
}
fn ke_key(seq: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(3 + 8);
    v.extend_from_slice(P_KE);
    v.extend_from_slice(&seq.to_be_bytes());
    v
}
fn tomb_key(k: &KOID) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + KOID_LEN);
    v.extend_from_slice(P_TOMB);
    v.extend_from_slice(k.as_bytes());
    v
}
fn idem_key(k: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + k.len());
    v.extend_from_slice(P_IDEM);
    v.extend_from_slice(k.as_bytes());
    v
}
fn sub_key(id: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + id.len());
    v.extend_from_slice(P_SUB);
    v.extend_from_slice(id.as_bytes());
    v
}

// ---- relationship index keys ----
fn rel_out_key(src: &KOID, rel_type: &str, dst: &KOID) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + KOID_LEN + 1 + rel_type.len() + 1 + KOID_LEN);
    v.extend_from_slice(P_REL_OUT);
    v.extend_from_slice(src.as_bytes());
    v.push(b'/');
    v.extend_from_slice(rel_type.as_bytes());
    v.push(b'/');
    v.extend_from_slice(dst.as_bytes());
    v
}
fn rel_out_prefix_src(src: &KOID) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + KOID_LEN + 1);
    v.extend_from_slice(P_REL_OUT);
    v.extend_from_slice(src.as_bytes());
    v.push(b'/');
    v
}
fn rel_out_prefix_src_type(src: &KOID, rel_type: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + KOID_LEN + 1 + rel_type.len() + 1);
    v.extend_from_slice(P_REL_OUT);
    v.extend_from_slice(src.as_bytes());
    v.push(b'/');
    v.extend_from_slice(rel_type.as_bytes());
    v.push(b'/');
    v
}
fn rel_in_key(dst: &KOID, rel_type: &str, src: &KOID) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + KOID_LEN + 1 + rel_type.len() + 1 + KOID_LEN);
    v.extend_from_slice(P_REL_IN);
    v.extend_from_slice(dst.as_bytes());
    v.push(b'/');
    v.extend_from_slice(rel_type.as_bytes());
    v.push(b'/');
    v.extend_from_slice(src.as_bytes());
    v
}
fn rel_in_prefix_dst(dst: &KOID) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + KOID_LEN + 1);
    v.extend_from_slice(P_REL_IN);
    v.extend_from_slice(dst.as_bytes());
    v.push(b'/');
    v
}
fn rel_in_prefix_dst_type(dst: &KOID, rel_type: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + KOID_LEN + 1 + rel_type.len() + 1);
    v.extend_from_slice(P_REL_IN);
    v.extend_from_slice(dst.as_bytes());
    v.push(b'/');
    v.extend_from_slice(rel_type.as_bytes());
    v.push(b'/');
    v
}
fn type_key(type_name: &str, koid: &KOID) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + type_name.len() + 1 + KOID_LEN);
    v.extend_from_slice(P_TYPE);
    v.extend_from_slice(type_name.as_bytes());
    v.push(b'/');
    v.extend_from_slice(koid.as_bytes());
    v
}
fn type_prefix(type_name: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + type_name.len() + 1);
    v.extend_from_slice(P_TYPE);
    v.extend_from_slice(type_name.as_bytes());
    v.push(b'/');
    v
}
fn decode_rel_out_key(key: &[u8]) -> KResult<(KOID, String, KOID)> {
    if key.len() < 5 + KOID_LEN + 1 + 1 + KOID_LEN {
        return Err(KError::Codec("rel out key too short".into()));
    }
    // justified: length guarded above — slice is exactly KOID_LEN bytes, try_into cannot fail
    let src = KOID::from_bytes(key[5..5 + KOID_LEN].try_into().unwrap());
    let tail = &key[5 + KOID_LEN + 1..];
    let split = tail.len() - KOID_LEN - 1;
    let rel_type = String::from_utf8(tail[..split].to_vec())
        .map_err(|_| KError::Codec("rel out key bad utf-8".into()))?;
    // justified: length guarded above — slice is exactly KOID_LEN bytes, try_into cannot fail
    let dst = KOID::from_bytes(tail[split + 1..].try_into().unwrap());
    Ok((src, rel_type, dst))
}
fn decode_rel_in_key(key: &[u8]) -> KResult<(KOID, String, KOID)> {
    if key.len() < 5 + KOID_LEN + 1 + 1 + KOID_LEN {
        return Err(KError::Codec("rel in key too short".into()));
    }
    // justified: length guarded above — slice is exactly KOID_LEN bytes, try_into cannot fail
    let dst = KOID::from_bytes(key[5..5 + KOID_LEN].try_into().unwrap());
    let tail = &key[5 + KOID_LEN + 1..];
    let split = tail.len() - KOID_LEN - 1;
    let rel_type = String::from_utf8(tail[..split].to_vec())
        .map_err(|_| KError::Codec("rel in key bad utf-8".into()))?;
    // justified: length guarded above — slice is exactly KOID_LEN bytes, try_into cannot fail
    let src = KOID::from_bytes(tail[split + 1..].try_into().unwrap());
    Ok((dst, rel_type, src))
}

fn encode_head(version: u64, commit_ts: u64, state: LifecycleState) -> Vec<u8> {
    let mut e = Enc::new();
    e.u64(version);
    e.u64(commit_ts);
    e.u8(state.tag());
    e.buf
}
fn decode_head(b: &[u8]) -> KResult<(u64, u64, LifecycleState)> {
    let mut d = Dec::new(b);
    let v = d.u64()?;
    let ts = d.u64()?;
    let st = d.u8()?;
    let st = LifecycleState::from_tag(st).ok_or_else(|| KError::Codec("bad head state".into()))?;
    d.finish()?;
    Ok((v, ts, st))
}
fn encode_journal(seq: u64, audit: [u8; 32], last_ts: u64) -> Vec<u8> {
    let mut e = Enc::new();
    e.u64(seq);
    e.hash256(&audit);
    e.u64(last_ts);
    e.buf
}
fn decode_journal(b: &[u8]) -> KResult<(u64, [u8; 32], u64)> {
    let mut d = Dec::new(b);
    let s = d.u64()?;
    let a = d.hash256()?;
    let t = d.u64()?;
    d.finish()?;
    Ok((s, a, t))
}
fn encode_idem(koid: &KOID, version: u64, commit_ts: u64) -> Vec<u8> {
    let mut e = Enc::new();
    e.raw(koid.as_bytes());
    e.u64(version);
    e.u64(commit_ts);
    e.buf
}
fn encode_tomb(payload_hash: [u8; 32], seq: u64) -> Vec<u8> {
    let mut e = Enc::new();
    e.hash256(&payload_hash);
    e.u64(seq);
    e.buf
}
fn decode_tomb(b: &[u8]) -> KResult<([u8; 32], u64)> {
    let mut d = Dec::new(b);
    let h = d.hash256()?;
    let s = d.u64()?;
    d.finish()?;
    Ok((h, s))
}

fn encode_filter(f: &EventFilter) -> Vec<u8> {
    let mut e = Enc::new();
    match &f.koid {
        None => e.u8(0),
        Some(k) => {
            e.u8(1);
            e.raw(k.as_bytes());
        }
    }
    match &f.kinds {
        None => e.u8(0),
        Some(ks) => {
            e.u8(1);
            e.u64(ks.len() as u64);
            for k in ks {
                e.u8(k.tag());
            }
        }
    }
    e.buf
}

fn decode_filter(d: &mut Dec) -> KResult<EventFilter> {
    let koid = match d.u8()? {
        0 => None,
        1 => Some(d.koid()?),
        t => return Err(KError::Codec(format!("invalid filter koid tag {}", t))),
    };
    let kinds = match d.u8()? {
        0 => None,
        1 => {
            let n = d.u64()? as usize;
            let mut ks = Vec::with_capacity(n.min(64));
            for _ in 0..n {
                let tag = d.u8()?;
                ks.push(
                    EventKind::from_tag(tag)
                        .ok_or_else(|| KError::Codec(format!("invalid event kind tag {}", tag)))?,
                );
            }
            Some(ks)
        }
        t => return Err(KError::Codec(format!("invalid filter kinds tag {}", t))),
    };
    Ok(EventFilter { koid, kinds })
}

fn encode_sub(rec: &SubscriptionRecord) -> Vec<u8> {
    let mut e = Enc::new();
    e.raw(&encode_filter(&rec.filter));
    e.u64(rec.last_seq);
    e.buf
}

fn decode_sub(buf: &[u8]) -> KResult<SubscriptionRecord> {
    let mut d = Dec::new(buf);
    let filter = decode_filter(&mut d)?;
    let last_seq = d.u64()?;
    d.finish()?;
    Ok(SubscriptionRecord { filter, last_seq })
}

/// KSE-10 report of a derived-index rebuild: the exact image of the
/// canonical heads, plus what the sweep removed.
pub struct DerivedIndexRebuild {
    pub heads_scanned: usize,
    pub relo_rows: usize,
    pub reli_rows: usize,
    pub type_rows: usize,
    /// Rows removed that decoded fine but referenced nothing canonical.
    pub removed_stale: usize,
    /// Rows removed that failed key decode (corrupt index entries).
    pub removed_invalid: usize,
}

/// The kernel's storage boundary. Hides key layout and low-level encoding.
pub struct KnowledgeRepository {
    engine: Arc<dyn StorageEngine>,
    cache: Option<KnowledgeCache>,
}

impl KnowledgeRepository {
    pub fn new(engine: Arc<dyn StorageEngine>) -> Self {
        Self {
            engine,
            cache: None,
        }
    }

    /// Enable an in-memory LRU cache of heads and object versions.
    pub fn with_cache(&mut self, capacity: usize) {
        self.cache = Some(KnowledgeCache::with_capacity(capacity));
    }

    pub(crate) fn engine(&self) -> &dyn StorageEngine {
        self.engine.as_ref()
    }

    pub(crate) fn constraint_capabilities(&self) -> crate::storage::store::ConstraintCapabilities {
        self.engine.constraint_capabilities()
    }

    pub fn write_batch(&self, batch: &WriteBatch) -> KResult<()> {
        self.engine().write_batch(batch)
    }

    // -----------------------------------------------------------------------
    // Relationship indexes (index-free adjacency over the same KV)
    // -----------------------------------------------------------------------

    /// Write both outbound and inbound index entries for one edge.
    /// Idempotent: same (src, rel_type, dst) key is a no-op at the KV level.
    pub fn write_rel_index(&self, batch: &mut WriteBatch, src: &KOID, rel_type: &str, dst: &KOID) {
        batch.put(rel_out_key(src, rel_type, dst), vec![]);
        batch.put(rel_in_key(dst, rel_type, src), vec![]);
    }

    /// Remove both outbound and inbound index entries for one edge.
    /// Idempotent: deleting an absent key is a no-op at the KV level.
    pub fn del_rel_index(&self, batch: &mut WriteBatch, src: &KOID, rel_type: &str, dst: &KOID) {
        batch.del(rel_out_key(src, rel_type, dst));
        batch.del(rel_in_key(dst, rel_type, src));
    }

    /// Scan outbound edges from `src`, optionally filtered by `rel_type`.
    /// Returns `(rel_type, target_koid)` pairs in key order.
    pub fn scan_outbound(
        &self,
        src: &KOID,
        rel_type: Option<&str>,
    ) -> KResult<Vec<(String, KOID)>> {
        let prefix = match rel_type {
            Some(rt) => rel_out_prefix_src_type(src, rt),
            None => rel_out_prefix_src(src),
        };
        let mut out = Vec::new();
        for (k, _v) in self.engine().scan(&prefix)? {
            let (_src, rt, dst) = decode_rel_out_key(&k)?;
            out.push((rt, dst));
        }
        Ok(out)
    }

    /// Scan inbound edges to `dst`, optionally filtered by `rel_type`.
    /// Returns `(rel_type, source_koid)` pairs in key order.
    pub fn scan_inbound(&self, dst: &KOID, rel_type: Option<&str>) -> KResult<Vec<(String, KOID)>> {
        let prefix = match rel_type {
            Some(rt) => rel_in_prefix_dst_type(dst, rt),
            None => rel_in_prefix_dst(dst),
        };
        let mut out = Vec::new();
        for (k, _v) in self.engine().scan(&prefix)? {
            let (_dst, rt, src) = decode_rel_in_key(&k)?;
            out.push((rt, src));
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Type index (R9: O(log N + per-type) scoped scans)
    // -----------------------------------------------------------------------

    /// Put one `type/<type_name>/<koid>` entry. Idempotent at the KV level.
    pub fn write_type_index(&self, batch: &mut WriteBatch, type_name: &str, koid: &KOID) {
        batch.put(type_key(type_name, koid), vec![]);
    }

    /// Remove one type-index entry (used on Erase; Tombstone keeps the head
    /// so the entry stays and is filtered by the Deleted-state check).
    pub fn delete_type_index(&self, batch: &mut WriteBatch, type_name: &str, koid: &KOID) {
        batch.del(type_key(type_name, koid));
    }

    /// KOIDs indexed under `type_name`, in koid order.
    pub fn scan_type(&self, type_name: &str) -> KResult<Vec<KOID>> {
        let prefix = type_prefix(type_name);
        let mut out = Vec::new();
        for (k, _v) in self.engine().scan(&prefix)? {
            if k.len() != prefix.len() + KOID_LEN {
                continue;
            }
            let mut kb = [0u8; KOID_LEN];
            kb.copy_from_slice(&k[prefix.len()..]);
            out.push(KOID(kb));
        }
        Ok(out)
    }

    /// Whether the one-time type-index backfill has run on this database.
    pub fn type_index_marker(&self) -> KResult<bool> {
        Ok(self.engine().get(K_TYPE_INDEX)?.is_some())
    }

    pub fn put_type_index_marker(&self, batch: &mut WriteBatch) {
        batch.put(K_TYPE_INDEX.to_vec(), vec![]);
    }

    /// KSE-10: rebuild all derived indexes (relo/reli/type) from canonical
    /// state. The ko/ heads are the authority; every derived row is
    /// recomputed as their exact image and the symmetric difference is
    /// applied in ONE batch (disjoint put/del key sets, so the engine's
    /// puts-before-dels order cannot cross a del with a put).
    /// ponytail: O(derived-index) memory for the two key sets — rebuild is a
    /// repair op, not a hot path.
    pub fn rebuild_derived_indexes(&self) -> KResult<DerivedIndexRebuild> {
        let mut old: HashSet<Vec<u8>> = HashSet::new();
        let mut removed_invalid = 0usize;
        for (key, _v) in self.engine().scan(P_REL_OUT)? {
            if decode_rel_out_key(&key).is_err() {
                removed_invalid += 1;
            }
            old.insert(key);
        }
        for (key, _v) in self.engine().scan(P_REL_IN)? {
            if decode_rel_in_key(&key).is_err() {
                removed_invalid += 1;
            }
            old.insert(key);
        }
        for (key, _v) in self.engine().scan(P_TYPE)? {
            if key.len() < P_TYPE.len() + 1 + KOID_LEN {
                removed_invalid += 1;
            }
            old.insert(key);
        }

        let mut new: HashSet<Vec<u8>> = HashSet::new();
        let mut relo_rows = 0usize;
        let mut reli_rows = 0usize;
        let mut type_rows = 0usize;
        let heads = self.scan_heads()?;
        let heads_scanned = heads.len();
        for (koid, _version, ts, _state) in heads {
            let Some(ko) = self.get_object_version(&koid, ts)? else {
                continue; // head without a version row — canonical corruption, not derivable
            };
            for rel in &ko.relationships {
                let (src, dst) = match rel.direction {
                    Direction::Outbound => (koid, rel.target),
                    Direction::Inbound => (rel.target, koid),
                };
                new.insert(rel_out_key(&src, &rel.rel_type, &dst));
                new.insert(rel_in_key(&dst, &rel.rel_type, &src));
                relo_rows += 1;
                reli_rows += 1;
            }
            new.insert(type_key(&ko.metadata.type_name, &koid));
            type_rows += 1;
        }

        let removed = old.difference(&new).count();
        let mut batch = WriteBatch::new();
        for key in old.difference(&new) {
            batch.del(key.clone());
        }
        for key in new.difference(&old) {
            batch.put(key.clone(), vec![]);
        }
        if !batch.is_empty() {
            self.write_batch(&batch)?;
        }
        Ok(DerivedIndexRebuild {
            heads_scanned,
            relo_rows,
            reli_rows,
            type_rows,
            removed_stale: removed - removed_invalid,
            removed_invalid,
        })
    }

    // -----------------------------------------------------------------------
    // Schemas (REC-002: persisted so backup/restore preserves constraints)
    // -----------------------------------------------------------------------

    /// Reserved key prefix for schema rows. ASCII — cannot collide with
    /// hash-keyed object rows or the other reserved ASCII markers.
    pub const K_SCHEMA_PREFIX: &[u8] = b"sys/schema/";

    pub fn put_schema_row(&self, batch: &mut WriteBatch, type_name: &str, bytes: &[u8]) {
        let mut key = Vec::with_capacity(Self::K_SCHEMA_PREFIX.len() + type_name.len());
        key.extend_from_slice(Self::K_SCHEMA_PREFIX);
        key.extend_from_slice(type_name.as_bytes());
        batch.put(key, bytes.to_vec());
    }

    /// All persisted schema rows as (type_name, encoded bytes), key order.
    pub fn schema_rows(&self) -> KResult<Vec<(String, Vec<u8>)>> {
        let mut out = Vec::new();
        for (k, v) in self.engine().scan(Self::K_SCHEMA_PREFIX)? {
            if k.len() <= Self::K_SCHEMA_PREFIX.len() {
                continue;
            }
            let name = String::from_utf8(k[Self::K_SCHEMA_PREFIX.len()..].to_vec())
                .map_err(|_| KError::Codec("schema row key bad utf-8".into()))?;
            out.push((name, v));
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Journal
    // -----------------------------------------------------------------------

    pub fn current_seq(&self) -> KResult<u64> {
        match self.engine().get(K_JOURNAL)? {
            Some(b) => {
                let (seq, _, _) = decode_journal(&b)?;
                Ok(seq)
            }
            None => Ok(0),
        }
    }

    pub fn journal_head(&self) -> KResult<Option<(u64, [u8; 32], u64)>> {
        match self.engine().get(K_JOURNAL)? {
            Some(b) => Ok(Some(decode_journal(&b)?)),
            None => Ok(None),
        }
    }

    pub fn put_journal(&self, batch: &mut WriteBatch, seq: u64, audit: [u8; 32], last_ts: u64) {
        batch.put(K_JOURNAL.to_vec(), encode_journal(seq, audit, last_ts));
    }

    // -----------------------------------------------------------------------
    // Objects / heads
    // -----------------------------------------------------------------------

    pub fn get_object_version(
        &self,
        koid: &KOID,
        commit_ts: u64,
    ) -> KResult<Option<KnowledgeObject>> {
        if let Some(c) = &self.cache {
            if let Some(ko) = c.get_object(koid, commit_ts) {
                return Ok(Some(ko));
            }
        }
        let res = match self.engine().get(&obj_key(koid, commit_ts))? {
            Some(b) => Some(codec::decode_ko_wire(&b)?),
            None => None,
        };
        if let Some(ko) = &res {
            if let Some(c) = &self.cache {
                c.put_object(koid, commit_ts, ko);
            }
        }
        Ok(res)
    }

    pub fn get_head(&self, koid: &KOID) -> KResult<Option<(u64, u64, LifecycleState)>> {
        if let Some(c) = &self.cache {
            if let Some(head) = c.get_head(koid) {
                return Ok(Some(head));
            }
        }
        let res = match self.engine().get(&head_key(koid))? {
            Some(b) => Some(decode_head(&b)?),
            None => None,
        };
        if let Some((version, ts, state)) = res {
            if let Some(c) = &self.cache {
                c.put_head(koid, version, ts, state);
            }
            Ok(Some((version, ts, state)))
        } else {
            Ok(None)
        }
    }

    /// SE2-M25 — batch head resolution, cache-aware parity with `get_head`:
    /// cache hits answer in place, misses go through one engine `get_many`.
    pub fn get_heads_many(
        &self,
        koids: &[KOID],
    ) -> KResult<Vec<Option<(u64, u64, LifecycleState)>>> {
        let mut out: Vec<Option<(u64, u64, LifecycleState)>> = vec![None; koids.len()];
        let mut misses: Vec<(usize, Vec<u8>)> = Vec::with_capacity(koids.len());
        for (i, koid) in koids.iter().enumerate() {
            if let Some(c) = &self.cache {
                if let Some(head) = c.get_head(koid) {
                    out[i] = Some(head);
                    continue;
                }
            }
            misses.push((i, head_key(koid)));
        }
        if misses.is_empty() {
            return Ok(out);
        }
        let keys: Vec<&[u8]> = misses.iter().map(|(_, k)| k.as_slice()).collect();
        let results = self.engine().get_many(&keys)?;
        for ((i, _), res) in misses.into_iter().zip(results) {
            if let Some(b) = res {
                let head = decode_head(&b)?;
                if let Some(c) = &self.cache {
                    c.put_head(&koids[i], head.0, head.1, head.2);
                }
                out[i] = Some(head);
            }
        }
        Ok(out)
    }

    /// SE2-M25 — batch object-version loads, cache-aware parity with
    /// `get_object_version`: cache hits answer in place, misses go through
    /// one engine `get_many`.
    pub fn get_object_versions_many(
        &self,
        koids_and_ts: &[(KOID, u64)],
    ) -> KResult<Vec<Option<KnowledgeObject>>> {
        let mut out: Vec<Option<KnowledgeObject>> = vec![None; koids_and_ts.len()];
        let mut misses: Vec<(usize, Vec<u8>)> = Vec::with_capacity(koids_and_ts.len());
        for (i, (koid, ts)) in koids_and_ts.iter().enumerate() {
            if let Some(c) = &self.cache {
                if let Some(ko) = c.get_object(koid, *ts) {
                    out[i] = Some(ko);
                    continue;
                }
            }
            misses.push((i, obj_key(koid, *ts)));
        }
        if misses.is_empty() {
            return Ok(out);
        }
        let keys: Vec<&[u8]> = misses.iter().map(|(_, k)| k.as_slice()).collect();
        let results = self.engine().get_many(&keys)?;
        for ((i, _), res) in misses.into_iter().zip(results) {
            if let Some(b) = res {
                let ko = codec::decode_ko_wire(&b)?;
                if let Some(c) = &self.cache {
                    c.put_object(&koids_and_ts[i].0, koids_and_ts[i].1, &ko);
                }
                out[i] = Some(ko);
            }
        }
        Ok(out)
    }

    pub fn get_object_at(&self, koid: &KOID, snap_ts: u64) -> KResult<Option<KnowledgeObject>> {
        let entries = self.engine().scan(&obj_prefix(koid))?;
        for (k, v) in entries.iter().rev() {
            let ts_bytes: &[u8] = &k[k.len() - 8..];
            let mut ts = [0u8; 8];
            ts.copy_from_slice(ts_bytes);
            if u64::from_be_bytes(ts) <= snap_ts {
                return Ok(Some(codec::decode_ko_wire(v)?));
            }
        }
        Ok(None)
    }

    pub fn put_object_version(
        &self,
        batch: &mut WriteBatch,
        koid: &KOID,
        commit_ts: u64,
        ko: &KnowledgeObject,
    ) {
        batch.put(obj_key(koid, commit_ts), codec::encode_ko_wire(ko));
        if let Some(c) = &self.cache {
            c.delete_object(koid, commit_ts);
        }
    }

    pub fn delete_object_version(&self, batch: &mut WriteBatch, koid: &KOID, commit_ts: u64) {
        batch.del(obj_key(koid, commit_ts));
        if let Some(c) = &self.cache {
            c.delete_object(koid, commit_ts);
        }
    }

    pub fn put_head(
        &self,
        batch: &mut WriteBatch,
        koid: &KOID,
        version: u64,
        commit_ts: u64,
        state: LifecycleState,
    ) {
        batch.put(head_key(koid), encode_head(version, commit_ts, state));
        if let Some(c) = &self.cache {
            c.delete_head(koid);
        }
    }

    pub fn delete_head(&self, batch: &mut WriteBatch, koid: &KOID) {
        batch.del(head_key(koid));
        if let Some(c) = &self.cache {
            c.delete_head(koid);
        }
    }

    pub fn scan_heads(&self) -> KResult<Vec<(KOID, u64, u64, LifecycleState)>> {
        let mut out = Vec::new();
        for (hk, hb) in self.engine().scan(P_HEAD)? {
            if hk.len() < P_HEAD.len() + KOID_LEN {
                continue;
            }
            let mut kb = [0u8; KOID_LEN];
            kb.copy_from_slice(&hk[P_HEAD.len()..P_HEAD.len() + KOID_LEN]);
            let koid = KOID(kb);
            let (version, ts, state) = decode_head(&hb)?;
            out.push((koid, version, ts, state));
        }
        Ok(out)
    }

    pub fn scan_object_versions(&self, koid: &KOID) -> KResult<Vec<(u64, KnowledgeObject)>> {
        let mut out = Vec::new();
        for (k, v) in self.engine().scan(&obj_prefix(koid))? {
            let ts_bytes = &k[k.len() - 8..];
            let mut ts = [0u8; 8];
            ts.copy_from_slice(ts_bytes);
            out.push((u64::from_be_bytes(ts), codec::decode_ko_wire(&v)?));
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Events
    // -----------------------------------------------------------------------

    pub fn put_event(&self, batch: &mut WriteBatch, seq: u64, ke: &KnowledgeEvent) {
        batch.put(ke_key(seq), codec::encode_ke(ke));
    }

    pub fn get_event(&self, seq: u64) -> KResult<Option<KnowledgeEvent>> {
        match self.engine().get(&ke_key(seq))? {
            Some(b) => Ok(Some(codec::decode_ke(&b)?)),
            None => Ok(None),
        }
    }

    pub fn scan_events(&self) -> KResult<Vec<KnowledgeEvent>> {
        let mut out = Vec::new();
        for (_, v) in self.engine().scan(P_KE)? {
            out.push(codec::decode_ke(&v)?);
        }
        Ok(out)
    }

    pub fn scan_events_after(&self, after_seq: u64) -> KResult<Vec<KnowledgeEvent>> {
        let mut out = Vec::new();
        for (_, v) in self.engine().scan(P_KE)? {
            let ke = codec::decode_ke(&v)?;
            if ke.seq > after_seq {
                out.push(ke);
            }
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Idempotency
    // -----------------------------------------------------------------------

    pub fn put_idem(
        &self,
        batch: &mut WriteBatch,
        key: &str,
        koid: &KOID,
        version: u64,
        commit_ts: u64,
    ) {
        batch.put(idem_key(key), encode_idem(koid, version, commit_ts));
    }

    pub fn get_idem(&self, key: &str) -> KResult<Option<(KOID, u64, u64)>> {
        match self.engine().get(&idem_key(key))? {
            Some(b) => {
                let mut d = Dec::new(&b);
                let k = d.koid()?;
                let v = d.u64()?;
                let ts = d.u64()?;
                d.finish()?;
                Ok(Some((k, v, ts)))
            }
            None => Ok(None),
        }
    }

    // -----------------------------------------------------------------------
    // Tombstones
    // -----------------------------------------------------------------------

    pub fn put_tombstone(
        &self,
        batch: &mut WriteBatch,
        koid: &KOID,
        payload_hash: [u8; 32],
        seq: u64,
    ) {
        batch.put(tomb_key(koid), encode_tomb(payload_hash, seq));
    }

    pub fn get_tombstone(&self, koid: &KOID) -> KResult<Option<([u8; 32], u64)>> {
        match self.engine().get(&tomb_key(koid))? {
            Some(b) => Ok(Some(decode_tomb(&b)?)),
            None => Ok(None),
        }
    }

    // -----------------------------------------------------------------------
    // Subscriptions
    // -----------------------------------------------------------------------

    pub fn put_subscription(&self, batch: &mut WriteBatch, id: &str, rec: &SubscriptionRecord) {
        batch.put(sub_key(id), encode_sub(rec));
    }

    pub fn delete_subscription(&self, batch: &mut WriteBatch, id: &str) {
        batch.del(sub_key(id));
    }

    pub fn scan_subscriptions(&self) -> KResult<Vec<(String, SubscriptionRecord)>> {
        let mut out = Vec::new();
        for (k, v) in self.engine().scan(P_SUB)? {
            if k.len() <= P_SUB.len() {
                continue;
            }
            let id = String::from_utf8(k[P_SUB.len()..].to_vec())
                .map_err(|_| KError::Codec("invalid subscription id bytes".into()))?;
            out.push((id, decode_sub(&v)?));
        }
        Ok(out)
    }
}
