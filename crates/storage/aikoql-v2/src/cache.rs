//! SE2-M7 — bounded raw-block cache, SE2-M9 — raw bytes (v2 blocks decode
//! per lookup; caching decoded entries would re-pay the full-block decode
//! that v2 removes). One cache per Db, shared by every SegmentReader the
//! Db opens. Keys are (reader_id, block_index) where reader_id comes from
//! a per-cache counter that is NEVER reused: segment ids can be reused
//! after an orphan is cleaned up, so a key based on segment ids could
//! alias a dead reader's blocks and serve wrong data. Capacity is raw
//! block bytes (28-byte header + payload), hard-capped with LRU eviction;
//! a block bigger than the cap is simply not cached. Only checksum-
//! validated bytes enter the cache. Answers never depend on the cache —
//! a hit hands the caller the same bytes the file would produce.
//!
//! # ponytail: O(n) recency move per hit (n = cached blocks, ~512 at
//! 8 MiB / 16 KiB blocks) — swap for a linked LRU if blocks get tiny.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    /// Raw block bytes currently held (never exceeds the cap).
    pub bytes: usize,
}

#[derive(Debug)]
pub struct BlockCache {
    cap: usize,
    state: Mutex<State>,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
    next_id: AtomicU64,
}

#[derive(Debug, Default)]
struct State {
    entries: HashMap<(u64, u32), Arc<Vec<u8>>>,
    recency: VecDeque<(u64, u32)>,
    bytes: usize,
}

impl BlockCache {
    pub fn new(cap: usize) -> Arc<Self> {
        Arc::new(BlockCache {
            cap,
            state: Mutex::new(State::default()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            next_id: AtomicU64::new(0),
        })
    }

    /// A fresh identity for one SegmentReader (never reused — see module
    /// doc).
    pub fn reader_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Lookup returns an Arc clone — the caller decodes from shared bytes.
    pub fn get(&self, id: u64, block: u32) -> Option<Arc<Vec<u8>>> {
        let key = (id, block);
        let mut st = self.state.lock().unwrap();
        let Some(e) = st.entries.get(&key) else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let hit = e.clone();
        self.hits.fetch_add(1, Ordering::Relaxed);
        st.recency.retain(|k| k != &key);
        st.recency.push_back(key);
        Some(hit)
    }

    pub fn insert(&self, id: u64, block: u32, raw: Arc<Vec<u8>>) {
        let bytes = raw.len();
        let mut st = self.state.lock().unwrap();
        if bytes > self.cap {
            return; // one block bigger than the cache: never cached
        }
        let key = (id, block);
        if let Some(old) = st.entries.remove(&key) {
            st.bytes -= old.len();
            st.recency.retain(|k| k != &key);
        }
        while st.bytes + bytes > self.cap {
            let Some(victim) = st.recency.pop_front() else {
                break;
            };
            if let Some(v) = st.entries.remove(&victim) {
                st.bytes -= v.len();
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
        st.bytes += bytes;
        st.entries.insert(key, raw);
        st.recency.push_back(key);
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            bytes: self.state.lock().unwrap().bytes,
        }
    }
}
