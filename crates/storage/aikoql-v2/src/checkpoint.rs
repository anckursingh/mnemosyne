//! SE2-M40 — the directory checkpoint (review P0-1/M5-M7): one coordinated
//! snapshot of the identity + replica + placement directories at one
//! manifest generation, so recovery is
//!
//! ```text
//! newest valid checkpoint ≤ CURRENT
//! + only delta logs published after that checkpoint
//! + the active WAL
//! ```
//!
//! instead of replaying the full metadata history since database creation.
//! The three directories are ONE consistency domain (a flush publishes all
//! three at the same generation), so ONE file with ONE atomic publish
//! carries all of them — per-directory checkpoints would need a joint
//! marker to be atomic (review Challenge B: coordinated wins).
//!
//! On-disk shape (all little-endian; the crate's sha256-8 checksum):
//! `AKCK | format_version u16 | generation u64 |
//!  identity_count u32 | records (ObjectId 16 | LogicalId 8) |
//!  replica_count u32 | records (LogicalId 8 | NodeId 8 | ReplicaId 8) |
//!  placement_count u32 | records (the placement record shape: ReplicaId 8 |
//!  variant u8 | SegmentId 8 | BlockId 4 | entry_offset u32 | generation u64) |
//!  checksum8`
//! Files are named `CHECKPOINT-{gen:06}.log` beside the manifest.
//!
//! The write protocol (review P0-2): publish (atomic write-temp → fsync →
//! rename) → VERIFY (read back + decode — a checkpoint is only trusted to
//! prune history after it proves decodable) → prune delta logs at or below
//! its generation → drop older checkpoints. The checkpoint publishes AFTER
//! CURRENT already names its generation, so it adds NO new crash state:
//! before the publish the full delta history is still there; after it the
//! checkpoint covers ≤ G and the deltas above G replay on top. Pruning a
//! leftover log is idempotent-safe anyway (the merge gates re-apply
//! duplicates as no-ops — the invariants that make crash windows converge).
//!
//! Damage policy: a corrupt checkpoint fails closed ALWAYS (no fallback to
//! the deltas). The verify-at-write step shrinks the window where a
//! checkpoint could be damaged-but-unpruned to milliseconds, and a partial
//! prune makes "fall back to the surviving deltas" silently unsound — the
//! surviving logs may not cover the pruned range. Fail closed preserves the
//! operator's evidence (the v1 closure's Q1 policy, verbatim).

use crate::format::{
    checksum8, crash_park, publish_atomic_staged, Cursor, FormatError, FORMAT_VERSION,
};
use crate::identity::directory::{identity_log_generation, IdentityRecord, ReplicaRecord};
use crate::identity::{NodeId, LOCAL_NODE_ID};
use crate::placement::directory::PlacementRecord;
use crate::placement::{BlockId, Placement, SegmentId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const CHECKPOINT_MAGIC: &[u8; 4] = b"AKCK";
/// oid 16 + lid 8 — the identity log's record shape, verbatim.
const IDENTITY_RECORD_LEN: usize = 24;
/// lid 8 + node 8 + rid 8 — the replica log's record shape.
const REPLICA_RECORD_LEN: usize = 24;
/// rid 8 + variant 1 + segment 8 + block 4 + entry 4 + generation 8.
const PLACEMENT_RECORD_LEN: usize = 33;
const HEADER_LEN: usize = 18; // magic 4 + version 2 + generation 8 + 3×count 4

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryCheckpoint {
    pub format_version: u16,
    pub generation: u64,
    pub identities: Vec<IdentityRecord>,
    pub replicas: Vec<ReplicaRecord>,
    pub placements: Vec<PlacementRecord>,
}

pub fn checkpoint_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("CHECKPOINT-{generation:06}.log"))
}

/// Parse the generation out of a `CHECKPOINT-{gen:06}.log` name.
pub fn checkpoint_generation(name: &str) -> Option<u64> {
    name.strip_prefix("CHECKPOINT-")
        .and_then(|s| s.strip_suffix(".log"))
        .and_then(|g| g.parse::<u64>().ok())
}

impl DirectoryCheckpoint {
    /// The live directories' snapshot at the Db's current generation.
    /// Records are SORTED by key (oid/lid/rid) — HashMap iteration order
    /// is random, and identical workloads must produce byte-identical
    /// checkpoints (the M35 determinism rule).
    pub fn from_state(
        generation: u64,
        identity: &HashMap<crate::identity::ObjectId, crate::identity::LogicalId>,
        replicas: &HashMap<crate::identity::LogicalId, crate::identity::ReplicaId>,
        placements: &HashMap<crate::identity::ReplicaId, Placement>,
    ) -> Self {
        let mut identities: Vec<IdentityRecord> = identity
            .iter()
            .map(|(&oid, &lid)| IdentityRecord { oid, lid })
            .collect();
        identities.sort_by_key(|r| r.oid);
        let mut replicas: Vec<ReplicaRecord> = replicas
            .iter()
            .map(|(&lid, &rid)| ReplicaRecord {
                lid,
                node: LOCAL_NODE_ID,
                rid,
            })
            .collect();
        replicas.sort_by_key(|r| r.lid);
        let mut placements: Vec<PlacementRecord> = placements
            .iter()
            .map(|(&rid, &placement)| PlacementRecord { rid, placement })
            .collect();
        placements.sort_by_key(|r| r.rid);
        DirectoryCheckpoint {
            format_version: FORMAT_VERSION,
            generation,
            identities,
            replicas,
            placements,
        }
    }

    /// Encoded bytes — the exact byte length is the checkpoint's memory
    /// footprint while publishing (Vec of records + this Vec; the maps stay
    /// live). At the 1M-object ceiling: ~81 B × 1M ≈ 81 MB transient.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(
            HEADER_LEN
                + self.identities.len() * IDENTITY_RECORD_LEN
                + self.replicas.len() * REPLICA_RECORD_LEN
                + self.placements.len() * PLACEMENT_RECORD_LEN
                + 8,
        );
        bytes.extend_from_slice(CHECKPOINT_MAGIC);
        bytes.extend_from_slice(&self.format_version.to_le_bytes());
        bytes.extend_from_slice(&self.generation.to_le_bytes());
        bytes.extend_from_slice(&(self.identities.len() as u32).to_le_bytes());
        for r in &self.identities {
            bytes.extend_from_slice(r.oid.as_bytes());
            bytes.extend_from_slice(&r.lid.to_bytes());
        }
        bytes.extend_from_slice(&(self.replicas.len() as u32).to_le_bytes());
        for r in &self.replicas {
            bytes.extend_from_slice(&r.lid.to_bytes());
            bytes.extend_from_slice(&r.node.to_bytes());
            bytes.extend_from_slice(&r.rid.to_bytes());
        }
        bytes.extend_from_slice(&(self.placements.len() as u32).to_le_bytes());
        for r in &self.placements {
            bytes.extend_from_slice(&r.rid.to_bytes());
            match r.placement {
                Placement::Memtable { generation } => {
                    bytes.push(1);
                    bytes.extend_from_slice(&[0u8; 16]); // zeroed placement fields
                    bytes.extend_from_slice(&generation.to_le_bytes());
                }
                Placement::Segment(loc) => {
                    bytes.push(2);
                    bytes.extend_from_slice(&loc.segment_id.to_bytes());
                    bytes.extend_from_slice(&loc.block_id.to_bytes());
                    bytes.extend_from_slice(&loc.entry_offset.to_le_bytes());
                    bytes.extend_from_slice(&loc.generation.to_le_bytes());
                }
                Placement::Retired { generation } => {
                    bytes.push(3);
                    bytes.extend_from_slice(&[0u8; 16]); // zeroed placement fields
                    bytes.extend_from_slice(&generation.to_le_bytes());
                }
            }
        }
        bytes.extend_from_slice(&checksum8(&bytes));
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut cur = Cursor::new(bytes);
        if cur.take(4)? != CHECKPOINT_MAGIC {
            return Err(FormatError::Corrupt("checkpoint bad magic".into()));
        }
        let format_version = cur.u16()?;
        if format_version != FORMAT_VERSION {
            return Err(FormatError::Unsupported(format!(
                "checkpoint format version {format_version} (this build: {FORMAT_VERSION})"
            )));
        }
        let generation = cur.u64()?;
        // Plausibility caps before allocating (the manifest precedent):
        // fixed-width records, so a count that cannot fit is corruption.
        let identity_count = cur.u32()? as usize;
        if identity_count > cur.remaining() / IDENTITY_RECORD_LEN {
            return Err(FormatError::Corrupt(format!(
                "checkpoint identity_count {identity_count} cannot fit in {} remaining bytes",
                cur.remaining()
            )));
        }
        let mut identities = Vec::with_capacity(identity_count);
        for _ in 0..identity_count {
            let oid = crate::identity::ObjectId::from_bytes(
                cur.take(16)?.try_into().expect("16-byte slice"),
            );
            let lid = crate::identity::LogicalId::from_bytes(
                cur.take(8)?.try_into().expect("8-byte slice"),
            );
            identities.push(IdentityRecord { oid, lid });
        }
        let replica_count = cur.u32()? as usize;
        if replica_count > cur.remaining() / REPLICA_RECORD_LEN {
            return Err(FormatError::Corrupt(format!(
                "checkpoint replica_count {replica_count} cannot fit in {} remaining bytes",
                cur.remaining()
            )));
        }
        let mut replicas = Vec::with_capacity(replica_count);
        for _ in 0..replica_count {
            let lid = crate::identity::LogicalId::from_bytes(
                cur.take(8)?.try_into().expect("8-byte slice"),
            );
            let node = NodeId::from_bytes(cur.take(8)?.try_into().expect("8-byte slice"));
            let rid = crate::identity::ReplicaId::from_bytes(
                cur.take(8)?.try_into().expect("8-byte slice"),
            );
            replicas.push(ReplicaRecord { lid, node, rid });
        }
        let placement_count = cur.u32()? as usize;
        if placement_count > cur.remaining() / PLACEMENT_RECORD_LEN {
            return Err(FormatError::Corrupt(format!(
                "checkpoint placement_count {placement_count} cannot fit in {} remaining bytes",
                cur.remaining()
            )));
        }
        let mut placements = Vec::with_capacity(placement_count);
        for _ in 0..placement_count {
            let rid = crate::identity::ReplicaId::from_bytes(
                cur.take(8)?.try_into().expect("8-byte slice"),
            );
            let variant = cur.u8()?;
            let placement = match variant {
                1 => {
                    let _ = cur.take(16)?; // zeroed placement fields
                    Placement::Memtable {
                        generation: cur.u64()?,
                    }
                }
                2 => Placement::Segment(crate::placement::directory::PhysicalLocation {
                    segment_id: SegmentId::from_bytes(
                        cur.take(8)?.try_into().expect("8-byte slice"),
                    ),
                    block_id: BlockId::from_bytes(cur.take(4)?.try_into().expect("4-byte slice")),
                    entry_offset: cur.u32()?,
                    generation: cur.u64()?,
                }),
                3 => {
                    let _ = cur.take(16)?; // zeroed placement fields
                    Placement::Retired {
                        generation: cur.u64()?,
                    }
                }
                other => {
                    return Err(FormatError::Unsupported(format!(
                        "checkpoint placement variant byte {other}"
                    )));
                }
            };
            placements.push(PlacementRecord { rid, placement });
        }
        let stored_ck = cur.take(8)?;
        if !cur.is_empty() {
            return Err(FormatError::Corrupt("checkpoint trailing bytes".into()));
        }
        if checksum8(&bytes[..bytes.len() - 8]) != stored_ck {
            return Err(FormatError::Corrupt("checkpoint checksum mismatch".into()));
        }
        Ok(DirectoryCheckpoint {
            format_version,
            generation,
            identities,
            replicas,
            placements,
        })
    }

    /// Atomic publish. SE2-M40 — staged: the crash-window harness parks
    /// inside the temp's write/fsync (`AIKOQL_V2_PLACE_PARK` naming
    /// `FAIL_AFTER_CHECKPOINT_WRITE` / `_FSYNC`, the M36 plumbing).
    pub fn publish_staged(
        path: &Path,
        checkpoint: &Self,
        stage: Option<&str>,
    ) -> Result<(), FormatError> {
        publish_atomic_staged(path, &checkpoint.encode(), stage)
    }

    /// Read back + decode — the verify-publication step (review P0-2 step 7):
    /// the history is only pruned after the checkpoint PROVES decodable.
    pub fn read(path: &Path) -> Result<Self, FormatError> {
        let bytes = std::fs::read(path)
            .map_err(|e| FormatError::Io(format!("read checkpoint {}: {e}", path.display())))?;
        Self::decode(&bytes)
    }
}

/// The newest valid checkpoint at or below `current_generation` — None when
/// no checkpoint exists (recovery falls back to the full delta history).
/// A file that fails to decode fails closed (see the module doc's damage
/// policy). A file whose internal generation disagrees with its name is a
/// publication anomaly — fail closed, never pick.
pub fn load_newest(
    dir: &Path,
    current_generation: u64,
) -> Result<Option<DirectoryCheckpoint>, FormatError> {
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(dir)
        .map_err(|e| FormatError::Io(format!("read checkpoints in {}: {e}", dir.display())))?
        .flatten()
    {
        let name = entry.file_name();
        let Some(gen) = checkpoint_generation(&name.to_string_lossy()) else {
            continue;
        };
        if gen <= current_generation && best.as_ref().is_none_or(|(g, _)| gen > *g) {
            best = Some((gen, entry.path()));
        }
    }
    let Some((gen, path)) = best else {
        return Ok(None);
    };
    let checkpoint = DirectoryCheckpoint::read(&path)?;
    if checkpoint.generation != gen {
        return Err(FormatError::Corrupt(format!(
            "CHECKPOINT-{gen:06}.log carries generation {}",
            checkpoint.generation
        )));
    }
    Ok(Some(checkpoint))
}

/// Delete every directory delta log at or below `generation` (fully
/// subsumed by the checkpoint now published at that generation) and every
/// OLDER checkpoint. Deletion failures warn — a leftover is harmless (the
/// checkpoint answers first; re-applying old logs is idempotent under the
/// merge gates). Parks after the first deletion for the crash matrix
/// (`AIKOQL_V2_CKP_PARK` = `after_first_prune`). Returns files removed.
pub fn prune_deltas_before(dir: &Path, generation: u64) -> Result<u32, FormatError> {
    let mut deleted: u32 = 0;
    let mut names: Vec<std::ffi::OsString> = Vec::new();
    for entry in std::fs::read_dir(dir)
        .map_err(|e| FormatError::Io(format!("prune directory logs in {}: {e}", dir.display())))?
        .flatten()
    {
        names.push(entry.file_name());
    }
    for name in names {
        let name = name.to_string_lossy();
        let log_gen = identity_log_generation(&name)
            .or_else(|| crate::identity::directory::replica_log_generation(&name))
            .or_else(|| crate::placement::directory::placement_log_generation(&name));
        let remove = match log_gen {
            Some(gen) => gen <= generation,
            None => checkpoint_generation(&name).is_some_and(|gen| gen < generation),
        };
        if !remove {
            continue;
        }
        let path = dir.join(name.as_ref());
        if let Err(e) = std::fs::remove_file(&path) {
            eprintln!(
                "aikoql-v2: obsolete directory file {} not removed: {e}",
                path.display()
            );
            continue;
        }
        deleted += 1;
        if deleted == 1 {
            crash_park("AIKOQL_V2_CKP_PARK", dir, "after_first_prune");
        }
    }
    Ok(deleted)
}

/// Total bytes of directory delta logs published after `after_generation`
/// (every generation, orphan windows included — orphan bytes will be
/// re-published, so they count toward the next checkpoint). The Db seeds
/// its checkpoint budget from this at open and adds each log it publishes.
pub fn directory_log_bytes(dir: &Path, after_generation: u64) -> Result<u64, FormatError> {
    let mut bytes: u64 = 0;
    for entry in std::fs::read_dir(dir)
        .map_err(|e| FormatError::Io(format!("sum directory logs in {}: {e}", dir.display())))?
        .flatten()
    {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let gen = identity_log_generation(&name)
            .or_else(|| crate::identity::directory::replica_log_generation(&name))
            .or_else(|| crate::placement::directory::placement_log_generation(&name));
        let Some(gen) = gen else { continue };
        if gen <= after_generation {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            bytes += meta.len();
        }
    }
    Ok(bytes)
}
