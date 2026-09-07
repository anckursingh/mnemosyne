//! SE2-M29 — ReplicaId (spec §6.3): a local materialized copy of a
//! logical object. For the MVP one logical object has exactly one local
//! replica; future distributed operation gives it replicas A/B/C. CRITICAL
//! TYPE RULE: even when the MVP assigns LogicalId(42) and ReplicaId(42),
//! they MUST remain different Rust types — the compiler prevents
//! substitution (the compile_fail doc-test on the identity module).

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReplicaId(pub u64);

impl ReplicaId {
    /// Little-endian wire form (ID-005 — byte-exact persistence).
    pub fn to_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    pub fn from_bytes(bytes: [u8; 8]) -> Self {
        ReplicaId(u64::from_le_bytes(bytes))
    }
}
