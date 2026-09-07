//! SE2-M29 — NodeId (spec §6.4): storage node identity. The MVP is
//! single-node: every replica directory entry keys on LOCAL_NODE_ID.
//! Future distributed operation introduces Node A/B/C — cluster
//! membership is NOT implemented here, only the type boundary that keeps
//! node identity from being conflated with object or replica identity.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u64);

/// The MVP's one storage node (§6.4 — `LocalNodeId`).
pub const LOCAL_NODE_ID: NodeId = NodeId(1);

impl NodeId {
    /// Little-endian wire form (ID-005 — byte-exact persistence).
    pub fn to_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    pub fn from_bytes(bytes: [u8; 8]) -> Self {
        NodeId(u64::from_le_bytes(bytes))
    }
}
