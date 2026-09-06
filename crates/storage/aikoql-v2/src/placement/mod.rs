//! SE2-M29 — placement identifiers (spec §28.1): SegmentId and BlockId are
//! strong newtypes, never bare u64/u32 — physical placement identifiers
//! must not be substitutable for identity types or for each other. The
//! Placement / PhysicalLocation types (spec §7, §34 — the mutable layer
//! below the identity hierarchy) land in SE2-M32.

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
