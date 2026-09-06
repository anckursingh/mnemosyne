//! SE2-M29/M32 — placement identifiers (spec §28.1): SegmentId and BlockId
//! are strong newtypes, never bare u64/u32 — physical placement identifiers
//! must not be substitutable for identity types or for each other. The
//! placement directory (the mutable layer below the identity hierarchy,
//! spec §34) lives in [`directory`].

pub mod directory;

pub use directory::{
    merge_placement, orphan_placement_logs, placement_log_path, validate_segment_location,
    ApplyOutcome, LocalPlacementResolver, PhysicalLocation, Placement, PlacementDirectory,
    PlacementLog, PlacementRecord, PlacementResolver,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SegmentId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

impl SegmentId {
    /// Little-endian wire form (ID-005 — byte-exact persistence).
    pub fn to_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    pub fn from_bytes(bytes: [u8; 8]) -> Self {
        SegmentId(u64::from_le_bytes(bytes))
    }
}

impl BlockId {
    /// Little-endian wire form (ID-005 — byte-exact persistence).
    pub fn to_bytes(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }

    pub fn from_bytes(bytes: [u8; 4]) -> Self {
        BlockId(u32::from_le_bytes(bytes))
    }
}
