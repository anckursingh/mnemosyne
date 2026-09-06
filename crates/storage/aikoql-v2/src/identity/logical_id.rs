//! SE2-M29 — LogicalId (spec §6.2): the internal logical database
//! identity, stable for the object lifetime. Compact (u64), persistent,
//! internal. CRITICAL RULE: the implementation MUST NOT assume
//! LogicalId == physical identity — logical identity and physical
//! placement remain separate concepts (the placement layer is the only
//! place that knows physical locations).

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LogicalId(pub u64);

impl LogicalId {
    /// Little-endian wire form (ID-005 — byte-exact persistence).
    pub fn to_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    pub fn from_bytes(bytes: [u8; 8]) -> Self {
        LogicalId(u64::from_le_bytes(bytes))
    }
}
