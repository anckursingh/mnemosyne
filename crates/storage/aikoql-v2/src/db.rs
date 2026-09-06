//! SE2-M2/M6 — Db: WAL → memtable → flush → segment (design §7–§10, §19).
//!
//! Commit pipeline (the KSE-13 120a order, ported): assign seq → append
//! WAL frame → durability boundary → apply memtable → ack. One frame =
//! one batch = one sequence number.
//!
//! Durability modes (§7): Sync is the default and fsyncs every batch;
//! Async skips the durability boundary. GroupCommit (SE2-M6) runs a
//! committer thread: batches submitted through `Db::writer()` handles
//! queue up and commit as groups — one fsync per group, bounded by
//! max_batch_ops / max_batch_bytes / max_wait_duration — applied and
//! acked in submission order (ack only after apply, so acked == durable
//! AND visible). Sync mode remains the correctness baseline: its WAL
//! bytes are what group commit must reproduce exactly. No mode may
//! silently downgrade — Sync is the Default.
//!
//! One-writer policy (§19): `LOCK` holds an OS file lock for the database
//! lifetime; a second open fails closed (FormatError::Locked).
//!
//! Flush is synchronous in M2 — deterministic correctness over lock-free
//! sophistication, the doc's own call. rotate() makes the active memtable
//! immutable (reads keep seeing it); flush() publishes each immutable as a
//! segment. Publication order makes every crash window recoverable:
//! segment files → manifest → CURRENT → WAL truncate. A crash before the
//! manifest leaves orphan segments plus the full WAL (replay recovers);
//! before CURRENT the old pair is consistent; after CURRENT the replay of
//! the not-yet-truncated WAL is idempotent (same (key, seq) → same value).
//!
//! Drop does NOT flush — recovery is the WAL's job.

use crate::cache::{BlockCache, CacheStats};
use crate::compaction::{merge, CompactStats, KeepAll, RetentionPolicy};
use crate::format::{verify_pair, Current, FormatError, Manifest, SegmentRecord, FORMAT_VERSION};
use crate::identity::directory::{
    identity_log_path, load_identity_logs, load_replica_logs, orphan_identity_logs,
    orphan_replica_logs, replica_log_path, IdentityLog, IdentityRecord, IdentityResolver,
    LocalIdentityDirectory, ReplicaLog, ReplicaRecord,
};
use crate::identity::topology::{LocalReplicaDirectory, ReplicaDirectory};
use crate::identity::{LogicalId, NodeId, ObjectId, ReplicaId, LOCAL_NODE_ID};
use crate::memtable::Memtable;
use crate::placement::directory::{
    load_placement_logs, merge_placement, orphan_placement_logs, placement_log_path,
    validate_segment_location, PhysicalLocation, Placement, PlacementLog, PlacementRecord,
};
use crate::placement::{BlockId, SegmentId};
use crate::segment::{
    SegmentAttach, SegmentEntry, SegmentReader, SegmentWriter, FLAG_DELETE, FLAG_PUT,
};
use crate::stats::{ReadPathStats, Stats};
use crate::wal::{encode_frame, replay_frames, Op};
use aikoql_kernel::knowledge::kom::sha256;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub const LOCK_FILE: &str = "LOCK";
pub const WAL_FILE: &str = "WAL-000001.log";

/// One scan row — a key and its current value.
pub type ScanRow = (Vec<u8>, Vec<u8>);

const DEFAULT_MEMTABLE_BYTES: usize = 64 * 1024 * 1024;
/// SE2-M9 — 16 KiB: v2 point lookups decode ≤16 entries of their block, so
/// the per-read cost scales with block size (~6 µs/KiB measured); 64 KiB
/// blocks cost the warm read path 4× the decode work of 16 KiB ones.
const DEFAULT_BLOCK_TARGET: usize = 16 * 1024;
const DEFAULT_GROUP_BATCH_OPS: usize = 4096;
const DEFAULT_GROUP_BATCH_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_CACHE_BYTES: usize = 8 * 1024 * 1024;
/// SE2-M10 — at least this many L0 segments triggers a KeepAll compaction
/// on the write path (steady state: one L1 + the active L0).
const DEFAULT_L0_COMPACT_TRIGGER: usize = 4;
/// SE2-M16 — a KeepAll merge rewrites the whole accumulated dataset, so a
/// monotonically growing bulk seed pays it quadratically; the size tier
/// skips the merge while L0 is not yet this fraction of L1 (L0 bytes >=
/// L1 bytes / ratio; L1 empty always merges; 0 restores count-only M10).
const DEFAULT_L0_TIER_RATIO: usize = 1;
/// SE2-M20 — compaction merge chunk cap in estimated entry bytes: a merge
/// publishes its output as a sequence of ~this-size segments instead of
/// buffering the whole merged dataset in one writer (the DS-PERF-L RSS
/// amplification). 0 = one unbounded segment (the pre-M20 shape). Small
/// datasets produce one chunk either way — every pre-M20 pin holds at the
/// 64 MiB default.
const DEFAULT_MERGE_CHUNK_BYTES: usize = 64 * 1024 * 1024;

pub fn manifest_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("MANIFEST-{generation:06}"))
}

pub use crate::segment::segment_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DurabilityMode {
    #[default]
    Sync,
    GroupCommit,
    Async,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub dir: PathBuf,
    pub memtable_bytes: usize,
    pub block_target: usize,
    pub durability: DurabilityMode,
    /// Group commit caps (SE2-M6): a group never exceeds these (a single
    /// batch larger than a cap commits alone) and waits at most
    /// `max_wait_duration` for company — ZERO commits as soon as the
    /// queue has what it has. SE2-M13 — the default stays ZERO: under the
    /// blocking ack (in-flight = 1 per writer), the window is dead time —
    /// coalescing comes from concurrent submitters, not from waiting (the
    /// M6 wait=5ms arm measured ~5 ms window tax per group for zero extra
    /// coalescing). Upgrade path, if a workload ever needs window-filling:
    /// a non-blocking submit API.
    pub max_batch_ops: usize,
    pub max_batch_bytes: usize,
    pub max_wait_duration: Duration,
    /// SE2-M7 — block cache cap in raw block bytes (SE2-M9); 0 disables.
    /// The cache only skips repeat block reads — it can never change an
    /// answer (pinned by `cache_never_changes_answers`).
    pub cache_bytes: usize,
    /// SE2-M10 — L0 compaction trigger: a write path that leaves this many
    /// L0 segments compacts them (KeepAll) into one L1. 0 disables. An
    /// explicit flush() is the caller's checkpoint and never triggers.
    pub l0_compact_trigger: usize,
    /// SE2-M16 — size tier above the count floor: the merge only fires once
    /// L0's bytes are at least L1's bytes divided by this ratio (1 = the
    /// L0 pile is as big as L1). 0 disables the size gate (M10
    /// count-only). With uniform flushes F and trigger 4, merges land at
    /// flushes 4, 8, 16, 32… instead of every 4th — the bulk-seed write
    /// amplification drops from quadratic to ~O(n log n).
    pub l0_tier_ratio: usize,
    /// SE2-M20 — compaction merge chunk cap in estimated entry bytes: a
    /// merge publishes its output as a sequence of ~this-size segments
    /// (one manifest record each) so compaction memory is bounded by the
    /// cap instead of the whole merged dataset. 0 = one unbounded segment
    /// (the pre-M20 shape).
    pub merge_chunk_bytes: usize,
}

impl Config {
    pub fn new(dir: PathBuf) -> Self {
        Config {
            dir,
            memtable_bytes: DEFAULT_MEMTABLE_BYTES,
            block_target: DEFAULT_BLOCK_TARGET,
            durability: DurabilityMode::default(),
            max_batch_ops: DEFAULT_GROUP_BATCH_OPS,
            max_batch_bytes: DEFAULT_GROUP_BATCH_BYTES,
            max_wait_duration: Duration::ZERO,
            cache_bytes: DEFAULT_CACHE_BYTES,
            l0_compact_trigger: DEFAULT_L0_COMPACT_TRIGGER,
            l0_tier_ratio: DEFAULT_L0_TIER_RATIO,
            merge_chunk_bytes: DEFAULT_MERGE_CHUNK_BYTES,
        }
    }
}

#[derive(Debug)]
struct State {
    active: Memtable,
    immutables: Vec<Memtable>,
    /// Manifest order, oldest first. SE2-M10 — arc-vectored: a get clones
    /// the whole vec under the read guard and probes lock-free, so a cold
    /// disk read never holds the state lock (the W8 write-stall). Readers
    /// that cloned the vec keep their segment set alive across a flush or
    /// compaction — snapshot semantics by construction.
    segments: Arc<Vec<Arc<SegmentReader>>>,
    segment_records: Vec<SegmentRecord>,
    next_seq: u64,
    next_segment_id: u64,
    generation: u64,
    /// SE2-M30 — identity/replica directories (spec §8.1/§8.2): the
    /// ObjectId → LogicalId and LogicalId → ReplicaId maps (rebuilt at
    /// open from the delta logs + the active WAL; every create applies
    /// before its ack), the pending delta records the next flush
    /// publishes in its publication window, and the monotonic allocators.
    /// The allocators never decrease — deleted ids are never reused
    /// (§16/§49); a crash between reservation and commit leaves a gap,
    /// which is not reuse.
    identity: HashMap<ObjectId, LogicalId>,
    replicas: HashMap<LogicalId, ReplicaId>,
    pending_identity: Vec<IdentityRecord>,
    pending_replicas: Vec<ReplicaRecord>,
    next_logical_id: u64,
    next_replica_id: u64,
    /// SE2-M32 — the placement directory (spec §34): ReplicaId →
    /// Placement, rebuilt at open from the delta logs + the active WAL.
    /// Every create applies Memtable placement before its ack (§14) — its
    /// generation rides the WAL op, so replay reproduces the exact record
    /// and the PL-005 gate stays one rule. Pending deltas publish in the
    /// flush window; the generation allocator never decreases (stale
    /// records are ignored, so map generations only grow).
    placements: HashMap<ReplicaId, Placement>,
    pending_placements: Vec<PlacementRecord>,
    next_placement_generation: u64,
}

/// One queued batch waiting on its group: the ops plus the ack channel
/// (a fresh bounded(1) per batch — std has no oneshot).
type Batch = (Vec<Op>, mpsc::SyncSender<Result<u64, FormatError>>);

pub struct Db {
    config: Config,
    /// Held forever — the OS lock (dropping the file releases it).
    _lock: File,
    /// Append-only handle; truncated at each flush publication. Shared:
    /// in GroupCommit mode the committer thread appends and flush may
    /// truncate — one mutex, always taken alone (never nested), so a
    /// flush can never interleave a group's append-and-apply window.
    wal: Arc<Mutex<File>>,
    state: Arc<RwLock<State>>,
    /// GroupCommit mode only: the Db's own sender (dropping it makes the
    /// queue disconnect and lets the committer exit) and the committer
    /// thread itself, joined on drop.
    queue_tx: Option<mpsc::Sender<Batch>>,
    committer: Option<std::thread::JoinHandle<()>>,
    /// Commit fsyncs so far — one per batch (Sync) or one per group
    /// (GroupCommit); flush truncation syncs are not counted.
    fsyncs: Arc<AtomicU64>,
    /// SE2-M7 — shared block cache (None when cache_bytes = 0). Readers
    /// consult and feed it; it never changes an answer.
    cache: Option<Arc<BlockCache>>,
    /// SE2-M8 — read-path instrumentation (the QA spec's truth layer):
    /// cumulative atomics shared with every reader the Db opens.
    stats: Arc<Stats>,
}

impl Db {
    pub fn open(config: Config) -> Result<Db, FormatError> {
        let lock = lock_directory(&config.dir)?;
        let current_path = config.dir.join("CURRENT");
        let current = match Current::read(&current_path) {
            Ok(c) => c,
            Err(FormatError::Io(_)) => {
                // Fresh database: publish the empty pair FIRST, so the
                // manifest always exists before the WAL can record a batch.
                let manifest = Manifest {
                    format_version: FORMAT_VERSION,
                    generation: 1,
                    segments: vec![],
                    wal_ids: vec![],
                };
                Manifest::publish(&manifest_path(&config.dir, 1), &manifest)?;
                let current = Current::new(FORMAT_VERSION, 1);
                Current::publish(&current_path, &current)?;
                current
            }
            Err(e) => return Err(e),
        };
        let manifest = Manifest::read(&manifest_path(&config.dir, current.manifest_generation))?;
        verify_pair(&current, &manifest)?;
        // Orphan segments (a crash between segment publication and
        // manifest/CURRENT, or compaction leftovers): reported and ignored.
        // They are unreferenced data — a later flush may reuse the id,
        // which is safe because nothing references the orphan.
        for id in orphan_segments(&config.dir, &manifest) {
            eprintln!("aikoql-v2: orphan segment SEGMENT-{id:06}.seg ignored (not in manifest)");
        }

        // SE2-M30 — identity/replica directories: apply every delta log at
        // or below CURRENT's generation (oldest first) — a damaged one
        // fails closed, identity state is unrecoverable without it — and
        // report-and-ignore newer orphans (a crash between log publish and
        // CURRENT, the §24 state-C window: the WAL still holds the frames,
        // so replay rebuilds the same records).
        let mut identity: HashMap<ObjectId, LogicalId> = HashMap::new();
        let mut replicas: HashMap<LogicalId, ReplicaId> = HashMap::new();
        for log in load_identity_logs(&config.dir, current.manifest_generation)? {
            for rec in &log.records {
                merge_identity(&mut identity, rec.oid, rec.lid)?;
            }
        }
        for log in load_replica_logs(&config.dir, current.manifest_generation)? {
            for rec in &log.records {
                merge_replica(&mut replicas, rec.lid, rec.node, rec.rid)?;
            }
        }
        for gen in orphan_identity_logs(&config.dir, current.manifest_generation) {
            eprintln!(
                "aikoql-v2: orphan identity log IDENTITY-{gen:06}.log ignored \
                 (generation past CURRENT)"
            );
        }
        for gen in orphan_replica_logs(&config.dir, current.manifest_generation) {
            eprintln!(
                "aikoql-v2: orphan replica log REPLICA-{gen:06}.log ignored \
                 (generation past CURRENT)"
            );
        }
        let mut pending_identity: Vec<IdentityRecord> = Vec::new();
        let mut pending_replicas: Vec<ReplicaRecord> = Vec::new();

        // Referenced segments must open (fail closed on missing/corrupt).
        // SE2-M7: when the block cache is on, every reader the Db opens
        // shares it (reopened segments get a fresh identity — the cache is
        // per-Db, in-memory). SE2-M8: readers also share the read-path
        // stats.
        let cache = (config.cache_bytes > 0).then(|| BlockCache::new(config.cache_bytes));
        let stats = Arc::new(Stats::default());
        let mut segments = Vec::with_capacity(manifest.segments.len());
        let mut readers_by_segment: HashMap<u64, Arc<SegmentReader>> = HashMap::new();
        for rec in &manifest.segments {
            let path = segment_path(&config.dir, rec.segment_id);
            let reader = Arc::new(SegmentReader::open_with(
                &path,
                cache.clone(),
                Some(Arc::clone(&stats)),
            )?);
            readers_by_segment.insert(rec.segment_id, Arc::clone(&reader));
            segments.push(reader);
        }

        // SE2-M32 — placement directory: apply every delta log ≤ CURRENT's
        // generation (oldest first) through the one merge gate. SE2-M35 —
        // the structural validator runs on the SURVIVING map, not per log:
        // a relocation log supersedes the flush logs' records, and a
        // superseded location naming a compacted-away segment is history,
        // not corruption. What remains must name a manifest segment with
        // an in-range block/entry — anything else fails closed. Newer
        // orphans are reported and ignored (the §24 state-C window: the
        // WAL still holds the ops, replay rebuilds the same records).
        let mut placements: HashMap<ReplicaId, Placement> = HashMap::new();
        for log in load_placement_logs(&config.dir, current.manifest_generation)? {
            for rec in &log.records {
                merge_placement(&mut placements, rec.rid, rec.placement)?;
            }
        }
        let segment_ids: HashSet<u64> = manifest.segments.iter().map(|s| s.segment_id).collect();
        for (rid, p) in &placements {
            if let Placement::Segment(loc) = p {
                let reader = readers_by_segment.get(&loc.segment_id.0).ok_or_else(|| {
                    FormatError::Corrupt(format!(
                        "placement for replica {rid:?} references segment {} absent from the manifest",
                        loc.segment_id.0
                    ))
                })?;
                let entries = reader.block_entry_count(loc.block_id.0);
                validate_segment_location(loc, &segment_ids, entries)?;
            }
        }
        for gen in orphan_placement_logs(&config.dir, current.manifest_generation) {
            eprintln!(
                "aikoql-v2: orphan placement log PLACEMENT-{gen:06}.log ignored \
                 (generation past CURRENT)"
            );
        }
        let mut pending_placements: Vec<PlacementRecord> = Vec::new();

        // Replay the active WAL (create it if this is the first open).
        // No append mode: on Windows FILE_APPEND_DATA handles cannot
        // SetEndOfFile, and flush truncates the WAL. Writes seek to the
        // end under the wal lock — the state lock held across a write
        // (SE-05) keeps exactly one writer in the append window.
        let wal_path = config.dir.join(WAL_FILE);
        let mut wal = OpenOptions::new()
            .create(true)
            .truncate(false) // the WAL holds acked batches — never truncate on open
            .read(true)
            .write(true)
            .open(&wal_path)
            .map_err(|e| FormatError::Io(format!("open WAL {}: {e}", wal_path.display())))?;
        let mut wal_bytes = Vec::new();
        wal.seek(SeekFrom::Start(0))
            .map_err(|e| FormatError::Io(format!("WAL seek: {e}")))?;
        wal.read_to_end(&mut wal_bytes)
            .map_err(|e| FormatError::Io(format!("WAL read: {e}")))?;
        let (frames, consumed) = replay_frames(&wal_bytes)?;
        if consumed != wal_bytes.len() {
            // torn tail: drop the partial final frame (it was never acked)
            wal.set_len(consumed as u64)
                .map_err(|e| FormatError::Io(format!("WAL truncate: {e}")))?;
            wal.sync_all()
                .map_err(|e| FormatError::Io(format!("WAL sync: {e}")))?;
        }

        // Replay bypasses the durability boundary — the frames are already
        // fsynced — but preserves every sequence number.
        let mut active = Memtable::new();
        let mut replay_max = 0;
        for frame in &frames {
            for op in &frame.ops {
                match op {
                    Op::Put(k, v) => active.apply(k.clone(), frame.seq, Some(v.clone())),
                    Op::Delete(k) => active.apply(k.clone(), frame.seq, None),
                    // SE2-M33 — the rid rides the op (spec §17/§18), so
                    // replay restores the entry's identity without
                    // consulting the directories.
                    Op::PutObject(rid, k, v) => {
                        active.apply_object(k.clone(), frame.seq, Some(v.clone()), *rid)
                    }
                    Op::DeleteObject(rid, k) => {
                        active.apply_object(k.clone(), frame.seq, None, *rid)
                    }
                    // SE2-M30 — a replayed create re-pends its records:
                    // the next flush re-exports them, so an identity that
                    // only ever lived in a truncated WAL still lands in a
                    // log (the merge rule makes the duplicate harmless).
                    // SE2-M32 — the placement record is the exact one the
                    // live apply produced (its generation rides the op):
                    // the PL-005 gate treats the replay as a duplicate of
                    // the logged record, or stale against a newer one.
                    Op::CreateObject {
                        oid,
                        lid,
                        rid,
                        pgen,
                    } => {
                        merge_identity(&mut identity, *oid, *lid)?;
                        pending_identity.push(IdentityRecord {
                            oid: *oid,
                            lid: *lid,
                        });
                        merge_replica(&mut replicas, *lid, LOCAL_NODE_ID, *rid)?;
                        pending_replicas.push(ReplicaRecord {
                            lid: *lid,
                            node: LOCAL_NODE_ID,
                            rid: *rid,
                        });
                        let placement = Placement::Memtable { generation: *pgen };
                        merge_placement(&mut placements, *rid, placement)?;
                        pending_placements.push(PlacementRecord {
                            rid: *rid,
                            placement,
                        });
                    }
                }
            }
            replay_max = replay_max.max(frame.seq);
        }
        let segment_max = manifest
            .segments
            .iter()
            .map(|s| s.seq_hi)
            .max()
            .unwrap_or(0);
        let next_seq = replay_max.max(segment_max) + 1;
        let next_segment_id = manifest
            .segments
            .iter()
            .map(|s| s.segment_id)
            .max()
            .unwrap_or(0)
            + 1;
        // SE2-M30 — the allocators recover past every id that ever existed
        // (logs + replayed WAL): ids are never reused after restart (§32
        // ID-014) or deletion (§49).
        let next_logical_id = identity.values().map(|l| l.0).max().unwrap_or(0) + 1;
        let next_replica_id = replicas.values().map(|r| r.0).max().unwrap_or(0) + 1;
        // SE2-M32 — placement generations recover past the newest applied
        // record (logs + replayed WAL); the gate ignores anything older,
        // so the map maximum IS the maximum ever allocated.
        let next_placement_generation = placements
            .values()
            .map(|p| p.generation())
            .max()
            .unwrap_or(0)
            + 1;

        let wal = Arc::new(Mutex::new(wal));
        let state = Arc::new(RwLock::new(State {
            active,
            immutables: vec![],
            segments: Arc::new(segments),
            segment_records: manifest.segments,
            next_seq,
            next_segment_id,
            generation: manifest.generation,
            identity,
            replicas,
            pending_identity,
            pending_replicas,
            next_logical_id,
            next_replica_id,
            placements,
            pending_placements,
            next_placement_generation,
        }));
        let fsyncs = Arc::new(AtomicU64::new(0));
        let (queue_tx, committer) = if config.durability == DurabilityMode::GroupCommit {
            let (tx, rx) = mpsc::channel();
            let handle = {
                let wal = Arc::clone(&wal);
                let state = Arc::clone(&state);
                let config = config.clone();
                let fsyncs = Arc::clone(&fsyncs);
                let cache = cache.clone();
                let stats = Arc::clone(&stats);
                std::thread::spawn(move || {
                    committer_loop(rx, wal, state, config, fsyncs, cache, stats)
                })
            };
            (Some(tx), Some(handle))
        } else {
            (None, None)
        };
        Ok(Db {
            config,
            _lock: lock,
            wal,
            state,
            queue_tx,
            committer,
            fsyncs,
            cache,
            stats,
        })
    }

    /// One batch, one sequence (design refinement: sequence is per-batch).
    /// GroupCommit mode routes through the commit queue — the same
    /// pipeline, executed by the committer thread one group at a time.
    /// PR#2 review SE-05: `&self` — with concurrent writers, the Sync/Async
    /// path holds the state lock across seq assignment → append → sync →
    /// apply, mirroring commit_group. That keeps WAL frame order == seq
    /// order (replay enforces strictly increasing seqs) and apply-before-
    /// ack visibility. Lock order is state → wal, never the reverse.
    pub fn write(&self, ops: &[Op]) -> Result<u64, FormatError> {
        if ops.is_empty() {
            return Err(FormatError::Invalid("empty write batch".into()));
        }
        if self.config.durability == DurabilityMode::GroupCommit {
            let seq = self.writer()?.write(ops)?;
            // The committer applied and (if the threshold fired) flushed
            // before the ack — the L0 count is current. SE2-M10 trigger.
            self.maybe_compact()?;
            return Ok(seq);
        }
        let mut state = self.state.write().unwrap();
        let seq = state.next_seq;
        state.next_seq += 1;
        let frame = encode_frame(seq, ops)?;
        {
            let mut wal = self.wal.lock().unwrap();
            wal.seek(SeekFrom::End(0))
                .map_err(|e| FormatError::Io(format!("WAL seek: {e}")))?;
            wal.write_all(&frame)
                .map_err(|e| FormatError::Io(format!("WAL append: {e}")))?;
            if self.config.durability == DurabilityMode::Sync {
                wal.sync_all()
                    .map_err(|e| FormatError::Io(format!("WAL sync: {e}")))?;
                self.fsyncs.fetch_add(1, Ordering::SeqCst);
            }
        }
        for op in ops {
            match op {
                Op::Put(k, v) => state.active.apply(k.clone(), seq, Some(v.clone())),
                Op::Delete(k) => state.active.apply(k.clone(), seq, None),
                // SE2-M33 — the object ops apply exactly like the byte ops;
                // the rid rides the op, the identity maps already hold the
                // create (acked == visible, the M6 rule).
                Op::PutObject(rid, k, v) => {
                    state
                        .active
                        .apply_object(k.clone(), seq, Some(v.clone()), *rid)
                }
                Op::DeleteObject(rid, k) => state.active.apply_object(k.clone(), seq, None, *rid),
                // SE2-M30 — a create applies its identity BEFORE the ack
                // (acked == durable AND visible, the M6 rule): the maps
                // hold it immediately and the pending records ride the
                // next flush publication.
                Op::CreateObject {
                    oid,
                    lid,
                    rid,
                    pgen,
                } => {
                    merge_identity(&mut state.identity, *oid, *lid)?;
                    state.pending_identity.push(IdentityRecord {
                        oid: *oid,
                        lid: *lid,
                    });
                    merge_replica(&mut state.replicas, *lid, LOCAL_NODE_ID, *rid)?;
                    state.pending_replicas.push(ReplicaRecord {
                        lid: *lid,
                        node: LOCAL_NODE_ID,
                        rid: *rid,
                    });
                    // SE2-M32 — §14: physical placement initially = the
                    // memtable, at the generation reserved in the op.
                    let placement = Placement::Memtable { generation: *pgen };
                    merge_placement(&mut state.placements, *rid, placement)?;
                    state.pending_placements.push(PlacementRecord {
                        rid: *rid,
                        placement,
                    });
                }
            }
        }
        if state.active.bytes() >= self.config.memtable_bytes {
            Self::flush_locked_impl(
                &self.config,
                &self.wal,
                &mut state,
                &self.cache,
                &self.stats,
            )?;
        }
        drop(state);
        self.maybe_compact()?;
        Ok(seq)
    }

    /// SE2-M10 — the L0 trigger: a write path that leaves at least
    /// `l0_compact_trigger` L0 segments compacts them (KeepAll) into one
    /// L1, so the steady state is one L1 + the active L0 and a get
    /// considers ≤ 2 segments. Explicit flush() is the caller's checkpoint
    /// and never triggers. SE2-M16 — the size tier gates the merge: it
    /// only fires once L0's bytes are at least L1's bytes divided by
    /// `l0_tier_ratio` (L1 empty always merges; 0 = count-only M10), so a
    /// growing bulk seed merges at 4, 8, 16, 32… flushes instead of every
    /// 4th — write amplification ~O(n log n) instead of quadratic.
    fn maybe_compact(&self) -> Result<(), FormatError> {
        if self.config.l0_compact_trigger == 0 {
            return Ok(());
        }
        let (l0, l0_bytes, l1_bytes) = {
            let state = self.state.read().unwrap();
            let mut l0 = 0usize;
            let mut l0_bytes = 0u64;
            let mut l1_bytes = 0u64;
            for r in &state.segment_records {
                if r.level == 0 {
                    l0 += 1;
                    l0_bytes += r.file_size;
                } else {
                    l1_bytes += r.file_size;
                }
            }
            (l0, l0_bytes, l1_bytes)
        };
        let triggered = l0 >= self.config.l0_compact_trigger;
        let tier_ok = self.config.l0_tier_ratio == 0
            || l1_bytes == 0
            || l0_bytes >= l1_bytes / self.config.l0_tier_ratio as u64;
        if triggered && tier_ok {
            self.compact()?;
        }
        Ok(())
    }

    /// A shared writer handle for group commit. Only GroupCommit mode has
    /// a commit queue — anything else returns Invalid. Drop every handle
    /// before dropping the Db: the committer exits (and the Db's drop
    /// joins it) only once no sender remains.
    pub fn writer(&self) -> Result<CommitWriter, FormatError> {
        match &self.queue_tx {
            Some(tx) => Ok(CommitWriter { tx: tx.clone() }),
            None => Err(FormatError::Invalid(
                "writer handles require DurabilityMode::GroupCommit".into(),
            )),
        }
    }

    /// Commit fsyncs so far: one per batch (Sync) or one per group
    /// (GroupCommit); none in Async. Flush truncation syncs are not
    /// counted.
    pub fn fsync_count(&self) -> u64 {
        self.fsyncs.load(Ordering::SeqCst)
    }

    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<u64, FormatError> {
        self.write(&[Op::Put(key.to_vec(), value.to_vec())])
    }

    pub fn delete(&self, key: &[u8]) -> Result<u64, FormatError> {
        self.write(&[Op::Delete(key.to_vec())])
    }

    /// SE2-M30 — allocate a new object identity (spec §14 write path,
    /// first half): ObjectId → LogicalId → ReplicaId in ONE WAL frame,
    /// durable per the durability mode, resolvable immediately after the
    /// ack. The ids are reserved under the state lock before the write
    /// commits, so concurrent creates never collide; a crash between
    /// reservation and commit leaves a gap, which is not reuse (§16/§49 —
    /// the allocators only advance). ObjectId = sha256(lid.to_le_bytes())
    /// [..16] — unique by the lid reservation, and batch-safe where a
    /// seq-derived id would collide for two creates in one batch; §6.1's
    /// future distributed generation = a documented per-node/instance
    /// salt.
    pub fn create_object(&self) -> Result<ObjectId, FormatError> {
        let (op, oid) = {
            let mut state = self.state.write().unwrap();
            let (lid, rid, pgen) = Self::reserve_identity(&mut state);
            let digest = sha256(&lid.to_bytes());
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&digest[..16]);
            let oid = ObjectId(bytes);
            (
                Op::CreateObject {
                    oid,
                    lid,
                    rid,
                    pgen,
                },
                oid,
            )
        };
        self.write(&[op])?;
        Ok(oid)
    }

    /// SE2-M33 — reserve the identity triple under the state lock (the
    /// allocators only advance — §16/§49, no reuse): `create_object`
    /// derives its ObjectId from the reserved lid; `put_object`'s
    /// new-object arm stamps the caller's ObjectId onto the reserved
    /// triple.
    fn reserve_identity(state: &mut State) -> (LogicalId, ReplicaId, u64) {
        let lid = LogicalId(state.next_logical_id);
        state.next_logical_id += 1;
        let rid = ReplicaId(state.next_replica_id);
        state.next_replica_id += 1;
        let pgen = state.next_placement_generation;
        state.next_placement_generation += 1;
        (lid, rid, pgen)
    }

    /// SE2-M33 — the §14 write path: PUT resolves the ObjectId through the
    /// §9.1/§9.2 views. An existing object writes under its own ReplicaId;
    /// an unknown one IS the create — the triple is reserved and
    /// Create+Put ride ONE frame (one seq, one fsync — atomic by
    /// construction, deviation 4). Update never allocates (§15 invariant,
    /// by construction: the existing-object arm resolves through views
    /// whose bodies are pure map reads). Two concurrent first-puts of the
    /// same fresh ObjectId fail closed (the second's reserved identity
    /// conflicts with the first's at the merge gate).
    pub fn put_object(&self, oid: ObjectId, key: &[u8], value: &[u8]) -> Result<u64, FormatError> {
        let ops = match LocalIdentityDirectory::new(self).resolve(oid)? {
            Some(lid) => {
                let rid = LocalReplicaDirectory::new(self)
                    .resolve_local(lid)?
                    .ok_or_else(|| {
                        FormatError::Corrupt(format!("logical {lid:?} has no local replica"))
                    })?;
                vec![Op::PutObject(rid, key.to_vec(), value.to_vec())]
            }
            None => {
                let (lid, rid, pgen) = {
                    let mut state = self.state.write().unwrap();
                    Self::reserve_identity(&mut state)
                };
                vec![
                    Op::CreateObject {
                        oid,
                        lid,
                        rid,
                        pgen,
                    },
                    Op::PutObject(rid, key.to_vec(), value.to_vec()),
                ]
            }
        };
        self.write(&ops)
    }

    /// SE2-M33 — the §16 delete path: the tombstone carries the object's
    /// own rid, so its identity metadata survives (the directories keep
    /// the mapping; placement eventually Retired — M35). Deleting an
    /// unknown ObjectId is caller misuse — fail closed.
    pub fn delete_object(&self, oid: ObjectId, key: &[u8]) -> Result<u64, FormatError> {
        let lid = LocalIdentityDirectory::new(self)
            .resolve(oid)?
            .ok_or_else(|| {
                FormatError::Invalid(format!("delete_object on unknown ObjectId {oid:?}"))
            })?;
        let rid = LocalReplicaDirectory::new(self)
            .resolve_local(lid)?
            .ok_or_else(|| FormatError::Corrupt(format!("logical {lid:?} has no local replica")))?;
        self.write(&[Op::DeleteObject(rid, key.to_vec())])
    }

    /// SE2-M33/M34 — the §13 read path: resolve → read through the identity
    /// filter. The memtable probe matches the object's OWN entries
    /// (replica_id == rid) — a byte-API row at the same key is another
    /// layer's data and never answers an object read (§11). SE2-M34 lifted
    /// the M33 fail-closed boundary: v3 segments answer through the
    /// rid-filtered bounded probe, newest-first; the object's own tombstone
    /// on disk shadows (None), and v1/v2 segments hold no rid rows.
    pub fn get_object(&self, oid: ObjectId, key: &[u8]) -> Result<Option<Vec<u8>>, FormatError> {
        let Some(lid) = LocalIdentityDirectory::new(self).resolve(oid)? else {
            return Ok(None); // never created — not a read error
        };
        let rid = LocalReplicaDirectory::new(self)
            .resolve_local(lid)?
            .ok_or_else(|| FormatError::Corrupt(format!("logical {lid:?} has no local replica")))?;
        // SE2-M10 — the state guard covers the memtable probes and the
        // segments arc clone; the disk probes run after the guard drops.
        let (value, segments) = {
            let state = self.state.read().unwrap();
            if let Some(e) = state.active.get_by_rid(key, rid) {
                return Ok(e.value.clone());
            }
            for mem in state.immutables.iter().rev() {
                if let Some(e) = mem.get_by_rid(key, rid) {
                    return Ok(e.value.clone());
                }
            }
            (None, Arc::clone(&state.segments))
        };
        for seg in segments.iter().rev() {
            if let Some(e) = seg.get_by_rid(key, rid)? {
                return Ok(if e.flags & FLAG_DELETE != 0 {
                    None
                } else {
                    Some(e.value)
                });
            }
        }
        Ok(value)
    }

    /// SE2-M30 — ObjectId → LogicalId (spec §9.1, the Db-level surface;
    /// the resolver abstractions wrap this in SE2-M31). In-memory: the
    /// maps are rebuilt at open from the delta logs + the active WAL, and
    /// every create applies before its ack.
    pub fn resolve_object(&self, oid: ObjectId) -> Option<LogicalId> {
        self.state.read().unwrap().identity.get(&oid).copied()
    }

    /// SE2-M31 — the local replica of a logical id (spec §9.2): every
    /// create reserves lid → rid 1:1, so a resolved logical always has one
    /// local replica. The topology views delegate here.
    pub(crate) fn resolve_local(&self, lid: LogicalId) -> Option<ReplicaId> {
        self.state.read().unwrap().replicas.get(&lid).copied()
    }

    /// SE2-M32 — the placement of a local replica (spec §9.3): created
    /// replicas carry Memtable placement from birth (§14); flush and
    /// compaction move it. The resolver view delegates here.
    pub(crate) fn resolve_placement(&self, rid: ReplicaId) -> Option<Placement> {
        self.state.read().unwrap().placements.get(&rid).copied()
    }

    /// Newest layer wins: active → immutables → segments (all newest
    /// first). A tombstone in a newer layer shadows an older value.
    /// SE2-M8: the Db-level read-path counters are recorded here — the
    /// segment-level counters (I/O, decode) fire inside the reader.
    /// SE2-M10: the state guard covers only the memtable probes and the
    /// arc clone — the segment probes (bloom, index, disk reads) run after
    /// the guard is dropped, so a cold get never stalls a writer.
    /// SE2-M21: `get_wall_ns` times the whole call (the attribution
    /// denominator), `lock_wait_ns` the state-guard wait, `bloom_probe_ns`
    /// the bloom pre-check; the memtable timer covers the value clone too —
    /// the clone is get work, not attribution residual.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, FormatError> {
        let t_wall = Instant::now();
        let out = self.get_inner(key);
        self.stats
            .get_wall_ns
            .fetch_add(t_wall.elapsed().as_nanos() as u64, Ordering::Relaxed);
        out
    }

    /// SE2-M25 — batch point lookups, answers parallel to the input. Dedups
    /// the key set (`lookups` counts unique keys — duplicates share one
    /// lookup; the M21 accounting pins measure `get` only and stay
    /// untouched), one state guard for the whole batch, one key hash per
    /// unique key across its segment blooms, and one block fetch per block
    /// via `SegmentReader::get_many` — the same resolution rules as `get`
    /// (newest PUT wins, a DELETE shadows everything older).
    pub fn get_many(&self, keys: &[&[u8]]) -> Result<Vec<Option<Vec<u8>>>, FormatError> {
        let t_wall = Instant::now();
        let out = self.get_many_inner(keys);
        self.stats
            .get_wall_ns
            .fetch_add(t_wall.elapsed().as_nanos() as u64, Ordering::Relaxed);
        out
    }

    fn get_many_inner(&self, keys: &[&[u8]]) -> Result<Vec<Option<Vec<u8>>>, FormatError> {
        let mut first_pos: HashMap<&[u8], usize> = HashMap::with_capacity(keys.len());
        let mut unique: Vec<usize> = Vec::with_capacity(keys.len());
        for (pos, key) in keys.iter().enumerate() {
            if !first_pos.contains_key(key) {
                first_pos.insert(key, pos);
                unique.push(pos);
            }
        }
        self.stats
            .lookups
            .fetch_add(unique.len() as u64, Ordering::Relaxed);

        let mut answers: Vec<Option<Vec<u8>>> = vec![None; keys.len()];
        let (mut remaining, segments) = {
            let t_lock = Instant::now();
            let state = self.state.read().unwrap();
            self.stats
                .lock_wait_ns
                .fetch_add(t_lock.elapsed().as_nanos() as u64, Ordering::Relaxed);
            let t0 = Instant::now();
            let mut remaining = Vec::with_capacity(unique.len());
            for pos in unique {
                let hit = state
                    .active
                    .get(keys[pos])
                    .or_else(|| state.immutables.iter().rev().find_map(|m| m.get(keys[pos])));
                if let Some(e) = hit {
                    self.stats.memtable_hits.fetch_add(1, Ordering::Relaxed);
                    answers[pos] = e.value.clone(); // None = memtable tombstone
                } else {
                    remaining.push(pos);
                }
            }
            self.stats
                .memtable_lookup_ns
                .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
            (remaining, Arc::clone(&state.segments))
        };
        // SE2-M22 parity — one key hash per unique key, shared by every
        // segment's bloom probe.
        let mut bloom_hashes: HashMap<usize, (u64, u64)> = HashMap::new();
        for seg in segments.iter().rev() {
            self.stats
                .segments_considered
                .fetch_add(1, Ordering::Relaxed);
            let mut wanted: Vec<usize> = Vec::with_capacity(remaining.len());
            for &pos in &remaining {
                let key = keys[pos];
                if key < seg.key_min() || key > seg.key_max() {
                    self.stats
                        .segments_range_skipped
                        .fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let t_bloom = Instant::now();
                let (h1, h2) = *bloom_hashes
                    .entry(pos)
                    .or_insert_with(|| SegmentReader::bloom_hashes(key));
                let may = seg.bloom_may_contain_hashes(h1, h2);
                self.stats
                    .bloom_probe_ns
                    .fetch_add(t_bloom.elapsed().as_nanos() as u64, Ordering::Relaxed);
                if !may {
                    self.stats
                        .segments_bloom_skipped
                        .fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                wanted.push(pos);
            }
            if wanted.is_empty() {
                continue;
            }
            self.stats
                .segments_index_searched
                .fetch_add(wanted.len() as u64, Ordering::Relaxed);
            let wanted_keys: Vec<&[u8]> = wanted.iter().map(|&p| keys[p]).collect();
            let found = seg.get_many(&wanted_keys)?;
            for (pos, e) in wanted.into_iter().zip(found) {
                if let Some(e) = e {
                    answers[pos] = if e.flags & FLAG_DELETE != 0 {
                        None // tombstone: shadow everything older
                    } else {
                        Some(e.value)
                    };
                    // ponytail: linear retain per resolution — fine at batch
                    // sizes (W4 fan-outs ≤ 1000)
                    remaining.retain(|&p| p != pos);
                }
            }
        }
        // duplicates answered from their first position's lookup
        for (pos, key) in keys.iter().enumerate() {
            if let Some(&first) = first_pos.get(key) {
                if first != pos {
                    answers[pos] = answers[first].clone();
                }
            }
        }
        Ok(answers)
    }

    fn get_inner(&self, key: &[u8]) -> Result<Option<Vec<u8>>, FormatError> {
        self.stats.lookups.fetch_add(1, Ordering::Relaxed);
        let segments = {
            let t_lock = Instant::now();
            let state = self.state.read().unwrap();
            self.stats
                .lock_wait_ns
                .fetch_add(t_lock.elapsed().as_nanos() as u64, Ordering::Relaxed);
            let t0 = Instant::now();
            if let Some(e) = state.active.get(key) {
                let value = e.value.clone();
                self.stats
                    .memtable_lookup_ns
                    .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
                self.stats.memtable_hits.fetch_add(1, Ordering::Relaxed);
                return Ok(value);
            }
            for mem in state.immutables.iter().rev() {
                if let Some(e) = mem.get(key) {
                    let value = e.value.clone();
                    self.stats
                        .memtable_lookup_ns
                        .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    self.stats.memtable_hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(value);
                }
            }
            self.stats
                .memtable_lookup_ns
                .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
            Arc::clone(&state.segments)
        };
        // SE2-M22 — one key hash per get, shared by every segment's bloom
        // probe (the per-segment re-hash was ~5-7 sha256s per get). Computed
        // inside the first segment's probe timer so the bloom row keeps
        // meaning "all bloom work for this get" (the M21 accounting pins).
        let mut bloom_hash: Option<(u64, u64)> = None;
        for seg in segments.iter().rev() {
            self.stats
                .segments_considered
                .fetch_add(1, Ordering::Relaxed);
            // SE2-M9 — key-range skip: a segment whose [key_min, key_max]
            // excludes the target cannot hold it; skip before the bloom is
            // even probed. Considered counts the iteration.
            if key < seg.key_min() || key > seg.key_max() {
                self.stats
                    .segments_range_skipped
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            }
            // SE2-M7 — bloom pre-check: false positives possible, false
            // negatives never (M1 pin), so skipping a segment the bloom
            // rejects is answer-preserving; it just saves the probe.
            // SE2-M22 — one key hash per get, shared across segments.
            let t_bloom = Instant::now();
            let (h1, h2) = *bloom_hash.get_or_insert_with(|| SegmentReader::bloom_hashes(key));
            let may = seg.bloom_may_contain_hashes(h1, h2);
            self.stats
                .bloom_probe_ns
                .fetch_add(t_bloom.elapsed().as_nanos() as u64, Ordering::Relaxed);
            if !may {
                self.stats
                    .segments_bloom_skipped
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            }
            self.stats
                .segments_index_searched
                .fetch_add(1, Ordering::Relaxed);
            if let Some(e) = seg.get(key)? {
                return Ok(if e.flags & FLAG_DELETE != 0 {
                    None // tombstone: shadow everything older
                } else {
                    Some(e.value)
                });
            }
        }
        Ok(None)
    }

    /// SE2-M8 — cumulative read-path counters (the QA spec's truth layer).
    /// Db-level counters cover `get` only; the segment-level counters cover
    /// every block load the Db's readers serve (scans and compaction
    /// included — a compaction's I/O is not a point read).
    pub fn read_path_stats(&self) -> ReadPathStats {
        self.stats.snapshot()
    }

    /// SE2-M7 — block cache metrics; all zeros when the cache is off
    /// (cache_bytes = 0).
    pub fn cache_stats(&self) -> CacheStats {
        self.cache.as_ref().map(|c| c.stats()).unwrap_or_default()
    }

    /// V2-Adopt — prefix scan, the kernel `StorageEngine` contract: keys
    /// sorted ascending, restricted to [prefix, prefix+∞), ONE entry per
    /// key — the newest layer's head (same layer order as `get`: active →
    /// immutables → segments, newest first). A tombstone in a newer layer
    /// shadows every older value: the key does not appear. Distinct keys
    /// only — the kernel stores history as distinct (koid, ts) keys, so
    /// head-per-key loses nothing.
    pub fn scan(&self, prefix: &[u8]) -> Result<Vec<ScanRow>, FormatError> {
        let end = prefix_end(prefix);
        // SE2-M12 — a k-way merge over the layer streams, oldest first
        // (segments in manifest order, immutables, active): for a key the
        // entry from the NEWEST stream holding it wins; a tombstone
        // suppresses the key. Memtable heads are collected to owned runs
        // under the guard; the segment cursors ride the arc snapshot and
        // decode one entry at a time (SegmentScan) — no whole-block Vec,
        // no BTreeMap of every prefix key, no state lock across segment I/O.
        let (segs, mem_runs, act_run) = {
            let state = self.state.read().unwrap();
            let segs = Arc::clone(&state.segments);
            let mut mem_runs: Vec<Vec<MergeEntry>> = Vec::new();
            for mem in state.immutables.iter() {
                mem_runs.push(
                    mem.prefix_heads(prefix)
                        .map(|(k, e)| (k.to_vec(), e.value.clone()))
                        .collect(),
                );
            }
            let act_run: Vec<MergeEntry> = state
                .active
                .prefix_heads(prefix)
                .map(|(k, e)| (k.to_vec(), e.value.clone()))
                .collect();
            (segs, mem_runs, act_run)
        };
        let mut streams: Vec<ScanStream<'_>> = Vec::new();
        for seg in segs.iter() {
            streams.push(ScanStream::Seg(seg.scan_iter(prefix, end.as_deref())));
        }
        for run in mem_runs {
            streams.push(ScanStream::Mem(run.into_iter()));
        }
        streams.push(ScanStream::Mem(act_run.into_iter()));

        // Min-heap of (Reverse<key>, stream_idx, value) — one head per
        // stream; equal keys drain together and the max-index (newest
        // layer) entry wins.
        let mut heap: std::collections::BinaryHeap<HeapEntry> = std::collections::BinaryHeap::new();
        for (i, s) in streams.iter_mut().enumerate() {
            if let Some((k, v)) = s.next()? {
                heap.push((Reverse(k), i, v));
            }
        }
        let mut out = Vec::new();
        while let Some((Reverse(k), i, v)) = heap.pop() {
            let mut drained = vec![(i, v)];
            while let Some((Reverse(k2), _, _)) = heap.peek() {
                if k2.as_slice() != k.as_slice() {
                    break;
                }
                let (_, i, v) = heap.pop().expect("peeked");
                drained.push((i, v));
            }
            let (_, win_v) = drained
                .iter()
                .cloned()
                .max_by_key(|(i, _)| *i)
                .expect("drained non-empty");
            for (i, _) in &drained {
                if let Some((nk, nv)) = streams[*i].next()? {
                    heap.push((Reverse(nk), *i, nv));
                }
            }
            if let Some(v) = win_v {
                out.push((k, v));
            }
        }
        Ok(out)
    }

    /// Flush's first half, public so the visibility contract is testable:
    /// the active memtable becomes immutable (reads keep seeing it) and a
    /// fresh active takes new writes. flush() = rotate + publish.
    pub fn rotate(&self) {
        let mut state = self.state.write().unwrap();
        if state.active.is_empty() {
            return;
        }
        let fresh = std::mem::take(&mut state.active);
        state.immutables.push(fresh);
    }

    pub fn flush(&self) -> Result<(), FormatError> {
        let mut state = self.state.write().unwrap();
        Self::flush_locked_impl(
            &self.config,
            &self.wal,
            &mut state,
            &self.cache,
            &self.stats,
        )
    }

    /// Publication order (every crash window recoverable — see module doc):
    /// segment files → manifest → CURRENT → WAL truncate. Shared with the
    /// group-commit committer — it takes the pieces, not the Db.
    fn flush_locked_impl(
        config: &Config,
        wal: &Arc<Mutex<File>>,
        state: &mut State,
        cache: &Option<Arc<BlockCache>>,
        stats: &Arc<Stats>,
    ) -> Result<(), FormatError> {
        if !state.active.is_empty() {
            let fresh = std::mem::take(&mut state.active);
            state.immutables.push(fresh);
        }
        if state.immutables.is_empty() {
            return Ok(());
        }
        let mut new_segments = Vec::with_capacity(state.immutables.len());
        let mut anchors: HashMap<ReplicaId, (u64, SegmentId, BlockId, u32)> = HashMap::new();
        for mem in state.immutables.drain(..) {
            let id = state.next_segment_id;
            state.next_segment_id += 1;
            let path = segment_path(&config.dir, id);
            // SE2-M34 — identity-carrying immutables become v3 blocks (rid
            // per entry); pure byte-API ones stay v2, byte-identical to M9.
            let mut writer = if mem.has_identity() {
                SegmentWriter::new_v3(config.block_target)
            } else {
                SegmentWriter::new_v2(config.block_target)
            };
            // into_entries: the flushed table is consumed — keys/values
            // move into the writer, no second copy (SE2-M15).
            for ((key, seq), e) in mem.into_entries() {
                let flags = if e.value.is_some() {
                    FLAG_PUT
                } else {
                    FLAG_DELETE
                };
                writer.push(SegmentEntry {
                    key,
                    value: e.value.unwrap_or_default(),
                    seq,
                    flags,
                    replica_id: e.replica_id,
                });
            }
            let (file_size, checksum, seg_anchors) = writer.publish_with_anchors(&path)?;
            // One flushed replica may span several immutables (rotates
            // between writes): the max-seq anchor across ALL segments this
            // flush writes wins.
            for a in seg_anchors {
                let slot = (a.seq, SegmentId(id), a.block_id, a.entry_offset);
                match anchors.get(&a.replica_id) {
                    Some(&(best_seq, ..)) if best_seq >= a.seq => {}
                    _ => {
                        anchors.insert(a.replica_id, slot);
                    }
                }
            }
            let reader = SegmentReader::open_with(&path, cache.clone(), Some(Arc::clone(stats)))?;
            let record = SegmentRecord {
                segment_id: id,
                level: 0,
                key_min: reader.key_min().to_vec(),
                key_max: reader.key_max().to_vec(),
                seq_lo: reader.seq_lo(),
                seq_hi: reader.seq_hi(),
                record_count: reader.entry_count(),
                file_size,
                checksum,
            };
            state.segment_records.push(record);
            new_segments.push(Arc::new(reader));
        }
        // SE2-M34 — every flushed replica publishes its Segment placement
        // in the SAME window as its segment (the §23 order): the anchor is
        // the replica's max-seq entry location, the record's generation
        // fresh (§32: a move IS a new generation).
        // SE2-M35 — deterministic generation order: identical workloads
        // must allocate identical generations (the crash-pin replays and
        // any future cross-node comparison compare them).
        let mut ordered_anchors: Vec<_> = anchors.into_iter().collect();
        ordered_anchors.sort_by_key(|&(rid, _)| rid);
        for (rid, (_seq, segment_id, block_id, entry_offset)) in ordered_anchors {
            let pgen = state.next_placement_generation;
            state.next_placement_generation += 1;
            let placement = Placement::Segment(PhysicalLocation {
                segment_id,
                block_id,
                entry_offset,
                generation: pgen,
            });
            merge_placement(&mut state.placements, rid, placement)?;
            state
                .pending_placements
                .push(PlacementRecord { rid, placement });
        }
        state.generation += 1;
        // SE2-M30/M32 — the pending identity/replica/placement deltas
        // publish in the SAME window as the segments they accompany,
        // before the manifest names the generation (the §23 order): a
        // crash before CURRENT leaves orphan logs + the full WAL (replay
        // rebuilds — the merge rules are idempotent), a crash after
        // CURRENT re-applies identical records from both. A generation
        // with no directory work publishes no log — gaps are normal.
        if !state.pending_identity.is_empty() {
            let log = IdentityLog {
                format_version: FORMAT_VERSION,
                generation: state.generation,
                records: std::mem::take(&mut state.pending_identity),
            };
            IdentityLog::publish(&identity_log_path(&config.dir, state.generation), &log)?;
        }
        if !state.pending_replicas.is_empty() {
            let log = ReplicaLog {
                format_version: FORMAT_VERSION,
                generation: state.generation,
                records: std::mem::take(&mut state.pending_replicas),
            };
            ReplicaLog::publish(&replica_log_path(&config.dir, state.generation), &log)?;
        }
        if !state.pending_placements.is_empty() {
            let log = PlacementLog {
                format_version: FORMAT_VERSION,
                generation: state.generation,
                records: std::mem::take(&mut state.pending_placements),
            };
            PlacementLog::publish(&placement_log_path(&config.dir, state.generation), &log)?;
        }
        crash_park("AIKOQL_V2_FLUSH_PARK", &config.dir, "after_identity");
        let manifest = Manifest {
            format_version: FORMAT_VERSION,
            generation: state.generation,
            segments: state.segment_records.clone(),
            wal_ids: vec![],
        };
        Manifest::publish(&manifest_path(&config.dir, state.generation), &manifest)?;
        Current::publish(
            &config.dir.join("CURRENT"),
            &Current::new(FORMAT_VERSION, state.generation),
        )?;
        {
            let wal = wal.lock().unwrap();
            wal.set_len(0)
                .map_err(|e| FormatError::Io(format!("WAL truncate: {e}")))?;
            wal.sync_all()
                .map_err(|e| FormatError::Io(format!("WAL sync: {e}")))?;
        }
        // make_mut: in-flight gets holding a clone keep their snapshot —
        // the new segments land in the fresh vec only.
        Arc::make_mut(&mut state.segments).extend(new_segments);
        Ok(())
    }

    /// SE2-M4 — L0 → L1 compaction: merge ALL segments (L0 + L1) into one
    /// L1 segment, per key only the newest entry survives, a tombstone
    /// drops the key (L1 is the bottom level). Synchronous — deterministic
    /// correctness over a background thread, the doc's own call for flush;
    /// a trigger threshold arrives when measurements justify one.
    /// Publication order mirrors flush (segment → manifest → CURRENT →
    /// delete obsolete) so every crash window recovers the SAME logical
    /// state — compaction is state-preserving. Memtables are not
    /// compaction material: they are newer than every segment and read
    /// first anyway.
    pub fn compact(&self) -> Result<CompactStats, FormatError> {
        // PR#2 review SE-05: with write(&self), two callers can pass
        // maybe_compact's trigger check concurrently and both merge.
        // compact_with serializes them on the state lock; this pre-check
        // skips the common redundant single-segment re-merge. Advisory, not
        // atomic — missing it costs one redundant merge, never correctness —
        // so the policy path (compact_with on a single segment is a caller's
        // explicit request) keeps its own is_empty guard.
        if self.state.read().unwrap().segments.len() <= 1 {
            return Ok(CompactStats::default());
        }
        self.compact_with(&KeepAll)
    }

    /// Compact with a retention policy (SE2-M5): KEEP/DROP/ARCHIVE per
    /// key class. The policy is an input — the caller asserts which rows
    /// are genuinely obsolete (superseded heads, tombstoned keys); the
    /// engine stays key-space-generic. ARCHIVE rows land in
    /// `archive/ARCHIVE-{id:06}.seg` and leave the live key space.
    /// SE2-M35 — Segment-placed replicas relocate with the merge (design
    /// §21–25): the relocation set names each one's new home (the
    /// surviving max-seq entry's anchor, or Retired when nothing of it
    /// survived the live key space), the placement log publishes in the
    /// §23 window before the manifest, and the in-memory placements swap
    /// only after CURRENT.
    pub fn compact_with(&self, policy: &dyn RetentionPolicy) -> Result<CompactStats, FormatError> {
        let mut state = self.state.write().unwrap();
        if state.segments.is_empty() {
            return Ok(CompactStats::default());
        }
        let mut next_id = state.next_segment_id;
        let attach = SegmentAttach {
            cache: self.cache.clone(),
            stats: Some(Arc::clone(&self.stats)),
        };
        let (stats, chunks, relocations) = merge(
            &state.segments,
            self.config.block_target,
            self.config.merge_chunk_bytes,
            &self.config.dir,
            &mut next_id,
            policy,
            &attach,
        )?;
        crash_park("AIKOQL_V2_COMPACT_PARK", &self.config.dir, "after_segment");

        // SE2-M35 — every Segment-placed replica relocates: a fresh §25
        // generation per move, the relocation set's anchor as the new home,
        // Retired when the merge dropped the replica's last live entry.
        // Memtable-placed replicas keep theirs — the next flush moves them.
        let mut placement_records = Vec::new();
        let mut segment_rids: Vec<ReplicaId> = state
            .placements
            .iter()
            .filter_map(|(&rid, p)| matches!(p, Placement::Segment(_)).then_some(rid))
            .collect();
        // SE2-M35 — sorted, so the fresh generations assign deterministically.
        segment_rids.sort_unstable();
        for rid in segment_rids {
            let pgen = state.next_placement_generation;
            state.next_placement_generation += 1;
            let relocated = match relocations.get(&rid) {
                Some(Some(loc)) => Placement::Segment(PhysicalLocation {
                    segment_id: loc.0,
                    block_id: loc.1,
                    entry_offset: loc.2,
                    generation: pgen,
                }),
                Some(None) => Placement::Retired { generation: pgen },
                // Fail closed: the merge saw every entry of every input
                // segment, so a Segment-placed replica missing from the set
                // means the placement predates the input — never relocate
                // what cannot be proven.
                None => {
                    return Err(FormatError::Corrupt(format!(
                        "segment-placed replica {rid:?} absent from the relocation set"
                    )))
                }
            };
            placement_records.push(PlacementRecord {
                rid,
                placement: relocated,
            });
        }

        let old_paths: Vec<PathBuf> = state
            .segment_records
            .iter()
            .map(|r| segment_path(&self.config.dir, r.segment_id))
            .collect();
        let mut new_records = Vec::new();
        let mut new_segments = Vec::new();
        for (segment_id, (reader, file_size, checksum), _anchors) in chunks {
            new_records.push(SegmentRecord {
                segment_id,
                level: 1,
                key_min: reader.key_min().to_vec(),
                key_max: reader.key_max().to_vec(),
                seq_lo: reader.seq_lo(),
                seq_hi: reader.seq_hi(),
                record_count: reader.entry_count(),
                file_size,
                checksum,
            });
            new_segments.push(Arc::new(reader));
        }
        state.next_segment_id = next_id;
        state.generation += 1;
        // SE2-M35 — the relocation records publish at the NEW generation,
        // before the manifest names it (the §23 order, mirroring flush):
        // state-C — log durable, manifest not — keeps the old placements
        // authoritative (the new log is an orphan past CURRENT), state-D
        // applies them on reopen. A compaction that relocated nothing
        // publishes no log — gaps are normal.
        if !placement_records.is_empty() {
            let log = PlacementLog {
                format_version: FORMAT_VERSION,
                generation: state.generation,
                records: placement_records.clone(),
            };
            PlacementLog::publish(
                &placement_log_path(&self.config.dir, state.generation),
                &log,
            )?;
        }
        crash_park("AIKOQL_V2_COMPACT_PARK", &self.config.dir, "after_location");
        let manifest = Manifest {
            format_version: FORMAT_VERSION,
            generation: state.generation,
            segments: new_records.clone(),
            wal_ids: vec![],
        };
        Manifest::publish(
            &manifest_path(&self.config.dir, state.generation),
            &manifest,
        )?;
        crash_park("AIKOQL_V2_COMPACT_PARK", &self.config.dir, "after_manifest");
        Current::publish(
            &self.config.dir.join("CURRENT"),
            &Current::new(FORMAT_VERSION, state.generation),
        )?;
        crash_park("AIKOQL_V2_COMPACT_PARK", &self.config.dir, "after_current");

        // Swap readers before deleting: handles open with share-delete, so
        // Windows marks the files delete-pending and any reader that still
        // references an obsolete segment keeps its data alive (the
        // Arc<Segment> lifetime guarantee, via the OS).
        state.segments = Arc::new(new_segments);
        state.segment_records = new_records;
        for rec in &placement_records {
            // Infallible in practice — the records carry fresh generations —
            // but the gate stays the one path in.
            merge_placement(&mut state.placements, rec.rid, rec.placement)?;
        }
        for p in &old_paths {
            if let Err(e) = std::fs::remove_file(p) {
                // Not fatal: the segment is unreferenced — a leftover is
                // reported and ignored at the next open.
                eprintln!(
                    "aikoql-v2: obsolete segment {} not removed: {e}",
                    p.display()
                );
            }
        }
        Ok(stats)
    }
}

/// SE2-M4 crash-matrix hook: parks forever only when the env names this
/// stage, so the child-kill harness can kill the process mid-compaction
/// and pin the §25 windows. Unset in production — a no-op.
impl Drop for Db {
    fn drop(&mut self) {
        // GroupCommit mode: drop the Db's own sender so the queue
        // disconnects once every CommitWriter handle is gone, let the
        // committer commit what is still pending, and join it — a reopen
        // of the same directory must never race the old committer's last
        // group. (CommitWriter's doc spells out the drop-order rule.)
        self.queue_tx = None;
        if let Some(handle) = self.committer.take() {
            let _ = handle.join();
        }
    }
}

/// Park forever when `var` names this stage — the crash-window harness
/// (no-op unset). The marker file tells the parent the park was reached.
fn crash_park(var: &str, dir: &Path, stage: &str) {
    if std::env::var(var).ok().as_deref() != Some(stage) {
        return;
    }
    std::fs::write(dir.join(stage), b"1").ok();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

/// A shared writer handle (GroupCommit mode): batches submitted through
/// handles queue up and commit as groups — one fsync per group, applied
/// and acked in submission order. Clone cheaply for one handle per
/// writer thread; drop every handle before dropping the Db.
#[derive(Clone)]
pub struct CommitWriter {
    tx: mpsc::Sender<Batch>,
}

impl CommitWriter {
    /// Submit one batch and block until its group commits: the returned
    /// seq is assigned in submission order, and the ack fires only after
    /// the batch is durable (group fsync) AND visible (applied).
    pub fn write(&self, ops: &[Op]) -> Result<u64, FormatError> {
        if ops.is_empty() {
            return Err(FormatError::Invalid("empty write batch".into()));
        }
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        self.tx
            .send((ops.to_vec(), ack_tx))
            .map_err(|_| FormatError::Io("commit queue closed".into()))?;
        match ack_rx.recv() {
            Ok(result) => result,
            Err(_) => Err(FormatError::Io("commit queue closed".into())),
        }
    }
}

fn batch_ops_of(b: &Batch) -> usize {
    b.0.len()
}

/// The engine's byte accounting for the cap: the sum over ops of
/// key+value bytes (a Delete carries only its key).
/// The byte successor of `prefix` — the exclusive end bound of the prefix
/// range. None when the prefix is empty or overflows (all 0xFF): the range
/// is then unbounded. Kernel keys may hold arbitrary byte values (a KOID's
/// 16 bytes are opaque), so an ASCII sentinel like b"~" would silently
/// drop high-byte keys from a full scan.
fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    if prefix.is_empty() {
        return None;
    }
    let mut end = prefix.to_vec();
    for b in end.iter_mut().rev() {
        *b = b.wrapping_add(1);
        if *b != 0 {
            return Some(end);
        }
    }
    None
}

/// SE2-M12 — one scan merge stream: a lazy segment cursor or an owned run
/// of memtable heads (key asc, one entry per key, None = tombstone).
enum ScanStream<'a> {
    Seg(crate::segment::SegmentScan<'a>),
    Mem(std::vec::IntoIter<MergeEntry>),
}

/// A stream's next row: key and value (None = tombstone).
type MergeEntry = (Vec<u8>, Option<Vec<u8>>);

/// One merge heap element: reverse key (min-first), stream index, value.
type HeapEntry = (Reverse<Vec<u8>>, usize, Option<Vec<u8>>);

impl ScanStream<'_> {
    fn next(&mut self) -> Result<Option<MergeEntry>, FormatError> {
        match self {
            ScanStream::Seg(s) => match s.next() {
                Some(Ok(e)) => Ok(Some((
                    e.key,
                    if e.flags & FLAG_DELETE != 0 {
                        None
                    } else {
                        Some(e.value)
                    },
                ))),
                Some(Err(e)) => Err(e),
                None => Ok(None),
            },
            ScanStream::Mem(m) => Ok(m.next()),
        }
    }
}

fn batch_bytes_of(b: &Batch) -> usize {
    b.0.iter()
        .map(|op| match op {
            Op::Put(k, v) => k.len() + v.len(),
            Op::Delete(k) => k.len(),
            // SE2-M30/M32 — the fixed payload width (oid 16 + lid 8 +
            // rid 8 + pgen 8), for the group-cap accounting.
            Op::CreateObject { .. } => 40,
            // SE2-M33 — rid 8 + key (+ value), the §17/§18 op shape.
            Op::PutObject(_, k, v) => 8 + k.len() + v.len(),
            Op::DeleteObject(_, k) => 8 + k.len(),
        })
        .sum()
}

/// SE2-M30 — the identity/replica merge rule (§24 crash windows): an
/// identical repeat (a crash after CURRENT but before WAL truncation —
/// the record arrives from both the log and the replay) is a no-op; a
/// CONFLICTING repeat (same ObjectId, different LogicalId) fails closed —
/// exactly one authoritative mapping, never two.
fn merge_identity(
    identity: &mut HashMap<ObjectId, LogicalId>,
    oid: ObjectId,
    lid: LogicalId,
) -> Result<(), FormatError> {
    match identity.entry(oid) {
        std::collections::hash_map::Entry::Occupied(e) if *e.get() == lid => Ok(()),
        std::collections::hash_map::Entry::Occupied(e) => Err(FormatError::Corrupt(format!(
            "identity directory: ObjectId {oid} maps to both {} and {}",
            e.get().0,
            lid.0
        ))),
        std::collections::hash_map::Entry::Vacant(v) => {
            v.insert(lid);
            Ok(())
        }
    }
}

/// SE2-M30 — the replica merge rule (same §24 shape, keyed by LogicalId).
/// The MVP is single-node: a record naming any other node comes from a
/// topology this build cannot place — fail closed (the M9 policy).
fn merge_replica(
    replicas: &mut HashMap<LogicalId, ReplicaId>,
    lid: LogicalId,
    node: NodeId,
    rid: ReplicaId,
) -> Result<(), FormatError> {
    if node != LOCAL_NODE_ID {
        return Err(FormatError::Corrupt(format!(
            "replica directory: record names {node:?}, this build only knows {LOCAL_NODE_ID:?}"
        )));
    }
    match replicas.entry(lid) {
        std::collections::hash_map::Entry::Occupied(e) if *e.get() == rid => Ok(()),
        std::collections::hash_map::Entry::Occupied(e) => Err(FormatError::Corrupt(format!(
            "replica directory: LogicalId {} maps to both {} and {}",
            e.key().0,
            e.get().0,
            rid.0
        ))),
        std::collections::hash_map::Entry::Vacant(v) => {
            v.insert(rid);
            Ok(())
        }
    }
}

/// The committer: drain the queue into groups bounded by the caps and
/// the wait window, commit each group with ONE fsync, apply, ack. Exits
/// when every sender is gone and nothing is pending.
fn committer_loop(
    rx: mpsc::Receiver<Batch>,
    wal: Arc<Mutex<File>>,
    state: Arc<RwLock<State>>,
    config: Config,
    fsyncs: Arc<AtomicU64>,
    cache: Option<Arc<BlockCache>>,
    stats: Arc<Stats>,
) {
    let wait = config.max_wait_duration;
    let mut carry: Option<Batch> = None;
    loop {
        let first = match carry.take().or_else(|| rx.recv().ok()) {
            Some(b) => b,
            None => return, // all senders dropped, nothing pending
        };
        let mut group = vec![first];
        let deadline = Instant::now() + wait;
        loop {
            // Sum over the whole group — groups are small; exact-fit caps.
            let (ops_n, bytes_n) = group.iter().fold((0usize, 0usize), |(o, b), batch| {
                (o + batch_ops_of(batch), b + batch_bytes_of(batch))
            });
            if ops_n >= config.max_batch_ops || bytes_n >= config.max_batch_bytes {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(batch) => {
                    if ops_n + batch_ops_of(&batch) > config.max_batch_ops
                        || bytes_n + batch_bytes_of(&batch) > config.max_batch_bytes
                    {
                        carry = Some(batch); // exact fit: leads the next group
                        break;
                    }
                    group.push(batch);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        commit_group(&group, &wal, &state, &config, &fsyncs, &cache, &stats);
    }
}

/// Commit one group: assign seqs, append every frame, ONE fsync, apply,
/// ack — all under one state write-lock, exactly like Sync's write()
/// (SE-05), so a flush can never interleave the append-and-apply window.
/// Lock order is always state → wal, and the wal lock is never held
/// across a flush.
fn commit_group(
    group: &[Batch],
    wal: &Arc<Mutex<File>>,
    state: &Arc<RwLock<State>>,
    config: &Config,
    fsyncs: &Arc<AtomicU64>,
    cache: &Option<Arc<BlockCache>>,
    stats: &Arc<Stats>,
) {
    let mut st = state.write().unwrap();
    let mut seqs: Vec<u64> = Vec::with_capacity(group.len());
    let mut outcome: Result<(), FormatError> = Ok(());
    {
        let mut wal = wal.lock().unwrap();
        for (ops, _) in group {
            let seq = st.next_seq;
            st.next_seq += 1;
            seqs.push(seq);
            let frame = match encode_frame(seq, ops) {
                Ok(f) => f,
                Err(e) => {
                    outcome = Err(e);
                    break;
                }
            };
            let appended = wal
                .seek(SeekFrom::End(0))
                .and_then(|_| wal.write_all(&frame));
            if let Err(e) = appended {
                outcome = Err(FormatError::Io(format!("WAL append: {e}")));
                break;
            }
        }
        if outcome.is_ok() {
            if let Err(e) = wal.sync_all() {
                outcome = Err(FormatError::Io(format!("WAL sync: {e}")));
            }
        }
    }
    if outcome.is_ok() {
        fsyncs.fetch_add(1, Ordering::SeqCst);
    }
    crash_park("AIKOQL_V2_GROUP_PARK", &config.dir, "after_fsync");
    if outcome.is_ok() {
        for ((ops, _), seq) in group.iter().zip(&seqs) {
            for op in ops {
                match op {
                    Op::Put(k, v) => st.active.apply(k.clone(), *seq, Some(v.clone())),
                    Op::Delete(k) => st.active.apply(k.clone(), *seq, None),
                    // SE2-M33 — the committer applies object ops with the
                    // same shape as Sync's write().
                    Op::PutObject(rid, k, v) => {
                        st.active
                            .apply_object(k.clone(), *seq, Some(v.clone()), *rid)
                    }
                    Op::DeleteObject(rid, k) => st.active.apply_object(k.clone(), *seq, None, *rid),
                    // SE2-M30 — the same apply as Sync's write(): the ids
                    // were reserved at submit time, so the committer just
                    // merges and pends them (acked == visible).
                    Op::CreateObject {
                        oid,
                        lid,
                        rid,
                        pgen,
                    } => {
                        if let Err(e) = merge_identity(&mut st.identity, *oid, *lid) {
                            outcome = Err(e);
                            break;
                        }
                        st.pending_identity.push(IdentityRecord {
                            oid: *oid,
                            lid: *lid,
                        });
                        if let Err(e) = merge_replica(&mut st.replicas, *lid, LOCAL_NODE_ID, *rid) {
                            outcome = Err(e);
                            break;
                        }
                        st.pending_replicas.push(ReplicaRecord {
                            lid: *lid,
                            node: LOCAL_NODE_ID,
                            rid: *rid,
                        });
                        // SE2-M32 — the placement applies and pends with
                        // the identity (acked == visible, the M6 rule).
                        let placement = Placement::Memtable { generation: *pgen };
                        if let Err(e) = merge_placement(&mut st.placements, *rid, placement) {
                            outcome = Err(e);
                            break;
                        }
                        st.pending_placements.push(PlacementRecord {
                            rid: *rid,
                            placement,
                        });
                    }
                }
            }
        }
        if outcome.is_ok() && st.active.bytes() >= config.memtable_bytes {
            if let Err(e) = Db::flush_locked_impl(config, wal, &mut st, cache, stats) {
                outcome = Err(e);
            }
        }
    }
    crash_park("AIKOQL_V2_GROUP_PARK", &config.dir, "after_apply");
    drop(st);
    for ((_, ack_tx), seq) in group.iter().zip(&seqs) {
        let _ = ack_tx.send(outcome.clone().map(|()| *seq));
    }
    crash_park("AIKOQL_V2_GROUP_PARK", &config.dir, "after_ack");
}

/// SEGMENT-*.seg files the manifest does not reference. Reported and
/// ignored at open — unreferenced data; a future flush may reuse the id,
/// which is safe because nothing references the orphan.
pub fn orphan_segments(dir: &Path, manifest: &Manifest) -> Vec<u64> {
    let mut orphans = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return orphans,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(stem) = name
            .strip_prefix("SEGMENT-")
            .and_then(|s| s.strip_suffix(".seg"))
        else {
            continue;
        };
        let Ok(id) = stem.parse::<u64>() else {
            continue;
        };
        if !manifest.segments.iter().any(|r| r.segment_id == id) {
            orphans.push(id);
        }
    }
    orphans.sort_unstable();
    orphans
}

fn lock_directory(dir: &Path) -> Result<File, FormatError> {
    std::fs::create_dir_all(dir)
        .map_err(|e| FormatError::Io(format!("create {}: {e}", dir.display())))?;
    let path = dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false) // the lock file is just a lock handle — never truncate
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| FormatError::Io(format!("open LOCK {}: {e}", path.display())))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(_) => Err(FormatError::Locked(format!(
            "database directory is held by another process: {}",
            dir.display()
        ))),
    }
}
