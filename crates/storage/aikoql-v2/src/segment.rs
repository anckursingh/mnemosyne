//! SE2-M1 — immutable segments (docs/IMPLEMENTATION-PLAN-V2.md SE2-M1,
//! docs/TESTING-PLAN-V2.md row V2-M1).
//!
//! On-disk layout (all integers little-endian; pinned byte-exact by the
//! golden fixture in tests/segment_golden.rs):
//!
//! SEGMENT = header | data blocks | index block | bloom block | footer
//!
//! Header: `AKSE | version u16 | data_block_count u32 | entry_count u64 |
//! key_min_len u32 | key_min | key_max_len u32 | key_max | seq_lo u64 |
//! seq_hi u64 | sha256-8(everything before)`
//!
//! Block: 28-byte header `AKBL | version u16 | type u8 | compression u8 |
//! entry_count u32 | compressed_size u32 | uncompressed_size u32 |
//! sha256-8(20-byte header + payload)` + payload.
//! Types: 0 data, 1 index, 2 bloom. Compression 0 = none.
//! Data blocks are version 1, 2 (SE2-M9) or 3 (SE2-M34); index and bloom
//! are always version 1. Anything newer fails closed: a block whose
//! checksum validates is Unsupported (a future format), a stale checksum
//! is Corrupt.
//!
//! Entry: `shared_prefix_len u16 | key_suffix_len u16 | key_suffix |
//! value_len u32 | value | seq u64 | flags u8`. Entries are sorted
//! (key asc, seq desc); a key's head is its first version. The first entry
//! of a block carries its full key (shared = 0).
//!
//! v2 data payload: `restart_interval u16 | restart_count u32 |
//! restart offsets u32[] (absolute payload positions) | entries` — entry
//! encoding is unchanged, but an entry at a restart position encodes
//! shared = 0 (full key), so every interval decodes standalone. A point
//! lookup binary-searches the restart keys (borrowed, no alloc) and
//! decodes only the one interval slice it lands in (≤ 16 entries — a
//! multi-version equal-key run extends its interval, see
//! `last_restart_key` in `publish`).
//!
//! v3 data payload (SE2-M34): the v2 table with `replica_id u64` appended
//! after each entry's flags — the v2 prefix (through flags) decodes
//! identically, so the restart table and bounded lookup are shared. Only
//! v3 blocks persist the rid; v1/v2 decode it as 0 (their writers never
//! emit it) and their bytes stay golden-pinned.
//!
//! Index payload: per data block `first_key_len u16 | first_key |
//! last_key_len u16 | last_key | block_offset u64 | entry_count u32`.
//!
//! Bloom payload: `m u32 | bits (ceil(m/8), lsb-first)` with m = 10·n and
//! k = 4 probes, double hashing h1 + i·h2 mod m over sha256(key).
//!
//! Footer: `AKFT | version u16 | entry_count u64 | sha256-8(skeleton)`.
//! The skeleton covers the header, every 28-byte block header, the index
//! and bloom blocks whole, and the footer fields — but not data payloads,
//! so open() stays O(block count) no matter the file size. Torn segments
//! are impossible (atomic publication); data payloads are validated lazily
//! on the read that touches the block. Structural damage fails at open,
//! payload damage fails on access.

use crate::cache::BlockCache;
use crate::format::{checksum8, publish_atomic_writer_staged, Cursor, FormatError};
use crate::identity::ReplicaId;
use crate::placement::BlockId;
use crate::stats::Stats;
use aikoql_kernel::knowledge::kom::sha256;
use sha2::{Digest, Sha256};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Write;
use std::ops::Range;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

pub const SEGMENT_VERSION: u16 = 1;
pub const FLAG_PUT: u8 = 1;
pub const FLAG_DELETE: u8 = 2;
pub const FLAG_VERSION: u8 = 4;

pub fn segment_path(dir: &Path, segment_id: u64) -> PathBuf {
    dir.join(format!("SEGMENT-{segment_id:06}.seg"))
}

const SEGMENT_MAGIC: &[u8; 4] = b"AKSE";
const BLOCK_MAGIC: &[u8; 4] = b"AKBL";
const FOOTER_MAGIC: &[u8; 4] = b"AKFT";
const BLOCK_HEADER_LEN: usize = 28;
const FOOTER_LEN: usize = 22;
const BLOCK_DATA: u8 = 0;
const BLOCK_INDEX: u8 = 1;
const BLOCK_BLOOM: u8 = 2;
/// Smallest possible entry: shared + suffix + value_len + value + seq + flags.
const MIN_ENTRY_LEN: usize = 2 + 2 + 4 + 8 + 1;
const BLOOM_BITS_PER_KEY: usize = 10;
const BLOOM_PROBES: u32 = 4;
/// SE2-M9 — v2 data blocks: a full key every RESTART_INTERVAL entries.
/// The size of one decode interval — the bounded lookup never decodes more.
const RESTART_INTERVAL: u16 = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentEntry {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub seq: u64,
    pub flags: u8,
    /// SE2-M34 — the owning replica (0 = byte API). Persisted by v3 data
    /// blocks only; v1/v2 decode it as 0 (their writers never emit it).
    pub replica_id: ReplicaId,
}

/// SE2-M34 — one replica's anchor in a published segment: its max-seq
/// entry's location (block-local entry index), for the placement directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentAnchor {
    pub replica_id: ReplicaId,
    pub seq: u64,
    pub block_id: BlockId,
    pub entry_offset: u32,
}

/// Buffers entries and writes them as one immutable segment.
pub struct SegmentWriter {
    target_block_bytes: usize,
    entries: Vec<SegmentEntry>,
    /// SE2-M9 — v2 data blocks (restart points). `new` stays v1: the M1
    /// golden pins the v1 writer byte-exact.
    v2: bool,
    /// SE2-M34 — v3 data blocks (restart points + per-entry replica id).
    /// v3 implies v2: the restart table is shared.
    v3: bool,
}

impl SegmentWriter {
    pub fn new(target_block_bytes: usize) -> Self {
        SegmentWriter {
            target_block_bytes,
            entries: Vec::new(),
            v2: false,
            v3: false,
        }
    }

    pub fn new_v2(target_block_bytes: usize) -> Self {
        SegmentWriter {
            target_block_bytes,
            entries: Vec::new(),
            v2: true,
            v3: false,
        }
    }

    /// SE2-M34 — v3 data blocks: the v2 restart table plus the per-entry
    /// rid, so identity rows survive the flush.
    pub fn new_v3(target_block_bytes: usize) -> Self {
        SegmentWriter {
            target_block_bytes,
            entries: Vec::new(),
            v2: true,
            v3: true,
        }
    }

    pub fn push(&mut self, entry: SegmentEntry) {
        self.entries.push(entry);
    }

    /// SE2-M35 — the compaction merge flips the writer to v3 the moment an
    /// identity-carrying entry arrives: buffered rid-0 entries encode
    /// identically (v3 is v2 + the rid field), and a chunk already
    /// published as v2 stays valid — chunks are independent segments and
    /// the reader dispatches per block version.
    pub(crate) fn enable_v3(&mut self) {
        self.v2 = true;
        self.v3 = true;
    }

    /// SE2-M34 — `publish` keeps its M15 shape (the manifest fields); the
    /// anchor list is the identity flush's extra return.
    pub fn publish(&mut self, path: &Path) -> Result<(u64, u64), FormatError> {
        self.publish_with_anchors(path)
            .map(|(file_size, checksum, _)| (file_size, checksum))
    }

    /// Sort (key asc, seq desc), split into target-sized data blocks, and
    /// write the segment atomically. Caller misuse — empty input, duplicate
    /// (key, seq), zero block target — is Invalid, never written to disk.
    ///
    /// SE2-M15 — the write streams: block boundaries come from a dry pass
    /// over the sorted entries (payloads never retained), then each block
    /// is encoded, written, and dropped as it completes. Peak memory is the
    /// entries themselves + one block + the index and bloom — not several
    /// whole-segment copies. Returns (file_size, checksum8) of the file as
    /// published — the manifest record's fields, so callers never read the
    /// segment back — plus SE2-M34's anchors: one per non-zero replica id,
    /// its max-seq entry's (block, offset).
    pub fn publish_with_anchors(
        &mut self,
        path: &Path,
    ) -> Result<(u64, u64, Vec<SegmentAnchor>), FormatError> {
        self.publish_with_anchors_staged(path, None)
    }

    /// SE2-M36 — the staged form parks at the §38 windows; only the
    /// compaction path stages (SEGMENT).
    pub fn publish_with_anchors_staged(
        &mut self,
        path: &Path,
        stage: Option<&str>,
    ) -> Result<(u64, u64, Vec<SegmentAnchor>), FormatError> {
        if self.target_block_bytes == 0 {
            return Err(FormatError::Invalid("target block size must be > 0".into()));
        }
        if self.entries.is_empty() {
            return Err(FormatError::Invalid(
                "cannot publish an empty segment".into(),
            ));
        }
        let mut entries = std::mem::take(&mut self.entries);
        entries.sort_by(|a, b| a.key.cmp(&b.key).then(b.seq.cmp(&a.seq)));
        if entries
            .windows(2)
            .any(|w| w[0].key == w[1].key && w[0].seq == w[1].seq)
        {
            return Err(FormatError::Invalid("duplicate (key, seq) pair".into()));
        }

        let entry_count = entries.len() as u64;
        let key_min = entries[0].key.clone();
        let key_max = entries[entries.len() - 1].key.clone();
        let seq_lo = entries.iter().map(|e| e.seq).min().expect("non-empty");
        let seq_hi = entries.iter().map(|e| e.seq).max().expect("non-empty");
        let v2 = self.v2;
        let v3 = self.v3;

        // Block boundaries without the payloads. The split state machine
        // mirrors the encode pass exactly — including the buffered writer's
        // estimate quirk: the split check sizes an entry with its shared
        // prefix (not the restart's full key), so a restart can overshoot
        // the target slightly, as before.
        let mut bounds: Vec<(usize, usize)> = Vec::new();
        {
            let mut len = 0usize;
            let mut prev: Option<Vec<u8>> = None;
            let mut last_restart_key: Option<Vec<u8>> = None;
            // SE2-M38 — the shared prefix the current key run's head entry
            // encoded with: a cadence restart that lands mid-run is
            // repositioned to the run head (see the encode pass), growing
            // the head's encoding by exactly these bytes.
            let mut run_head_shared: Option<usize> = None;
            let mut start = 0usize;
            for (i, e) in entries.iter().enumerate() {
                let key_changed = prev
                    .as_ref()
                    .is_none_or(|p| e.key.as_slice() > p.as_slice());
                let shared_c = shared_of(&prev, e);
                let est = 2
                    + 2
                    + (e.key.len() - shared_c)
                    + 4
                    + e.value.len()
                    + 8
                    + 1
                    + if v3 { 8 } else { 0 };
                if len > 0 && len + est > self.target_block_bytes {
                    bounds.push((start, i));
                    len = 0;
                    prev = None;
                    last_restart_key = None;
                    run_head_shared = None;
                    start = i;
                }
                // v2: every RESTART_INTERVAL-th entry (0, 16, 32, …) with a
                // key strictly greater than the last restart key carries
                // its full key (shared = 0) and its position goes into the
                // table. Equal keys are skipped (see `last_restart_key`).
                // `i - start` is the block-local index — the counter the
                // split resets, without the counter.
                let is_restart = v2
                    && (i - start).is_multiple_of(RESTART_INTERVAL as usize)
                    && last_restart_key
                        .as_ref()
                        .is_none_or(|k| e.key.as_slice() > k.as_slice());
                // After a split the entry encodes with shared = 0 (the new
                // block's prev is None) — recomputed like the encode pass,
                // not from the split estimate above.
                let shared = if is_restart { 0 } else { shared_of(&prev, e) };
                if key_changed {
                    run_head_shared = Some(shared);
                }
                len += 2
                    + 2
                    + (e.key.len() - shared)
                    + 4
                    + e.value.len()
                    + 8
                    + 1
                    + if v3 { 8 } else { 0 };
                if is_restart {
                    len += run_head_shared.take().unwrap_or(0);
                    last_restart_key = Some(e.key.clone());
                }
                prev = Some(e.key.clone());
            }
            bounds.push((start, entries.len()));
        }
        let block_count = bounds.len() as u32;

        // Bloom: m = 10·n bits, 4 probes, double hashing over sha256(key).
        let m = BLOOM_BITS_PER_KEY as u64 * entry_count;
        let mut bits = vec![0u8; m.div_ceil(8) as usize];

        let mut index_payload = Vec::new();
        let mut skeleton = Vec::new();
        let mut payload = Vec::new();
        let mut table = Vec::new();
        let mut restarts: Vec<u32> = Vec::new();
        let mut anchors: HashMap<ReplicaId, SegmentAnchor> = HashMap::new();
        let mut file_size = 0u64;
        let mut whole = Sha256::new();

        publish_atomic_writer_staged(path, stage, move |f: &mut File| {
            // Header.
            let mut header = Vec::new();
            header.extend_from_slice(SEGMENT_MAGIC);
            header.extend_from_slice(&SEGMENT_VERSION.to_le_bytes());
            header.extend_from_slice(&block_count.to_le_bytes());
            header.extend_from_slice(&entry_count.to_le_bytes());
            header.extend_from_slice(&(key_min.len() as u32).to_le_bytes());
            header.extend_from_slice(&key_min);
            header.extend_from_slice(&(key_max.len() as u32).to_le_bytes());
            header.extend_from_slice(&key_max);
            header.extend_from_slice(&seq_lo.to_le_bytes());
            header.extend_from_slice(&seq_hi.to_le_bytes());
            header.extend_from_slice(&checksum8(&header));
            f.write_all(&header)?;
            whole.update(&header);
            skeleton.extend_from_slice(&header);
            file_size += header.len() as u64;

            // Data blocks, written as they complete (index offsets are
            // final-file positions).
            for (block_id, &(start, end)) in bounds.iter().enumerate() {
                payload.clear();
                restarts.clear();
                let mut prev: Option<Vec<u8>> = None;
                let mut last_restart_key: Option<Vec<u8>> = None;
                // SE2-M38 — (payload position, shared prefix) of the current
                // key run's head entry. A run can start mid-cadence (one
                // row per replica — a hot key's run is long): its first
                // cadence point would land the restart MID-RUN, hiding the
                // entries between the head and the restart from every
                // lookup, which scans [last restart ≤ key, next restart).
                // Reposition the restart to the run head — same count, the
                // strictly-increasing key invariant holds (the head IS the
                // run's first entry), density unchanged. The head entry was
                // encoded with a shared prefix, so re-encode it with its
                // full key (a restart position must decode standalone).
                let mut run_head: Option<(u32, usize)> = None;
                for (count, e) in entries[start..end].iter().enumerate() {
                    let key_changed = prev
                        .as_ref()
                        .is_none_or(|p| e.key.as_slice() > p.as_slice());
                    let is_restart = v2
                        && count.is_multiple_of(RESTART_INTERVAL as usize)
                        && last_restart_key
                            .as_ref()
                            .is_none_or(|k| e.key.as_slice() > k.as_slice());
                    let shared = if is_restart { 0 } else { shared_of(&prev, e) };
                    if key_changed {
                        run_head = Some((payload.len() as u32, shared));
                    }
                    if is_restart {
                        let (hpos, hshared) =
                            run_head.take().expect("a restart entry starts a run");
                        if hshared > 0 {
                            let mut full = Vec::with_capacity(4 + e.key.len());
                            full.extend_from_slice(&0u16.to_le_bytes());
                            full.extend_from_slice(&(e.key.len() as u16).to_le_bytes());
                            full.extend_from_slice(&e.key);
                            let old_len = 4 + e.key.len() - hshared;
                            payload.splice(hpos as usize..hpos as usize + old_len, full);
                        }
                        restarts.push(hpos);
                        last_restart_key = Some(e.key.clone());
                    }
                    payload.extend_from_slice(&(shared as u16).to_le_bytes());
                    payload.extend_from_slice(&((e.key.len() - shared) as u16).to_le_bytes());
                    payload.extend_from_slice(&e.key[shared..]);
                    payload.extend_from_slice(&(e.value.len() as u32).to_le_bytes());
                    payload.extend_from_slice(&e.value);
                    payload.extend_from_slice(&e.seq.to_le_bytes());
                    payload.push(e.flags);
                    if v3 {
                        // SE2-M34 — the rid rides after the flags: the v2
                        // prefix (through flags) decodes identically.
                        payload.extend_from_slice(&e.replica_id.0.to_le_bytes());
                    }
                    if v3 && e.replica_id != ReplicaId(0) {
                        // Anchor: the rid's max-seq entry. Byte-API rows
                        // (rid 0) never anchor.
                        let anchor = SegmentAnchor {
                            replica_id: e.replica_id,
                            seq: e.seq,
                            block_id: BlockId(block_id as u32),
                            entry_offset: count as u32,
                        };
                        match anchors.entry(e.replica_id) {
                            Entry::Vacant(v) => {
                                v.insert(anchor);
                            }
                            Entry::Occupied(mut o) => {
                                if e.seq > o.get().seq {
                                    *o.get_mut() = anchor;
                                }
                            }
                        }
                    }
                    prev = Some(e.key.clone());
                    let d = sha256(&e.key);
                    let h1 = u64::from_le_bytes(d[..8].try_into().expect("sha256 len"));
                    let h2 = u64::from_le_bytes(d[8..16].try_into().expect("sha256 len"));
                    for i in 0..BLOOM_PROBES {
                        let bit = ((h1 as u128 + i as u128 * h2 as u128) % m as u128) as usize;
                        bits[bit / 8] |= 1 << (bit % 8);
                    }
                }
                let count_u32 = (end - start) as u32;
                index_payload.extend_from_slice(&(entries[start].key.len() as u16).to_le_bytes());
                index_payload.extend_from_slice(&entries[start].key);
                index_payload.extend_from_slice(&(entries[end - 1].key.len() as u16).to_le_bytes());
                index_payload.extend_from_slice(&entries[end - 1].key);
                index_payload.extend_from_slice(&file_size.to_le_bytes());
                index_payload.extend_from_slice(&count_u32.to_le_bytes());
                let block = if v2 {
                    // Table first: interval, restart count, then each
                    // restart's absolute payload position (table size +
                    // entry position).
                    table.clear();
                    table.extend_from_slice(&RESTART_INTERVAL.to_le_bytes());
                    table.extend_from_slice(&(restarts.len() as u32).to_le_bytes());
                    for &pos in &restarts {
                        table.extend_from_slice(
                            &((6 + 4 * restarts.len() + pos as usize) as u32).to_le_bytes(),
                        );
                    }
                    table.extend_from_slice(&payload);
                    encode_block(BLOCK_DATA, count_u32, &table, if v3 { 3 } else { 2 })
                } else {
                    encode_block(BLOCK_DATA, count_u32, &payload, SEGMENT_VERSION)
                };
                f.write_all(&block)?;
                whole.update(&block);
                skeleton.extend_from_slice(&block[..BLOCK_HEADER_LEN]);
                file_size += block.len() as u64;
            }

            let index_block =
                encode_block(BLOCK_INDEX, block_count, &index_payload, SEGMENT_VERSION);
            let mut bloom_payload = Vec::with_capacity(4 + bits.len());
            bloom_payload.extend_from_slice(&(m as u32).to_le_bytes());
            bloom_payload.extend_from_slice(&bits);
            let bloom_block = encode_block(
                BLOCK_BLOOM,
                entry_count as u32,
                &bloom_payload,
                SEGMENT_VERSION,
            );

            // Footer checksum over the skeleton: header, all block headers,
            // the index and bloom blocks whole, and the footer fields. Data
            // payloads are excluded so open() never hashes the whole file.
            f.write_all(&index_block)?;
            whole.update(&index_block);
            skeleton.extend_from_slice(&index_block);
            file_size += index_block.len() as u64;
            f.write_all(&bloom_block)?;
            whole.update(&bloom_block);
            skeleton.extend_from_slice(&bloom_block);
            file_size += bloom_block.len() as u64;
            skeleton.extend_from_slice(FOOTER_MAGIC);
            skeleton.extend_from_slice(&SEGMENT_VERSION.to_le_bytes());
            skeleton.extend_from_slice(&entry_count.to_le_bytes());

            let mut footer = Vec::with_capacity(FOOTER_LEN);
            footer.extend_from_slice(FOOTER_MAGIC);
            footer.extend_from_slice(&SEGMENT_VERSION.to_le_bytes());
            footer.extend_from_slice(&entry_count.to_le_bytes());
            footer.extend_from_slice(&checksum8(&skeleton));
            f.write_all(&footer)?;
            whole.update(&footer);
            file_size += footer.len() as u64;

            let digest = whole.finalize();
            let checksum = u64::from_le_bytes(digest[..8].try_into().expect("sha256-8 slice"));
            Ok((file_size, checksum, anchors.into_values().collect()))
        })
    }
}

/// Common prefix of the previous key and this one — the entry stores only
/// the suffix (0 when there is no previous key, e.g. the first of a block).
fn shared_of(prev: &Option<Vec<u8>>, e: &SegmentEntry) -> usize {
    match prev {
        Some(p) => common_prefix(p, &e.key),
        None => 0,
    }
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

fn encode_block(kind: u8, entries: u32, payload: &[u8], version: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(BLOCK_HEADER_LEN + payload.len());
    out.extend_from_slice(BLOCK_MAGIC);
    out.extend_from_slice(&version.to_le_bytes());
    out.push(kind);
    out.push(0); // compression: none
    out.extend_from_slice(&entries.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    let mut sk = Vec::with_capacity(20 + payload.len());
    sk.extend_from_slice(&out);
    sk.extend_from_slice(payload);
    out.extend_from_slice(&checksum8(&sk));
    out.extend_from_slice(payload);
    out
}

/// A read-only handle on a published segment. Open reads only the skeleton
/// (header, block headers, index, bloom, footer) — O(block count), never
/// O(file size) — and defers data-block payloads to the read that touches
/// the block, which validates that block's checksum on first touch.
#[derive(Debug)]
pub struct SegmentReader {
    file: File,
    file_len: u64,
    block_count: u32,
    entry_count: u64,
    key_min: Vec<u8>,
    key_max: Vec<u8>,
    seq_lo: u64,
    seq_hi: u64,
    data: Vec<DataBlock>,
    /// Index payload — per-block key ranges point into this buffer.
    index: Vec<u8>,
    /// Bloom payload (m u32 + bits).
    bloom: Vec<u8>,
    bloom_m: u32,
    /// SE2-M7 — shared block cache, when the Db runs one. Consulted before
    /// a block read, fed after a validated decode; answers never change.
    cache: Option<std::sync::Arc<BlockCache>>,
    cache_id: u64,
    /// SE2-M8 — read-path stats, when the Db runs them (cumulative atomics;
    /// timings run only when present). None = zero instrumentation overhead.
    stats: Option<std::sync::Arc<Stats>>,
}

#[derive(Debug)]
struct DataBlock {
    /// File offset of the 28-byte block header (payload follows at +28).
    header: u64,
    payload_len: usize,
    entries: u32,
    /// SE2-M9 — v2 payload (restart table) vs v1 (plain entries).
    v2: bool,
    /// SE2-M34 — v3 payload (v2 restart table + per-entry rid). v3 ⇒ v2.
    v3: bool,
    /// Key ranges into the reader's index payload.
    first: Range<usize>,
    last: Range<usize>,
    /// Atomic so concurrent readers on one segment are safe (SE2-M4) — a
    /// benign race re-validates a block's deterministic checksum twice.
    validated: AtomicBool,
}

/// Bounded positional read: short reads are the same Corrupt truncation
/// class the whole-file Cursor used to report (so the M1 pins hold), real
/// I/O failures stay Io. `read_at` has no shared seek position — concurrent
/// readers on one segment never race payload loads.
fn read_segment_at(
    file: &File,
    file_len: u64,
    offset: u64,
    n: usize,
) -> Result<Vec<u8>, FormatError> {
    if file_len - offset < n as u64 {
        return Err(FormatError::Corrupt(format!(
            "truncated: need {n} bytes at offset {offset}, {} remain",
            file_len - offset
        )));
    }
    let mut buf = vec![0u8; n];
    #[cfg(unix)]
    file.read_exact_at(&mut buf, offset)
        .map_err(|e| FormatError::Io(format!("segment read at {offset}: {e}")))?;
    #[cfg(windows)]
    file.seek_read(&mut buf, offset)
        .map_err(|e| FormatError::Io(format!("segment read at {offset}: {e}")))?;
    Ok(buf)
}

/// The reader attachment every segment the Db opens for reads carries: the
/// shared block cache and the shared read-path stats (SE2-M21 — the pair
/// that makes a segment's reads cached and counted; grouped because every
/// reader-opening site passes them together).
pub(crate) struct SegmentAttach {
    pub(crate) cache: Option<std::sync::Arc<BlockCache>>,
    pub(crate) stats: Option<std::sync::Arc<Stats>>,
}

impl SegmentReader {
    pub fn open(path: &Path) -> Result<Self, FormatError> {
        Self::open_inner(path, None, None)
    }

    /// Open with a shared block cache (SE2-M7) and/or read-path stats
    /// (SE2-M8). The cache assigns this reader a never-reused identity —
    /// segment ids can be reused after orphan cleanup, so the cache key
    /// cannot be the segment id. Stats are cumulative atomics shared with
    /// the Db — None means zero instrumentation overhead.
    pub(crate) fn open_with(
        path: &Path,
        cache: Option<std::sync::Arc<BlockCache>>,
        stats: Option<std::sync::Arc<Stats>>,
    ) -> Result<Self, FormatError> {
        Self::open_inner(path, cache, stats)
    }

    fn open_inner(
        path: &Path,
        cache: Option<std::sync::Arc<BlockCache>>,
        stats: Option<std::sync::Arc<Stats>>,
    ) -> Result<Self, FormatError> {
        let file = File::open(path)
            .map_err(|e| FormatError::Io(format!("open segment {}: {e}", path.display())))?;
        let file_len = file
            .metadata()
            .map_err(|e| FormatError::Io(format!("segment metadata {}: {e}", path.display())))?
            .len();

        // Header: magic(4) version(2) data_block_count(4) entry_count(8)
        // key_min_len(4) key_min key_max_len(4) key_max seq_lo(8) seq_hi(8)
        // checksum(8). Read in pieces — the variable key ranges are tiny.
        let prefix = read_segment_at(&file, file_len, 0, 22)?;
        let mut cur = Cursor::new(&prefix);
        if cur.take(4)? != SEGMENT_MAGIC {
            return Err(FormatError::Corrupt("segment bad magic".into()));
        }
        let version = cur.u16()?;
        if version != SEGMENT_VERSION {
            // A newer format whose v1-shaped header checksum still validates
            // is Unsupported, not damaged; anything else fails closed.
            if file_len >= 54 {
                let head = read_segment_at(&file, file_len, 0, 54)?;
                if checksum8(&head[..46]) == head[46..54] {
                    return Err(FormatError::Unsupported(format!(
                        "segment format version {version} (this build: {SEGMENT_VERSION})"
                    )));
                }
            }
            return Err(FormatError::Corrupt(format!(
                "segment version {version} damaged"
            )));
        }
        let block_count = cur.u32()?;
        let entry_count = cur.u64()?;
        let key_min_len = cur.u32()? as usize;
        let mut header = Vec::with_capacity(22 + key_min_len + 4 + 32);
        header.extend_from_slice(&prefix);
        let key_min = read_segment_at(&file, file_len, header.len() as u64, key_min_len)?;
        header.extend_from_slice(&key_min);
        let key_max_len_b = read_segment_at(&file, file_len, header.len() as u64, 4)?;
        let key_max_len =
            u32::from_le_bytes(key_max_len_b[..4].try_into().expect("u32 slice")) as usize;
        header.extend_from_slice(&key_max_len_b);
        let key_max = read_segment_at(&file, file_len, header.len() as u64, key_max_len)?;
        header.extend_from_slice(&key_max);
        let tail = read_segment_at(&file, file_len, header.len() as u64, 24)?; // seq_lo | seq_hi | checksum
        let mut tcur = Cursor::new(&tail);
        let seq_lo = tcur.u64()?;
        let seq_hi = tcur.u64()?;
        let stored: [u8; 8] = tcur.take(8)?.try_into().expect("8-byte checksum");
        header.extend_from_slice(&tail[..16]);
        header.extend_from_slice(&stored);
        if checksum8(&header[..header.len() - 8]) != stored {
            return Err(FormatError::Corrupt(
                "segment header checksum mismatch".into(),
            ));
        }
        if seq_lo > seq_hi {
            return Err(FormatError::Corrupt(format!(
                "seq_lo {seq_lo} > seq_hi {seq_hi}"
            )));
        }

        // Walk blocks until the footer, skipping payloads by size (a
        // payload that happens to start with "AKFT" is never misread).
        let mut data: Vec<DataBlock> = Vec::new();
        let mut block_headers: Vec<u8> = Vec::new();
        let mut last_kind: Option<u8> = None;
        let mut index_block: Option<(u64, usize)> = None;
        let mut bloom_block: Option<(u64, usize)> = None;
        let mut cur = header.len() as u64;
        loop {
            let remaining = file_len - cur;
            let footer = remaining >= 4
                && read_segment_at(&file, file_len, cur, 4)?.as_slice() == FOOTER_MAGIC;
            if footer {
                break;
            }
            if remaining < BLOCK_HEADER_LEN as u64 {
                return Err(FormatError::Corrupt(format!(
                    "truncated: need a block or footer at offset {cur}, {remaining} bytes remain"
                )));
            }
            let header_off = cur;
            let hdr = read_segment_at(&file, file_len, cur, BLOCK_HEADER_LEN)?;
            let mut hcur = Cursor::new(&hdr);
            if hcur.take(4)? != BLOCK_MAGIC {
                return Err(FormatError::Corrupt("block bad magic".into()));
            }
            let version = hcur.u16()?;
            let kind = hcur.u8()?;
            let compression = hcur.u8()?;
            if compression != 0 {
                return Err(FormatError::Unsupported(format!(
                    "block compression {compression}"
                )));
            }
            if kind > BLOCK_BLOOM || last_kind.is_some_and(|k| kind < k) {
                return Err(FormatError::Corrupt("block types out of order".into()));
            }
            last_kind = Some(kind);
            let entries = hcur.u32()?;
            let compressed = hcur.u32()? as usize;
            hcur.u32()?; // uncompressed size (same: compression 0)
            let stored: [u8; 8] = hcur.take(8)?.try_into().expect("8-byte checksum");
            // SE2-M9/M34 — data blocks are version 1|2|3, index and bloom
            // stay version 1. Anything else fails closed: a valid checksum
            // is a future format (Unsupported), a stale one is damage
            // (Corrupt).
            let in_set = version == SEGMENT_VERSION
                || ((version == 2 || version == 3) && kind == BLOCK_DATA);
            if !in_set {
                if version == 0 {
                    return Err(FormatError::Corrupt("block version 0".into()));
                }
                let payload =
                    read_segment_at(&file, file_len, cur + BLOCK_HEADER_LEN as u64, compressed)?;
                let mut sk = Vec::with_capacity(20 + compressed);
                sk.extend_from_slice(&hdr[..20]);
                sk.extend_from_slice(&payload);
                if checksum8(&sk) != stored {
                    return Err(FormatError::Corrupt(format!(
                        "block version {version} damaged"
                    )));
                }
                return Err(FormatError::Unsupported(format!(
                    "block version {version} (this build: data 1|2|3, index/bloom 1)"
                )));
            }
            cur += BLOCK_HEADER_LEN as u64;
            if file_len - cur < compressed as u64 {
                return Err(FormatError::Corrupt(format!(
                    "truncated: need {compressed} bytes at offset {cur}, {} remain",
                    file_len - cur
                )));
            }
            match kind {
                BLOCK_DATA => {
                    if entries as usize > compressed / MIN_ENTRY_LEN {
                        return Err(FormatError::Corrupt(format!(
                            "{entries} entries cannot fit in {compressed} bytes"
                        )));
                    }
                    block_headers.extend_from_slice(&hdr);
                    data.push(DataBlock {
                        header: header_off,
                        payload_len: compressed,
                        entries,
                        v2: version == 2 || version == 3,
                        v3: version == 3,
                        first: 0..0,
                        last: 0..0,
                        validated: AtomicBool::new(false),
                    });
                }
                BLOCK_INDEX => {
                    if index_block.is_some() {
                        return Err(FormatError::Corrupt("two index blocks".into()));
                    }
                    index_block = Some((header_off, compressed));
                }
                BLOCK_BLOOM => {
                    if bloom_block.is_some() {
                        return Err(FormatError::Corrupt("two bloom blocks".into()));
                    }
                    bloom_block = Some((header_off, compressed));
                }
                _ => unreachable!("kind checked above"),
            }
            cur += compressed as u64;
        }
        let footer_start = cur;
        if file_len - footer_start != FOOTER_LEN as u64 {
            return Err(FormatError::Corrupt(format!(
                "footer must be exactly {FOOTER_LEN} bytes at the end, {} remain",
                file_len - footer_start
            )));
        }
        let footer = read_segment_at(&file, file_len, footer_start, FOOTER_LEN)?;
        let mut fcur = Cursor::new(&footer);
        if fcur.take(4)? != FOOTER_MAGIC {
            return Err(FormatError::Corrupt("footer bad magic".into()));
        }
        if fcur.u16()? != SEGMENT_VERSION {
            return Err(FormatError::Corrupt("footer version".into()));
        }
        if fcur.u64()? != entry_count {
            return Err(FormatError::Corrupt("footer entry_count mismatch".into()));
        }
        let (index_header, index_len) =
            index_block.ok_or_else(|| FormatError::Corrupt("missing index block".into()))?;
        let (bloom_header, bloom_len) =
            bloom_block.ok_or_else(|| FormatError::Corrupt("missing bloom block".into()))?;
        if data.is_empty() || data.len() != block_count as usize {
            return Err(FormatError::Corrupt(format!(
                "header says {block_count} data blocks, found {}",
                data.len()
            )));
        }
        let index = read_segment_at(
            &file,
            file_len,
            index_header + BLOCK_HEADER_LEN as u64,
            index_len,
        )?;
        let bloom = read_segment_at(
            &file,
            file_len,
            bloom_header + BLOCK_HEADER_LEN as u64,
            bloom_len,
        )?;

        // Index payload: per-block key range, offset, entry count. Key
        // ranges point into the index buffer; the stored offset is the
        // file-absolute block header position.
        let mut icur = Cursor::new(&index);
        let mut total = 0u64;
        for db in &mut data {
            let len = icur.u16()? as usize;
            let start = icur.pos();
            icur.take(len)?;
            db.first = start..icur.pos();
            let len = icur.u16()? as usize;
            let start = icur.pos();
            icur.take(len)?;
            db.last = start..icur.pos();
            let offset = icur.u64()?;
            let count = icur.u32()?;
            if offset != db.header {
                return Err(FormatError::Corrupt(format!(
                    "index says block at {offset}, found at {}",
                    db.header
                )));
            }
            if count != db.entries {
                return Err(FormatError::Corrupt(format!(
                    "index says {count} entries, block header says {}",
                    db.entries
                )));
            }
            total += count as u64;
        }
        if !icur.is_empty() {
            return Err(FormatError::Corrupt("index trailing bytes".into()));
        }
        if total != entry_count {
            return Err(FormatError::Corrupt(format!(
                "data blocks hold {total} entries, header says {entry_count}"
            )));
        }

        // Bloom payload: m u32 + ceil(m/8) bytes of bits.
        let mut bcur = Cursor::new(&bloom);
        let bloom_m = bcur.u32()?;
        if bcur.remaining() as u32 != bloom_m.div_ceil(8) {
            return Err(FormatError::Corrupt(format!(
                "bloom: m = {bloom_m} needs {} bit-bytes, {} present",
                bloom_m.div_ceil(8),
                bcur.remaining()
            )));
        }

        // Index + bloom block headers must agree with the header counts
        // (block header: magic 4 | version 2 | type 1 | compression 1 |
        // entry_count u32 — so the count sits at offset 8).
        let idx_entries = read_segment_at(&file, file_len, index_header + 8, 4)?;
        let idx_entries = u32::from_le_bytes(idx_entries[..4].try_into().expect("u32 slice"));
        if idx_entries != block_count {
            return Err(FormatError::Corrupt(
                "index block entry count mismatch".into(),
            ));
        }
        let blm_entries = read_segment_at(&file, file_len, bloom_header + 8, 4)?;
        let blm_entries = u32::from_le_bytes(blm_entries[..4].try_into().expect("u32 slice"));
        if blm_entries as u64 != entry_count {
            return Err(FormatError::Corrupt(
                "bloom block entry count mismatch".into(),
            ));
        }

        // Footer checksum over the skeleton (the index header + index
        // payload + bloom header + bloom payload are contiguous in the
        // file, so one bounded read covers that span).
        let mut skeleton = Vec::with_capacity(
            header.len()
                + data.len() * BLOCK_HEADER_LEN
                + (bloom_header + BLOCK_HEADER_LEN as u64 + bloom_len as u64 - index_header)
                    as usize
                + 14,
        );
        skeleton.extend_from_slice(&header);
        skeleton.extend_from_slice(&block_headers);
        skeleton.extend_from_slice(&read_segment_at(
            &file,
            file_len,
            index_header,
            (bloom_header + BLOCK_HEADER_LEN as u64 + bloom_len as u64 - index_header) as usize,
        )?);
        skeleton.extend_from_slice(&footer[..14]);
        let stored: [u8; 8] = footer[14..].try_into().expect("8-byte checksum");
        if checksum8(&skeleton) != stored {
            return Err(FormatError::Corrupt(
                "footer skeleton checksum mismatch".into(),
            ));
        }

        let cache_id = cache.as_ref().map(|c| c.reader_id()).unwrap_or(0);
        Ok(SegmentReader {
            file,
            file_len,
            block_count,
            entry_count,
            key_min,
            key_max,
            seq_lo,
            seq_hi,
            data,
            index,
            bloom,
            bloom_m,
            cache,
            cache_id,
            stats,
        })
    }

    pub fn entry_count(&self) -> u64 {
        self.entry_count
    }

    /// SE2-M34 — the entry at (block, offset): the full decode of that
    /// block, nth entry (offsets are block-local entry indexes — the
    /// placement anchors' shape). Out-of-range is None, not an error.
    pub fn entry_at(
        &self,
        block_id: BlockId,
        entry_offset: u32,
    ) -> Result<Option<SegmentEntry>, FormatError> {
        if block_id.0 as usize >= self.data.len() {
            return Ok(None);
        }
        let entries = self.block_entries(block_id.0 as usize)?;
        Ok(entries.into_iter().nth(entry_offset as usize))
    }

    pub fn block_count(&self) -> u32 {
        self.block_count
    }

    /// SE2-M32 — per-block entry count for placement validation (the
    /// §34 structural check: block/entry within range at recovery).
    pub(crate) fn block_entry_count(&self, block_id: u32) -> Option<u32> {
        self.data.get(block_id as usize).map(|b| b.entries)
    }

    pub fn key_min(&self) -> &[u8] {
        &self.key_min
    }

    pub fn key_max(&self) -> &[u8] {
        &self.key_max
    }

    pub fn seq_lo(&self) -> u64 {
        self.seq_lo
    }

    pub fn seq_hi(&self) -> u64 {
        self.seq_hi
    }

    /// The key's bloom hash pair (the sha256 split) — one hash per get,
    /// shared by every segment's probe (SE2-M22: the probe used to re-hash
    /// the key per segment, ~5-7 sha256s per get).
    pub fn bloom_hashes(key: &[u8]) -> (u64, u64) {
        let d = sha256(key);
        (
            u64::from_le_bytes(d[..8].try_into().expect("sha256 len")),
            u64::from_le_bytes(d[8..16].try_into().expect("sha256 len")),
        )
    }

    /// False positives possible, false negatives never: a false answer means
    /// the key is definitely not in the segment.
    pub fn bloom_may_contain(&self, key: &[u8]) -> bool {
        let (h1, h2) = Self::bloom_hashes(key);
        self.bloom_may_contain_hashes(h1, h2)
    }

    /// The probe over an already-computed hash pair (SE2-M22 — the Db hashes
    /// once per get and shares the pair across segments).
    pub fn bloom_may_contain_hashes(&self, h1: u64, h2: u64) -> bool {
        let bits = &self.bloom[4..];
        for i in 0..BLOOM_PROBES {
            let bit = ((h1 as u128 + i as u128 * h2 as u128) % self.bloom_m as u128) as usize;
            if bits[bit / 8] & (1 << (bit % 8)) == 0 {
                return false;
            }
        }
        true
    }

    /// The head version of `key` (highest seq — entries sort seq-descending).
    pub fn get(&self, key: &[u8]) -> Result<Option<SegmentEntry>, FormatError> {
        let t0 = self.stats.as_ref().map(|_| Instant::now());
        let located = self.locate(key);
        if let (Some(st), Some(t0)) = (&self.stats, t0) {
            st.index_lookup_ns
                .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        let Some(i) = located else {
            return Ok(None);
        };
        self.block_get(i, key)
    }

    /// SE2-M25 — batch point lookup, answers parallel to the input (`None` =
    /// not in this segment). Keys are located once and grouped by block;
    /// each block is fetched/checksum-validated once, then every key in it
    /// decodes from the held payload (v2) or is found in the whole-block
    /// decode (v1) — the same dispatch as `block_get`, with the block I/O
    /// amortized over the batch.
    pub fn get_many(&self, keys: &[&[u8]]) -> Result<Vec<Option<SegmentEntry>>, FormatError> {
        let t0 = self.stats.as_ref().map(|_| Instant::now());
        let mut by_block: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (pos, key) in keys.iter().enumerate() {
            if let Some(i) = self.locate(key) {
                by_block.entry(i).or_default().push(pos);
            }
        }
        if let (Some(st), Some(t0)) = (&self.stats, t0) {
            st.index_lookup_ns
                .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        let mut out: Vec<Option<SegmentEntry>> = (0..keys.len()).map(|_| None).collect();
        for (i, positions) in by_block {
            let b = &self.data[i];
            if !b.v2 {
                let entries = self.block_entries(i)?;
                for pos in positions {
                    out[pos] = entries
                        .iter()
                        .find(|e| e.key.as_slice() == keys[pos])
                        .cloned();
                }
                continue;
            }
            let raw = self.block_raw(i)?;
            let payload = &raw[BLOCK_HEADER_LEN..];
            for pos in positions {
                let t1 = self.stats.as_ref().map(|_| Instant::now());
                let res = self.block_get_v2(keys[pos], payload, b.v3, None)?;
                if let (Some(st), Some(t1)) = (&self.stats, t1) {
                    st.block_decode_ns
                        .fetch_add(t1.elapsed().as_nanos() as u64, Ordering::Relaxed);
                }
                out[pos] = res;
            }
        }
        Ok(out)
    }

    /// Every version of `key`, seq-descending. Versions may straddle a block
    /// boundary, so this walks blocks while their first key ≤ the target.
    pub fn versions(&self, key: &[u8]) -> Result<Vec<SegmentEntry>, FormatError> {
        let Some(mut i) = self.locate(key) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        while i < self.data.len() && self.block_key(&self.data[i].first) <= key {
            let entries = self.block_entries(i)?;
            out.extend(entries.into_iter().filter(|e| e.key.as_slice() == key));
            i += 1;
        }
        Ok(out)
    }

    /// Keys in [start, end) byte order, versions seq-descending within a key.
    pub fn scan(&self, start: &[u8], end: &[u8]) -> Result<Vec<SegmentEntry>, FormatError> {
        let first = self
            .data
            .partition_point(|b| self.block_key(&b.last) < start);
        let mut out = Vec::new();
        for (i, b) in self.data[first..].iter().enumerate() {
            if self.block_key(&b.first) >= end {
                break;
            }
            let entries = self.block_entries(first + i)?;
            out.extend(
                entries
                    .into_iter()
                    .filter(|e| e.key.as_slice() >= start && e.key.as_slice() < end),
            );
        }
        Ok(out)
    }

    /// First block whose key range covers `key`, if any.
    fn locate(&self, key: &[u8]) -> Option<usize> {
        let i = self.data.partition_point(|b| self.block_key(&b.last) < key);
        (i < self.data.len() && self.block_key(&self.data[i].first) <= key).then_some(i)
    }

    fn block_key(&self, r: &Range<usize>) -> &[u8] {
        &self.index[r.clone()]
    }

    /// Fetch and checksum-validate the raw block bytes (28-byte header +
    /// payload), served from the shared cache when present (SE2-M7, now raw
    /// bytes — SE2-M9). Only validated bytes enter the cache, so a decode
    /// failure reproduces deterministically on a hit. Lazy: open() stays
    /// O(block count); the payload is validated on the first read that
    /// touches it.
    fn block_raw(&self, i: usize) -> Result<std::sync::Arc<Vec<u8>>, FormatError> {
        let b = &self.data[i];
        if let Some(cache) = &self.cache {
            let t0 = self.stats.as_ref().map(|_| Instant::now());
            let hit = cache.get(self.cache_id, i as u32);
            if let (Some(st), Some(t0)) = (&self.stats, t0) {
                st.block_cache_lookup_ns
                    .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
            }
            match hit {
                Some(raw) => {
                    if let Some(st) = &self.stats {
                        st.block_cache_hits.fetch_add(1, Ordering::Relaxed);
                    }
                    return Ok(raw);
                }
                None => {
                    if let Some(st) = &self.stats {
                        st.block_cache_misses.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        let t0 = self.stats.as_ref().map(|_| Instant::now());
        let raw = read_segment_at(
            &self.file,
            self.file_len,
            b.header,
            BLOCK_HEADER_LEN + b.payload_len,
        )?;
        // SE2-M21: the first-touch checksum validation is block-I/O-path
        // work — inside the io timer, so a cold get attributes fully.
        if !b.validated.load(Ordering::Relaxed) {
            // SE2-M22 — chained updates over the two slices instead of
            // copying them into one buffer first (that copy was a second
            // full-block memcpy per first touch). Byte-identical digest.
            let mut hasher = Sha256::new();
            hasher.update(&raw[..20]);
            hasher.update(&raw[BLOCK_HEADER_LEN..]);
            let digest = hasher.finalize();
            if digest[..8] != raw[20..BLOCK_HEADER_LEN] {
                return Err(FormatError::Corrupt(format!(
                    "data block {i} checksum mismatch"
                )));
            }
            b.validated.store(true, Ordering::Relaxed);
        }
        if let (Some(st), Some(t0)) = (&self.stats, t0) {
            st.block_io_ns
                .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
            st.blocks_read.fetch_add(1, Ordering::Relaxed);
            st.bytes_read.fetch_add(raw.len() as u64, Ordering::Relaxed);
        }
        let raw = std::sync::Arc::new(raw);
        if let Some(cache) = &self.cache {
            cache.insert(self.cache_id, i as u32, raw.clone());
        }
        Ok(raw)
    }

    /// SE2-M9 — point lookup. A v2 block binary-searches its restart keys
    /// and decodes only the ≤16-entry interval the key lands in, chaining
    /// prefixes through one scratch buffer; only the winning key + value
    /// are cloned. A v1 block decodes fully — the bound applies to the v2
    /// format, which is what the Db writes.
    fn block_get(&self, i: usize, key: &[u8]) -> Result<Option<SegmentEntry>, FormatError> {
        let b = &self.data[i];
        if !b.v2 {
            return Ok(self
                .block_entries(i)?
                .into_iter()
                .find(|e| e.key.as_slice() == key));
        }
        let raw = self.block_raw(i)?;
        let t0 = self.stats.as_ref().map(|_| Instant::now());
        let out = self.block_get_v2(key, &raw[BLOCK_HEADER_LEN..], b.v3, None)?;
        if let (Some(st), Some(t0)) = (&self.stats, t0) {
            st.block_decode_ns
                .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        Ok(out)
    }

    /// SE2-M34 — the object's head within a v3 segment: the newest entry
    /// of `key` whose replica_id matches, decoding the same bounded
    /// restart interval the byte-API lookup decodes. Other replicas' rows
    /// (rid 0 = byte API included) never answer (§11). v1/v2 blocks persist
    /// no replica id, so they hold no object rows — None, not an error.
    pub fn get_by_rid(
        &self,
        key: &[u8],
        rid: ReplicaId,
    ) -> Result<Option<SegmentEntry>, FormatError> {
        let Some(mut i) = self.locate(key) else {
            return Ok(None);
        };
        loop {
            let b = &self.data[i];
            // SE2-M38 — the key's equal-key run can straddle a block
            // boundary (one row per replica: a hot key's run easily
            // exceeds a block), so a miss on this block falls through
            // while the run continues into the next one.
            // ponytail: O(run length) — the rid-filtered scan walks the
            // key's whole seq-descending run (restarts only bound distinct
            // keys). M39's §43 certification measures it; the §13 fix is a
            // per-block rid→offset index, not a scan tweak.
            let run_spills = self.block_key(&b.last) == key && i + 1 < self.data.len();
            if b.v3 {
                let raw = self.block_raw(i)?;
                let t0 = self.stats.as_ref().map(|_| Instant::now());
                let out = self.block_get_v2(key, &raw[BLOCK_HEADER_LEN..], true, Some(rid))?;
                if let (Some(st), Some(t0)) = (&self.stats, t0) {
                    st.block_decode_ns
                        .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
                }
                if out.is_some() {
                    return Ok(out);
                }
            }
            if !run_spills {
                return Ok(None);
            }
            i += 1;
        }
    }

    /// Bounded v2/v3 point lookup. The restart table is validated up front
    /// (offsets inside the payload, full keys at restart positions, keys
    /// strictly increasing) so the binary search below cannot silently
    /// misread damaged data — it fails closed instead. Stats count only
    /// decoded interval entries; restart probes are key reads, not decodes.
    /// SE2-M34: `v3` payloads carry the rid after the flags; a `rid` filter
    /// keeps scanning the key's equal-key run past other replicas' rows
    /// (the run is seq-descending, so the first matching entry is the
    /// object's head) — `None` answers the raw key-space head as before.
    fn block_get_v2(
        &self,
        key: &[u8],
        payload: &[u8],
        v3: bool,
        rid: Option<ReplicaId>,
    ) -> Result<Option<SegmentEntry>, FormatError> {
        let mut cur = Cursor::new(payload);
        cur.u16()?; // restart interval (stored for forward compat)
        let restarts = cur.u32()? as usize;
        let table_len = 6 + 4 * restarts;
        if table_len > payload.len() {
            return Err(FormatError::Corrupt(
                "v2 restart table exceeds payload".into(),
            ));
        }
        let offs = &payload[6..table_len];
        let mut keys: Vec<&[u8]> = Vec::with_capacity(restarts);
        let mut prev: Option<&[u8]> = None;
        for j in 0..restarts {
            let o =
                u32::from_le_bytes(offs[j * 4..j * 4 + 4].try_into().expect("u32 slice")) as usize;
            if o < table_len || o >= payload.len() {
                return Err(FormatError::Corrupt(format!(
                    "restart offset {o} outside payload"
                )));
            }
            let k = restart_key(payload, o)?;
            if prev.is_some_and(|p| k <= p) {
                return Err(FormatError::Corrupt(
                    "restart keys not strictly increasing".into(),
                ));
            }
            keys.push(k);
            prev = Some(k);
        }
        // First restart whose key > target — decode starts at the one
        // before it (its key ≤ target, and entries before it are strictly
        // smaller, so the interval holds every possible match).
        let r = keys.partition_point(|k| *k <= key);
        if r == 0 {
            return Ok(None); // key < first restart key — locate() prevents this
        }
        let start =
            u32::from_le_bytes(offs[(r - 1) * 4..r * 4].try_into().expect("u32 slice")) as usize;
        let end = if r < restarts {
            u32::from_le_bytes(offs[r * 4..r * 4 + 4].try_into().expect("u32 slice")) as usize
        } else {
            payload.len()
        };
        let mut cur = Cursor::new(&payload[start..end]);
        let mut scratch: Vec<u8> = Vec::new();
        let mut decoded = 0u64;
        let mut out = None;
        while !cur.is_empty() {
            let shared = cur.u16()? as usize;
            if shared > scratch.len() {
                return Err(FormatError::Corrupt(format!(
                    "entry shared prefix {shared} exceeds previous key {}",
                    scratch.len()
                )));
            }
            let suffix_len = cur.u16()? as usize;
            let suffix = cur.take(suffix_len)?;
            scratch.truncate(shared);
            scratch.extend_from_slice(suffix);
            decoded += 1;
            if scratch.as_slice() == key {
                let value = cur.vec()?;
                let seq = cur.u64()?;
                let flags = cur.u8()?;
                let entry_rid = if v3 {
                    ReplicaId(cur.u64()?)
                } else {
                    ReplicaId(0)
                };
                if rid.is_none_or(|r| entry_rid == r) {
                    out = Some(SegmentEntry {
                        key: scratch.clone(),
                        value,
                        seq,
                        flags,
                        replica_id: entry_rid,
                    });
                    break;
                }
                // Another replica's row at this key — the run continues
                // seq-descending (the rid's head may sit below).
                continue;
            }
            if scratch.as_slice() > key {
                break; // entries are sorted — nothing further matches
            }
            let value_len = cur.u32()? as usize;
            cur.take(value_len)?;
            cur.u64()?; // seq
            cur.u8()?; // flags
            if v3 {
                cur.u64()?; // SE2-M34 — replica id
            }
        }
        if let Some(st) = &self.stats {
            st.entries_decoded.fetch_add(decoded, Ordering::Relaxed);
        }
        Ok(out)
    }

    /// Decode a data block in full (scans, version walks, the streaming
    /// iterator — compaction). v2 payloads start with the restart table;
    /// the entries decode identically to v1, restart entries carrying
    /// shared = 0 so each interval chains from a full key.
    fn block_entries(&self, i: usize) -> Result<Vec<SegmentEntry>, FormatError> {
        let b = &self.data[i];
        let raw = self.block_raw(i)?;
        let t0 = self.stats.as_ref().map(|_| Instant::now());
        let payload = &raw[BLOCK_HEADER_LEN..];
        let start = if b.v2 {
            let mut tcur = Cursor::new(payload);
            tcur.u16()?;
            let restarts = tcur.u32()? as usize;
            let table_len = 6 + 4 * restarts;
            if table_len > payload.len() {
                return Err(FormatError::Corrupt(
                    "v2 restart table exceeds payload".into(),
                ));
            }
            table_len
        } else {
            0
        };
        let mut cur = Cursor::new(&payload[start..]);
        let mut out = Vec::with_capacity(b.entries as usize);
        let mut prev: Vec<u8> = Vec::new();
        for _ in 0..b.entries {
            let shared = cur.u16()? as usize;
            if shared > prev.len() {
                return Err(FormatError::Corrupt(format!(
                    "entry shared prefix {shared} exceeds previous key {}",
                    prev.len()
                )));
            }
            let suffix_len = cur.u16()? as usize;
            let suffix = cur.take(suffix_len)?.to_vec();
            let mut key = prev[..shared].to_vec();
            key.extend_from_slice(&suffix);
            let value = cur.vec()?;
            let seq = cur.u64()?;
            let flags = cur.u8()?;
            let replica_id = if b.v3 {
                ReplicaId(cur.u64()?) // SE2-M34 — the rid after the flags
            } else {
                ReplicaId(0)
            };
            prev = key.clone();
            out.push(SegmentEntry {
                key,
                value,
                seq,
                flags,
                replica_id,
            });
        }
        if !cur.is_empty() {
            return Err(FormatError::Corrupt(format!(
                "data block {i} trailing bytes"
            )));
        }
        if let (Some(st), Some(t0)) = (&self.stats, t0) {
            st.block_decode_ns
                .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
            st.entries_decoded
                .fetch_add(b.entries as u64, Ordering::Relaxed);
        }
        Ok(out)
    }
}

/// The key at a restart position in a v2 payload — the format requires
/// shared = 0 there (a full key), so the suffix IS the key, borrowed.
fn restart_key(payload: &[u8], o: usize) -> Result<&[u8], FormatError> {
    let mut cur = Cursor::new(&payload[o..]);
    let shared = cur.u16()? as usize;
    if shared != 0 {
        return Err(FormatError::Corrupt(format!(
            "restart entry at {o} has shared prefix {shared}"
        )));
    }
    let len = cur.u16()? as usize;
    cur.take(len)
}

/// Streaming iterator over every entry in key order — compaction's k-way
/// merge pulls one entry at a time, so the merge is O(k) memory, not
/// O(dataset). Blocks load (and validate) as the cursor reaches them.
pub struct SegmentIter<'a> {
    reader: &'a SegmentReader,
    block: usize,
    entries: std::vec::IntoIter<SegmentEntry>,
}

impl<'a> Iterator for SegmentIter<'a> {
    type Item = Result<SegmentEntry, FormatError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(e) = self.entries.next() {
                return Some(Ok(e));
            }
            let i = self.block;
            if i >= self.reader.data.len() {
                return None;
            }
            match self.reader.block_entries(i) {
                Ok(v) => {
                    self.entries = v.into_iter();
                    self.block += 1;
                }
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

impl SegmentReader {
    pub fn iter(&self) -> SegmentIter<'_> {
        SegmentIter {
            reader: self,
            block: 0,
            entries: Vec::new().into_iter(),
        }
    }

    /// SE2-M12 — a streaming cursor over [start, end) (`end` None =
    /// unbounded): starts at the first block whose key range can hold
    /// `start`; blocks whose first key ≥ end are never opened.
    pub fn scan_iter<'a>(&'a self, start: &'a [u8], end: Option<&'a [u8]>) -> SegmentScan<'a> {
        let first = self
            .data
            .partition_point(|b| self.block_key(&b.last) < start);
        SegmentScan {
            reader: self,
            start,
            end,
            block: first,
            raw: None,
            pos: 0,
            scratch: Vec::new(),
            last: None,
            v3: false,
            done: false,
        }
    }
}

/// SE2-M12 — per-segment streaming scan cursor: one block in memory at a
/// time (cache-served raw bytes), one entry decoded per step after a
/// restart-table seek — no whole-block Vec materialization (the legacy
/// `scan` path keeps its for the M1 suite). Versions within a key are
/// consecutive and seq-descending, so the cursor yields each key's HEAD
/// only; every decoded entry — skipped versions and out-of-range entries
/// included — counts in entries_decoded (the W4/W5 amplification
/// evidence).
pub struct SegmentScan<'a> {
    reader: &'a SegmentReader,
    start: &'a [u8],
    end: Option<&'a [u8]>,
    block: usize,
    raw: Option<std::sync::Arc<Vec<u8>>>,
    pos: usize,
    scratch: Vec<u8>,
    last: Option<Vec<u8>>,
    /// SE2-M34 — the loaded block is v3 (entries carry the rid).
    v3: bool,
    done: bool,
}

impl<'a> SegmentScan<'a> {
    /// Load the next block that can hold in-range keys (the first one's
    /// key ≥ end stops the scan), seek the decode position, and reset the
    /// prefix scratch — a block's entry chain starts fresh at its first
    /// entry (v2 restart entries carry shared = 0).
    fn load_block(&mut self) -> Result<bool, FormatError> {
        if self.block >= self.reader.data.len() {
            return Ok(false);
        }
        if let Some(end) = self.end {
            if self.reader.block_key(&self.reader.data[self.block].first) >= end {
                return Ok(false);
            }
        }
        let b = &self.reader.data[self.block];
        let raw = self.reader.block_raw(self.block)?;
        self.pos = scan_seek_pos(&raw[BLOCK_HEADER_LEN..], b.v2, self.start)?;
        self.raw = Some(raw);
        self.v3 = b.v3;
        self.block += 1;
        self.scratch.clear();
        Ok(true)
    }
}

impl<'a> Iterator for SegmentScan<'a> {
    type Item = Result<SegmentEntry, FormatError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            if self.raw.is_none() {
                match self.load_block() {
                    Ok(true) => {}
                    Ok(false) => return None,
                    Err(e) => return Some(Err(e)),
                }
            }
            let raw = self.raw.as_ref().expect("loaded");
            let payload = &raw[BLOCK_HEADER_LEN..];
            if self.pos >= payload.len() {
                self.raw = None; // block exhausted — next block
                continue;
            }
            let mut cur = Cursor::new(&payload[self.pos..]);
            let shared = match cur.u16() {
                Ok(v) => v as usize,
                Err(e) => return Some(Err(e)),
            };
            if shared > self.scratch.len() {
                return Some(Err(FormatError::Corrupt(format!(
                    "entry shared prefix {shared} exceeds previous key {}",
                    self.scratch.len()
                ))));
            }
            let suffix = match cur.u16().and_then(|n| cur.take(n as usize)) {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            self.scratch.truncate(shared);
            self.scratch.extend_from_slice(suffix);
            if let Some(st) = &self.reader.stats {
                st.entries_decoded.fetch_add(1, Ordering::Relaxed);
            }
            // The first loaded block may hold keys before start (the seek
            // lands at most one restart interval back): skip their bodies.
            if self.scratch.as_slice() < self.start {
                match skip_entry_body(&mut cur, self.v3) {
                    Ok(()) => {}
                    Err(e) => return Some(Err(e)),
                }
                self.pos += cur.pos();
                continue;
            }
            if self.end.is_some_and(|end| self.scratch.as_slice() >= end) {
                self.done = true; // sorted — nothing further is in range
                return None;
            }
            // Versions of one key are consecutive — yield the head only.
            if self.last.as_deref() == Some(self.scratch.as_slice()) {
                match skip_entry_body(&mut cur, self.v3) {
                    Ok(()) => {}
                    Err(e) => return Some(Err(e)),
                }
                self.pos += cur.pos();
                continue;
            }
            let value = match cur.vec() {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            let seq = match cur.u64() {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            let flags = match cur.u8() {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            let replica_id = if self.v3 {
                match cur.u64() {
                    Ok(v) => ReplicaId(v),
                    Err(e) => return Some(Err(e)),
                }
            } else {
                ReplicaId(0)
            };
            self.pos += cur.pos();
            let key = self.scratch.clone();
            self.last = Some(key.clone());
            return Some(Ok(SegmentEntry {
                key,
                value,
                seq,
                flags,
                replica_id,
            }));
        }
    }
}

/// Skip the value/seq/flags tail of an entry whose key is out of range or
/// an older version — no clone for rows the scan will not return.
fn skip_entry_body(cur: &mut Cursor<'_>, v3: bool) -> Result<(), FormatError> {
    let value_len = cur.u32()? as usize;
    cur.take(value_len)?;
    cur.u64()?; // seq
    cur.u8()?; // flags
    if v3 {
        cur.u64()?; // SE2-M34 — replica id
    }
    Ok(())
}

/// SE2-M12 — the payload position where [start, ·) decoding begins: the
/// last v2 restart whose key ≤ start (the first restart when none is — its
/// key > start and nothing precedes it), or 0 for v1. Restart keys are
/// strictly increasing (a writer invariant, validated up front in
/// block_get_v2), so a binary search lands the decode point with at most
/// one interval (≤16 entries) decoded before `start`.
/// ponytail: probes validate bounds + shared = 0 per restart only — the
/// block checksum covers accidental corruption, and the legacy whole-block
/// scan path never validated the table either; a valid-checksum lying
/// table is a malicious-writer case. Full-table validation stays where it
/// exists (block_get_v2); add it here if a v2 table is ever found corrupt.
fn scan_seek_pos(payload: &[u8], v2: bool, start: &[u8]) -> Result<usize, FormatError> {
    if !v2 {
        return Ok(0);
    }
    let mut cur = Cursor::new(payload);
    cur.u16()?; // restart interval
    let restarts = cur.u32()? as usize;
    let table_len = 6 + 4 * restarts;
    if table_len > payload.len() {
        return Err(FormatError::Corrupt(
            "v2 restart table exceeds payload".into(),
        ));
    }
    let offs = &payload[6..table_len];
    let at = |j: usize| -> Result<&[u8], FormatError> {
        let o = u32::from_le_bytes(offs[j * 4..j * 4 + 4].try_into().expect("u32 slice")) as usize;
        if o < table_len || o >= payload.len() {
            return Err(FormatError::Corrupt(format!(
                "restart offset {o} outside payload"
            )));
        }
        restart_key(payload, o)
    };
    let (mut lo, mut hi) = (0usize, restarts);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if at(mid)? <= start {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let r = lo.saturating_sub(1); // 0 when start < the first restart key
    let o = u32::from_le_bytes(offs[r * 4..r * 4 + 4].try_into().expect("u32 slice")) as usize;
    Ok(o)
}
