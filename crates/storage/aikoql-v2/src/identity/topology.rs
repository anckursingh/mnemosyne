//! SE2-M31 — the local replica directory and topology (spec §9.2/§10/§27):
//! resolution views over the Db's replica state, as the spec's trait
//! shapes. `ReplicaDirectory` resolves a LogicalId to its one local
//! ReplicaId; `ReplicaTopology` answers which replicas exist for a
//! LogicalId (MVP: exactly one, on the local node — §26 forbids assuming
//! one-LogicalId == one-PhysicalLocation beyond this).
//!
//! The MVP views borrow the live Db, so resolution always sees the current
//! state (the create path reserves lid → rid 1:1 under the state lock).
//! Errors use the crate's established `FormatError` (the spec's
//! `StorageError` name is its own to evolve — §27): resolution is an
//! in-memory map read here, so the MVP bodies cannot fail; the `Result`
//! shape is the trait contract a distributed implementation needs.

use crate::db::Db;
use crate::format::FormatError;
use crate::identity::{LogicalId, NodeId, ReplicaId, LOCAL_NODE_ID};

/// One replica of a logical object, on one node (§10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicaDescriptor {
    pub node: NodeId,
    pub replica: ReplicaId,
}

/// §9.2 — resolves a LogicalId to its local ReplicaId.
pub trait ReplicaDirectory: Send + Sync {
    fn resolve_local(&self, logical_id: LogicalId) -> Result<Option<ReplicaId>, FormatError>;
}

/// The MVP implementation: a live view over one Db's replica state.
pub struct LocalReplicaDirectory<'a> {
    db: &'a Db,
}

impl<'a> LocalReplicaDirectory<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }
}

impl ReplicaDirectory for LocalReplicaDirectory<'_> {
    fn resolve_local(&self, logical_id: LogicalId) -> Result<Option<ReplicaId>, FormatError> {
        Ok(self.db.resolve_local(logical_id))
    }
}

/// §10 — which replicas serve a LogicalId. The MVP answers the one local
/// replica; the future shape is the node list.
pub trait ReplicaTopology: Send + Sync {
    fn replicas_for(&self, logical_id: LogicalId) -> Result<Vec<ReplicaDescriptor>, FormatError>;
}

/// The MVP implementation: the local node holds every replica.
pub struct LocalTopology<'a> {
    db: &'a Db,
}

impl<'a> LocalTopology<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }
}

impl ReplicaTopology for LocalTopology<'_> {
    fn replicas_for(&self, logical_id: LogicalId) -> Result<Vec<ReplicaDescriptor>, FormatError> {
        Ok(match self.db.resolve_local(logical_id) {
            Some(replica) => vec![ReplicaDescriptor {
                node: LOCAL_NODE_ID,
                replica,
            }],
            None => Vec::new(),
        })
    }
}
