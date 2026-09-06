//! SE2-M29 — ObjectId (spec §6.1): the canonical object identity — an
//! AIKOQL Knowledge Object's identity independent of storage. It survives
//! compaction, export, import, replication, migration and physical
//! relocation. Representation: 16 raw bytes (never a printable form);
//! ordering is byte-lexicographic (derived Ord — the §6.1 documented
//! requirement); Display is lowercase hex for diagnostics.

use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub [u8; 16]);

impl ObjectId {
    /// The wire width of an ObjectId (§6.1: globally unique, persistent,
    /// serialization-stable).
    pub const LEN: usize = 16;

    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        ObjectId(bytes)
    }

    pub fn to_bytes(self) -> [u8; 16] {
        self.0
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({self})")
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}
