//! SE2-M29 — the identity layer's strong types (artifacts/storage-engine-v2/
//! logical-id-physical-id.md §6, §28). The identity hierarchy is
//! ObjectId → LogicalId → ReplicaId → (placement) PhysicalLocation: the
//! external object identity, the internal logical database identity, and
//! the local materialization identity. Each is a DISTINCT Rust newtype —
//! the compiler prevents accidental substitution (§6.3, §28.2). Identity
//! NEVER changes because of flush/compaction/restart/recovery/migration;
//! only the placement layer below it may move.
//!
//! ```compile_fail
//! // ID-004 — the §6.3 rule, verbatim: a LogicalId must never be accepted
//! // where a ReplicaId is required, even when both wrap the value 42.
//! use aikoql_storage_v2::identity::{LogicalId, ReplicaId};
//!
//! fn accepts_replica(_: ReplicaId) {}
//!
//! let logical = LogicalId(42);
//! accepts_replica(logical);
//! ```

pub mod logical_id;
pub mod node_id;
pub mod object_id;
pub mod replica_id;

pub use logical_id::LogicalId;
pub use node_id::{NodeId, LOCAL_NODE_ID};
pub use object_id::ObjectId;
pub use replica_id::ReplicaId;
