//! SE2-M32 — the placement directory (spec §34 milestone 4, §14/§16/§25):
//! ReplicaId → Placement, where Placement is the deviation-1 variant type —
//! `Memtable { generation }` (§14: physical placement initially = Memtable),
//! `Segment(PhysicalLocation)` (the §25 struct, verbatim), or
//! `Retired { generation }` (§16: deletion stays historically resolvable).
//! Every variant carries a generation so PL-005's rule is one uniform
//! check: a NEWER generation is never replaced by an OLDER one — older
//! records are stale and ignored (§25's stale-location detection), an
//! identical repeat (crash-window double-apply) is a no-op, and an
//! equal-generation different record is a protocol violation that fails
//! closed (every update allocates a fresh generation).
//!
//! Persistence mirrors the identity/replica families (SE2-M30):
//! per-generation fixed-record delta logs, all little-endian:
//! `AKPL | format_version u16 | generation u64 | record_count u32 |
//!  records (ReplicaId 8 | variant u8 | SegmentId 8 | BlockId 4 |
//!  entry_offset u32 | generation u64) | checksum8`
//! — one 33-byte shape for all three variants (Memtable/Retired zero the
//! placement fields); atomic publish; decode order magic → version →
//! structure → checksum. Recovery applies logs ≤ CURRENT's generation
//! through the caller's validation closure (the Db passes manifest +
//! reader bounds), so a Segment placement referencing a segment the
//! manifest does not name, or a block/entry outside the reader's ranges,
//! fails closed.

use crate::db::Db;
use crate::format::{checksum8, publish_atomic_staged, Cursor, FormatError, FORMAT_VERSION};
use crate::identity::ReplicaId;
use crate::placement::{BlockId, SegmentId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const PLACEMENT_MAGIC: &[u8; 4] = b"AKPL";
/// rid 8 + variant 1 + segment 8 + block 4 + entry 4 + generation 8 —
/// fixed width for all variants (non-Segment records zero the placement
/// fields), so plausibility caps and decode stay uniform.
const PLACEMENT_RECORD_LEN: usize = 33;
const HEADER_LEN: usize = 18; // magic 4 + version 2 + generation 8 + count 4

const VARIANT_MEMTABLE: u8 = 1;
const VARIANT_SEGMENT: u8 = 2;
const VARIANT_RETIRED: u8 = 3;

/// §25 — the physical location of one replica, the spec's struct verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalLocation {
    pub segment_id: SegmentId,
    pub block_id: BlockId,
    pub entry_offset: u32,
    pub generation: u64,
}

/// Deviation 1 — the placement is a variant: §14's initial Memtable state
/// and §16's Retired state are placements a flat PhysicalLocation cannot
/// express. Generation lives in every variant (one uniform PL-005 check).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Memtable { generation: u64 },
    Segment(PhysicalLocation),
    Retired { generation: u64 },
}

impl Placement {
    /// The generation every variant carries — the PL-005 gate key.
    pub fn generation(self) -> u64 {
        match self {
            Placement::Memtable { generation } | Placement::Retired { generation } => generation,
            Placement::Segment(loc) => loc.generation,
        }
    }
}

/// One delta record: a replica's placement at the moment it was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementRecord {
    pub rid: ReplicaId,
    pub placement: Placement,
}

/// How one apply resolved under the generation gate (PL-005).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// Inserted or replaced a strictly older generation.
    Applied,
    /// Identical repeat — a crash-window double-apply.
    Duplicate,
    /// Older than the existing placement — stale, ignored (§25).
    Stale,
}

/// The one gate implementation — every apply path (recovery, live write,
/// WAL replay) routes through here so the rule cannot drift.
pub fn merge_placement(
    placements: &mut HashMap<ReplicaId, Placement>,
    rid: ReplicaId,
    placement: Placement,
) -> Result<ApplyOutcome, FormatError> {
    let incoming = placement.generation();
    match placements.get(&rid) {
        None => {
            placements.insert(rid, placement);
            Ok(ApplyOutcome::Applied)
        }
        Some(existing) if existing.generation() > incoming => Ok(ApplyOutcome::Stale),
        Some(existing) if *existing == placement => Ok(ApplyOutcome::Duplicate),
        Some(existing) if existing.generation() < incoming => {
            placements.insert(rid, placement);
            Ok(ApplyOutcome::Applied)
        }
        Some(_) => Err(FormatError::Corrupt(format!(
            "placement for replica {rid:?} changed at the same generation {}",
            incoming
        ))),
    }
}

/// Validate one Segment placement against the recovery-time authorities:
/// the manifest's segment set and the segment reader's per-block entry
/// count (`None` = block id past the segment's block count). The Db's
/// recovery closure calls this per Segment record; unit tests call it
/// directly.
pub fn validate_segment_location(
    loc: &PhysicalLocation,
    manifest_segment_ids: &std::collections::HashSet<u64>,
    block_entries: Option<u32>,
) -> Result<(), FormatError> {
    if !manifest_segment_ids.contains(&loc.segment_id.0) {
        return Err(FormatError::Corrupt(format!(
            "placement references segment {} absent from the manifest",
            loc.segment_id.0
        )));
    }
    let Some(entries) = block_entries else {
        return Err(FormatError::Corrupt(format!(
            "placement block {} past segment {}'s blocks",
            loc.block_id.0, loc.segment_id.0
        )));
    };
    if loc.entry_offset >= entries {
        return Err(FormatError::Corrupt(format!(
            "placement entry {} past block {}'s {entries} entries",
            loc.entry_offset, loc.block_id.0
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementLog {
    pub format_version: u16,
    pub generation: u64,
    pub records: Vec<PlacementRecord>,
}

pub fn placement_log_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("PLACEMENT-{generation:06}.log"))
}

impl PlacementLog {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes =
            Vec::with_capacity(HEADER_LEN + self.records.len() * PLACEMENT_RECORD_LEN + 8);
        bytes.extend_from_slice(PLACEMENT_MAGIC);
        bytes.extend_from_slice(&self.format_version.to_le_bytes());
        bytes.extend_from_slice(&self.generation.to_le_bytes());
        bytes.extend_from_slice(&(self.records.len() as u32).to_le_bytes());
        for r in &self.records {
            bytes.extend_from_slice(&r.rid.to_bytes());
            match r.placement {
                Placement::Memtable { generation } => {
                    bytes.push(VARIANT_MEMTABLE);
                    bytes.extend_from_slice(&[0u8; 16]); // zeroed placement fields
                    bytes.extend_from_slice(&generation.to_le_bytes());
                }
                Placement::Segment(loc) => {
                    bytes.push(VARIANT_SEGMENT);
                    bytes.extend_from_slice(&loc.segment_id.to_bytes());
                    bytes.extend_from_slice(&loc.block_id.to_bytes());
                    bytes.extend_from_slice(&loc.entry_offset.to_le_bytes());
                    bytes.extend_from_slice(&loc.generation.to_le_bytes());
                }
                Placement::Retired { generation } => {
                    bytes.push(VARIANT_RETIRED);
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
        if cur.take(4)? != PLACEMENT_MAGIC {
            return Err(FormatError::Corrupt("placement log bad magic".into()));
        }
        let format_version = cur.u16()?;
        if format_version != FORMAT_VERSION {
            return Err(FormatError::Unsupported(format!(
                "placement log format version {format_version} (this build: {FORMAT_VERSION})"
            )));
        }
        let generation = cur.u64()?;
        let count = cur.u32()? as usize;
        if count > cur.remaining() / PLACEMENT_RECORD_LEN {
            return Err(FormatError::Corrupt(format!(
                "placement log record_count {count} cannot fit in {} remaining bytes",
                cur.remaining()
            )));
        }
        let mut records = Vec::with_capacity(count);
        for _ in 0..count {
            let rid = ReplicaId::from_bytes(cur.take(8)?.try_into().expect("8-byte slice"));
            let variant = cur.u8()?;
            let placement = match variant {
                VARIANT_MEMTABLE => {
                    let _ = cur.take(16)?; // zeroed placement fields
                    Placement::Memtable {
                        generation: cur.u64()?,
                    }
                }
                VARIANT_SEGMENT => Placement::Segment(PhysicalLocation {
                    segment_id: SegmentId::from_bytes(
                        cur.take(8)?.try_into().expect("8-byte slice"),
                    ),
                    block_id: BlockId::from_bytes(cur.take(4)?.try_into().expect("4-byte slice")),
                    entry_offset: cur.u32()?,
                    generation: cur.u64()?,
                }),
                VARIANT_RETIRED => {
                    let _ = cur.take(16)?; // zeroed placement fields
                    Placement::Retired {
                        generation: cur.u64()?,
                    }
                }
                other => {
                    return Err(FormatError::Unsupported(format!(
                        "placement variant byte {other}"
                    )));
                }
            };
            records.push(PlacementRecord { rid, placement });
        }
        let stored_ck = cur.take(8)?;
        if !cur.is_empty() {
            return Err(FormatError::Corrupt("placement log trailing bytes".into()));
        }
        if checksum8(&bytes[..bytes.len() - 8]) != stored_ck {
            return Err(FormatError::Corrupt(
                "placement log checksum mismatch".into(),
            ));
        }
        Ok(PlacementLog {
            format_version,
            generation,
            records,
        })
    }

    pub fn publish(path: &Path, log: &Self) -> Result<(), FormatError> {
        Self::publish_staged(path, log, None)
    }

    /// SE2-M36 — the §38 crash windows on the compaction path.
    pub fn publish_staged(path: &Path, log: &Self, stage: Option<&str>) -> Result<(), FormatError> {
        publish_atomic_staged(path, &log.encode(), stage)
    }

    /// SE2-M40 — encoded byte length without encoding (the checkpoint
    /// trigger's budget).
    pub fn encoded_len(&self) -> usize {
        HEADER_LEN + self.records.len() * PLACEMENT_RECORD_LEN + 8
    }
}

/// Parse the generation out of a `PLACEMENT-{gen:06}.log` name.
/// SE2-M40 — pub(crate), the checkpoint pruner shares it.
pub(crate) fn placement_log_generation(name: &str) -> Option<u64> {
    name.strip_prefix("PLACEMENT-")
        .and_then(|s| s.strip_suffix(".log"))
        .and_then(|g| g.parse::<u64>().ok())
}

/// Delta logs at or below CURRENT's generation, oldest first; a damaged
/// authoritative log fails closed. SE2-M35 — structural validation is the
/// caller's, on the MERGED map: records superseded by a relocation may
/// legitimately name segments the relocation retired, so a per-log
/// validator would reject history (the loader no longer validates).
pub fn load_placement_logs(
    dir: &Path,
    current_generation: u64,
) -> Result<Vec<PlacementLog>, FormatError> {
    let mut gens = Vec::new();
    for entry in std::fs::read_dir(dir)
        .map_err(|e| FormatError::Io(format!("read placement logs in {}: {e}", dir.display())))?
        .flatten()
    {
        let name = entry.file_name();
        let Some(gen) = placement_log_generation(&name.to_string_lossy()) else {
            continue;
        };
        if gen <= current_generation {
            gens.push(gen);
        }
    }
    gens.sort_unstable();
    let mut logs = Vec::new();
    for gen in gens {
        let bytes = std::fs::read(placement_log_path(dir, gen))
            .map_err(|e| FormatError::Io(format!("read PLACEMENT-{gen:06}.log: {e}")))?;
        let log = PlacementLog::decode(&bytes)?;
        if log.generation != gen {
            return Err(FormatError::Corrupt(format!(
                "PLACEMENT-{gen:06}.log carries generation {}",
                log.generation
            )));
        }
        logs.push(log);
    }
    Ok(logs)
}

/// Logs past CURRENT's generation (a crash between placement-log publish
/// and CURRENT — the §24 state-C window): reported and ignored; the WAL
/// still holds the ops, so replay rebuilds the same records.
pub fn orphan_placement_logs(dir: &Path, current_generation: u64) -> Vec<u64> {
    let mut orphans = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return orphans;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(gen) = placement_log_generation(&name.to_string_lossy()) else {
            continue;
        };
        if gen > current_generation {
            orphans.push(gen);
        }
    }
    orphans.sort_unstable();
    orphans
}

/// SE2-M40 — the highest placement generation inside the ORPHAN placement
/// logs (gens > CURRENT, the §24 state-C window of a compaction). The
/// relocation records do NOT ride the WAL, so the recovered map never sees
/// them — but the numbers were durably published and must never be handed
/// out again (review INV-05). The allocator seeds past this maximum.
/// Best-effort: orphans are non-authoritative (recovery must not fail on
/// them — cp009), so an undecodable orphan warns and contributes nothing.
pub fn orphan_placement_max_generation(dir: &Path, current_generation: u64) -> u64 {
    let mut max = 0u64;
    for gen in orphan_placement_logs(dir, current_generation) {
        let bytes = match std::fs::read(placement_log_path(dir, gen)) {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "aikoql-v2: orphan PLACEMENT-{gen:06}.log unreadable ({e}); \
                     allocator cannot see its generations"
                );
                continue;
            }
        };
        match PlacementLog::decode(&bytes) {
            Ok(log) => {
                for rec in &log.records {
                    max = max.max(rec.placement.generation());
                }
            }
            Err(e) => eprintln!(
                "aikoql-v2: orphan PLACEMENT-{gen:06}.log not decodable ({e}); \
                 allocator cannot see its generations"
            ),
        }
    }
    max
}

/// The in-memory placement directory: the gate-protected map plus the
/// recovery path. The Db keeps the map itself (State) and routes every
/// apply through `merge_placement`; this struct is the unit-testable
/// surface for the same rules.
#[derive(Debug, Default)]
pub struct PlacementDirectory {
    placements: HashMap<ReplicaId, Placement>,
}

impl PlacementDirectory {
    pub fn apply(&mut self, rec: PlacementRecord) -> Result<ApplyOutcome, FormatError> {
        merge_placement(&mut self.placements, rec.rid, rec.placement)
    }

    pub fn resolve(&self, rid: ReplicaId) -> Option<&Placement> {
        self.placements.get(&rid)
    }

    pub fn len(&self) -> usize {
        self.placements.len()
    }

    pub fn is_empty(&self) -> bool {
        self.placements.is_empty()
    }

    /// Rebuild from the delta logs ≤ CURRENT's generation, validating
    /// Segment placements through the caller's closure (the M0 pattern —
    /// format contracts standalone before Db wiring). SE2-M35 — validation
    /// runs on the SURVIVING map after the merge gate: superseded records
    /// may name retired segments; history is not corruption.
    pub fn recover(
        dir: &Path,
        current_generation: u64,
        validate: &mut dyn FnMut(&PhysicalLocation) -> Result<(), FormatError>,
    ) -> Result<Self, FormatError> {
        let mut placements = HashMap::new();
        for log in load_placement_logs(dir, current_generation)? {
            for rec in &log.records {
                merge_placement(&mut placements, rec.rid, rec.placement)?;
            }
        }
        for p in placements.values() {
            if let Placement::Segment(loc) = p {
                validate(loc)?;
            }
        }
        Ok(PlacementDirectory { placements })
    }
}

/// §9.3 — resolves a ReplicaId to its Placement (deviation 1: the variant
/// type, not a bare PhysicalLocation — §14/§16 need the other variants).
pub trait PlacementResolver: Send + Sync {
    fn resolve(&self, replica_id: ReplicaId) -> Result<Option<Placement>, FormatError>;
}

/// The MVP implementation: a live view over one Db's placement state.
pub struct LocalPlacementResolver<'a> {
    db: &'a Db,
}

impl<'a> LocalPlacementResolver<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }
}

impl PlacementResolver for LocalPlacementResolver<'_> {
    fn resolve(&self, replica_id: ReplicaId) -> Result<Option<Placement>, FormatError> {
        Ok(self.db.resolve_placement(replica_id))
    }
}
