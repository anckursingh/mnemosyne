AIKOQL Storage V2 — Future-Ready Identity & Placement Architecture
TDD Design and Implementation Specification

Status: Proposed
Target: AIKOQL Storage Engine V2
Implementation mode: Strict TDD
Priority: Foundational architecture
Scope: Single-node implementation with future distributed extension points
Out of scope: Replication implementation, consensus, networking, clustering

1. Executive Summary

AIKOQL Storage V2 is being designed as a future-ready storage engine.

The objective of this work is not to implement a distributed database or replication in the current MVP.

The objective is to ensure that the storage engine architecture does not create future constraints that would require:

rewriting storage formats,
breaking SDK APIs,
redesigning object identity,
rewriting compaction,
rebuilding physical storage indexes,
or migrating all existing AIKOQL data

when replication, sharding, or distributed deployment is implemented later.

The central architectural principle is:

Stable logical identity must be separated from mutable physical storage placement.

The proposed identity hierarchy is:

┌───────────────────────────────────────────────┐
│ ObjectId                                      │
│                                               │
│ Global / external object identity             │
│ Stable across export, migration and replicas  │
└───────────────────────┬───────────────────────┘
                        │
                        ▼
┌───────────────────────────────────────────────┐
│ LogicalId                                     │
│                                               │
│ Internal logical database identity            │
│ Stable for the object lifetime                │
└───────────────────────┬───────────────────────┘
                        │
                        ▼
┌───────────────────────────────────────────────┐
│ ReplicaId                                     │
│                                               │
│ Stable local materialization identity         │
│ Different replicas may use different IDs      │
└───────────────────────┬───────────────────────┘
                        │
                        ▼
┌───────────────────────────────────────────────┐
│ PhysicalLocation                              │
│                                               │
│ Mutable physical storage location             │
│ Segment / Block / Entry                       │
└───────────────────────────────────────────────┘

For the MVP:

Single Node
     │
     ▼
One Logical Object
     │
     ▼
One Local Replica
     │
     ▼
One Physical Location

For future distributed operation:

                    Logical Object
                          │
             ┌────────────┼────────────┐
             │            │            │
             ▼            ▼            ▼

          Replica A     Replica B     Replica C

             │            │            │

             ▼            ▼            ▼

          Location      Location      Location
2. Core Architectural Principle
2.1 Separate Identity From Placement

The system must never treat physical location as object identity.

This must be avoided:

Object
   │
   ▼
Segment 42
Block 18
Slot 7

because compaction can change:

Segment 42
Block 18
Slot 7

to:

Segment 99
Block 4
Slot 12

The object must remain the same.

Therefore:

Object Identity
      │
      ▼
Stable IDs
      │
      ▼
Mutable Physical Location

The invariant is:

Identity NEVER changes because of:

✓ flush
✓ compaction
✓ restart
✓ recovery
✓ storage optimization
✓ segment rewrite
✓ cache eviction
✓ future migration

Only:

PhysicalLocation

may change.

3. Current Storage V2 Context

The current AIKOQL Storage V2 architecture includes concepts such as:

Db
 │
 ├── Memtable
 │
 ├── Immutable Memtables
 │
 ├── Segments
 │
 │      ├── Bloom Filter
 │      ├── Block Index
 │      ├── Blocks
 │      └── Restart Points
 │
 ├── WAL
 │
 ├── Manifest
 │
 └── Compaction

Current physical reads conceptually perform:

GET(key)

   │

   ▼

Memtable

   │

   ▼

Segment Range Check

   │

   ▼

Bloom Filter

   │

   ▼

Block Index

   │

   ▼

Block Read

   │

   ▼

Entry Search

The future-ready design must preserve the strengths of the current segment architecture.

The objective is not to replace the segment engine.

The objective is to introduce:

Identity Layer
        +
Placement Layer

above the physical storage layer.

4. Target Architecture
4.1 MVP Architecture
                         AIKOQL API
                              │
                              ▼
                        ObjectId
                              │
                              ▼
                    Identity Directory
                              │
                              ▼
                         LogicalId
                              │
                              ▼
                     Replica Directory
                              │
                              ▼
                         ReplicaId
                              │
                              ▼
                   Placement Resolver
                              │
                              ▼
                     PhysicalLocation
                              │
                              ▼
                       Segment Store
                              │
                              ▼
                   Segment / Block / Entry
4.2 Future Architecture
                           ObjectId
                               │
                               ▼
                        Global Identity
                               │
                               ▼
                           LogicalId
                               │
                               ▼
                        Cluster Topology
                               │
               ┌───────────────┼───────────────┐
               │               │               │
               ▼               ▼               ▼

            Node A          Node B          Node C

               │               │               │

               ▼               ▼               ▼

            ReplicaId       ReplicaId       ReplicaId

               │               │               │

               ▼               ▼               ▼

          Placement       Placement       Placement

               │               │               │

               ▼               ▼               ▼

           Local Store     Local Store     Local Store

No replication functionality is required in this MVP.

Only the boundaries required to support it later must exist.

5. Scope
5.1 In Scope

The following MUST be implemented.

Identity
ObjectId
LogicalId
ReplicaId
NodeId
identity allocation
identity persistence
identity recovery
Placement
PhysicalLocation
local placement directory
location updates
compaction relocation
atomic location publication
Abstractions
identity resolution
replica directory
placement resolution
local topology
Correctness
crash recovery
compaction correctness
stale location prevention
identity stability
restart persistence
5.2 Explicitly Out of Scope

The following MUST NOT be implemented as part of this work.

✗ Raft
✗ Paxos
✗ consensus
✗ leader election
✗ network transport
✗ RPC
✗ replication protocol
✗ quorum reads
✗ quorum writes
✗ distributed transactions
✗ shard balancing
✗ cluster membership
✗ remote reads
✗ remote writes

The architecture may expose extension points for these capabilities.

The implementation must remain single-node.

6. Identity Model
6.1 ObjectId
Purpose

ObjectId is the canonical object identity.

It represents the identity of an AIKOQL Knowledge Object independent of storage.

Example:

ObjectId
   │
   ├── survives compaction
   ├── survives export
   ├── survives import
   ├── survives replication
   ├── survives migration
   └── survives physical relocation
Proposed Type
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ObjectId(pub [u8; 16]);

Alternative representations may be evaluated.

The selected representation MUST satisfy:

✓ globally unique
✓ persistent
✓ serialization stable
✓ ordering requirements documented
✓ future distributed generation supported
6.2 LogicalId
Purpose

LogicalId is the internal logical database identity.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LogicalId(pub u64);

Properties:

✓ compact
✓ stable
✓ persistent
✓ internal
✓ not derived from physical location
Important Rule

The implementation MUST NOT assume:

LogicalId == physical identity

Logical identity and physical placement must remain separate concepts.

6.3 ReplicaId
Purpose

ReplicaId represents a local materialized copy of a logical object.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReplicaId(pub u64);

For MVP:

Logical Object
      │
      ▼
One Local Replica

Future:

Logical Object
      │
      ├── Replica A
      ├── Replica B
      └── Replica C
Critical Type Rule

Even if the MVP implementation initially assigns:

LogicalId(42)
ReplicaId(42)

they MUST remain different Rust types.

The compiler must prevent accidental substitution.

The following must not compile:

fn accepts_replica(_: ReplicaId) {}

let logical = LogicalId(42);

accepts_replica(logical);
6.4 NodeId
Purpose

Represents storage node identity.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub u64);

MVP:

LocalNodeId

Future:

Node A
Node B
Node C

The MVP does not implement cluster membership.

7. PhysicalLocation

Physical location is mutable.

It must never be used as object identity.

Proposed conceptual structure:

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalLocation {
    segment_id: SegmentId,
    block_id: BlockId,
    entry_offset: u32,
    generation: u64,
}

The exact representation may change based on the current Storage V2 segment format.

Important

The fields SHOULD remain private unless a subsystem explicitly requires access.

Only these layers should understand physical placement:

✓ Segment Store
✓ Placement Resolver
✓ Compaction
✓ Recovery

Higher-level code should not depend on:

segment_id
block_id
entry_offset
8. Persistent Directories

The MVP architecture introduces three conceptual directories.

8.1 Identity Directory
ObjectId
    │
    ▼
LogicalId

Conceptual record:

ObjectId → LogicalId

Example:

ObjectId: 01HXYZ...
LogicalId: 42

Requirements:

✓ persistent
✓ crash recoverable
✓ unique ObjectId mapping
✓ LogicalId stability
✓ no duplicate assignment
8.2 Replica Directory
LogicalId
NodeId
    │
    ▼
ReplicaId

Conceptual record:

LogicalId + NodeId → ReplicaId

MVP:

LogicalId + LocalNodeId → ReplicaId

Future:

LogicalId + NodeA → ReplicaId A
LogicalId + NodeB → ReplicaId B
LogicalId + NodeC → ReplicaId C
8.3 Placement Directory
ReplicaId
     │
     ▼
PhysicalLocation

Example:

ReplicaId: 501

        ↓

Segment: 42
Block: 18
Entry: 7
Generation: 99
9. Resolver Abstractions

Abstractions MUST be real and exercised by MVP.

Do not introduce unused speculative traits.

9.1 Identity Resolver
pub trait IdentityResolver: Send + Sync {
    fn resolve(
        &self,
        object_id: ObjectId,
    ) -> Result<Option<LogicalId>, StorageError>;
}

MVP implementation:

LocalIdentityDirectory
9.2 Replica Directory
pub trait ReplicaDirectory: Send + Sync {
    fn resolve_local(
        &self,
        logical_id: LogicalId,
    ) -> Result<Option<ReplicaId>, StorageError>;
}

MVP implementation:

LocalReplicaDirectory
9.3 Placement Resolver
pub trait PlacementResolver: Send + Sync {
    fn resolve(
        &self,
        replica_id: ReplicaId,
    ) -> Result<Option<PhysicalLocation>, StorageError>;
}

MVP implementation:

LocalPlacementResolver
10. Local Topology

A topology abstraction may be added only if it is exercised.

Concept:

pub trait ReplicaTopology: Send + Sync {
    fn replicas_for(
        &self,
        logical_id: LogicalId,
    ) -> Result<Vec<ReplicaDescriptor>, StorageError>;
}

MVP:

LocalTopology

Result:

LogicalId 42

    ↓

[ Local Replica ]

Future:

LogicalId 42

    ↓

[
    Node A,
    Node B,
    Node C
]
11. Critical Design Rule: No Direct Physical Lookup From Logical Layer

This must be prohibited architecturally:

Logical Object
      │
      ▼
SegmentReader::get()

Target:

Logical Object
      │
      ▼
Identity Resolution
      │
      ▼
LogicalId
      │
      ▼
Replica Resolution
      │
      ▼
ReplicaId
      │
      ▼
Placement Resolution
      │
      ▼
PhysicalLocation
      │
      ▼
Physical Reader
12. Performance Architecture

The abstraction must not automatically create:

ObjectId
  │
  ▼
Hash Lookup

LogicalId
  │
  ▼
Hash Lookup

ReplicaId
  │
  ▼
Hash Lookup

PhysicalLocation

on every hot read.

The architecture may maintain a flattened local hot path.

LogicalId
    │
    ▼
Hot Placement Cache
    │
    ▼
PhysicalLocation

Fallback:

LogicalId

   │

   ▼

Replica Directory

   │

   ▼

ReplicaId

   │

   ▼

Placement Directory

   │

   ▼

PhysicalLocation
13. Hot Path Rule

The implementation must measure the cost of indirection.

Acceptance target:

Identity / placement abstraction must not materially regress
existing Storage V2 read gates.

Initial performance gate:

Warm point read regression ≤ 10%
Hot point read regression ≤ 10%

If these gates fail, the coder must provide:

benchmark evidence,
root cause,
proposed optimization,
alternative design.

Do not accept performance regression merely because the architecture is cleaner.

14. Write Path

Target write path:

PUT(Object)

      │

      ▼

Resolve ObjectId

      │

      ├── Existing
      │
      └── New Object

             │

             ▼

       Allocate LogicalId

             │

             ▼

       Allocate ReplicaId

             │

             ▼

             WAL

             │

             ▼

          Memtable

             │

             ▼

       Physical placement
       initially = Memtable
15. Update Path

An update MUST preserve identity.

UPDATE ObjectId

       │

       ▼

Same ObjectId

       │

       ▼

Same LogicalId

       │

       ▼

Same ReplicaId

       │

       ▼

New version / value

       │

       ▼

PhysicalLocation may change

Invariant:

UPDATE must NEVER allocate a new LogicalId.

Invariant:

UPDATE must NEVER allocate a new ReplicaId
for the same local replica.
16. Delete Path

Deletion must not silently destroy identity metadata until lifecycle policy explicitly allows it.

Initial deletion:

ObjectId
LogicalId
ReplicaId

remain historically resolvable depending on tombstone/versioning policy.

For MVP:

Delete behavior MUST be explicitly documented.

The coder must not make identity reuse assumptions.

Recommended invariant:

Deleted IDs must never be reused.
17. Memtable Integration

Current memtable entries must be evaluated and extended.

Target conceptual entry:

pub struct MemEntry {
    pub logical_id: LogicalId,
    pub replica_id: ReplicaId,
    pub value: Option<Vec<u8>>,
}

The exact layout may differ based on existing code.

Requirement

The ID relationship must survive:

Memtable
    │
    ▼
Immutable Memtable
    │
    ▼
Flush
    │
    ▼
Segment

No flush may lose stable identity.

18. Segment Integration

Every persisted entry must retain sufficient stable identity to allow relocation.

The implementation must evaluate whether persisted records require:

LogicalId
ReplicaId

or only:

ReplicaId
Default recommendation

Persist:

ReplicaId

with each physical record.

Reason:

ReplicaId

is the direct stable handle used by:

ReplicaId → PhysicalLocation

The logical relationship remains in metadata.

However, the coder MUST evaluate storage overhead and provide evidence.

19. Segment Format Challenge

Adding an ID to every record increases storage.

The AI coder must evaluate at least:

Option A
Persist ReplicaId in every entry.
Option B
Persist ReplicaId delta encoding.
Option C
Persist IDs in parallel block metadata.
Option D
Derive record identity from stable key + sequence.

The coder must provide a recommendation with:

storage overhead
write overhead
read overhead
compaction complexity
recovery complexity
future replication compatibility

Do not implement blindly.

20. Physical Location Granularity

The exact location representation must be validated.

Candidates:

SegmentId + BlockId + Slot
SegmentId + BlockId + ByteOffset
SegmentId + RecordOffset

The implementation must benchmark the chosen approach.

Recommended initial design:

SegmentId
BlockId
EntryOffset
Generation

But this must be validated against actual segment encoding.

21. Compaction Relocation Protocol

This is the most important new mechanism.

Current conceptual compaction:

Old Segments
      │
      ▼
Merge
      │
      ▼
New Segment
      │
      ▼
Manifest Publish

Future-ready compaction:

Old Segments
      │
      ▼
Merge
      │
      ▼
New Segment
      │
      ▼
Generate Relocation Set

ReplicaId
      │
      ▼
New PhysicalLocation
      │
      ▼
Build New Placement State
      │
      ▼
Durable Publish
      │
      ▼
Retire Old State
22. Relocation Set

During compaction:

ReplicaId 501

Old:

Segment 10
Block 3
Slot 8

New:

Segment 99
Block 17
Slot 2

The relocation output:

ReplicaId 501
    │
    ▼
PhysicalLocation(
    Segment 99,
    Block 17,
    Slot 2
)
23. Atomic Publication Requirement

The following invalid state must never be visible:

New segment visible
BUT
placement directory still points to deleted old segment

Or:

Placement points to new segment
BUT
new segment is not durable

Required ordering:

1. Write new segment files

2. fsync new segment files

3. Build placement updates

4. Persist placement updates

5. fsync placement metadata

6. Build new manifest state

7. fsync manifest

8. Atomically publish CURRENT state

9. Retire old segments later

Exact implementation may differ.

But the crash invariant must hold.

24. Crash Consistency States

The implementation MUST be tested at each boundary.

State A
New segment not durable

Recovery:

Old state remains authoritative.
State B
New segment durable
Placement not durable

Recovery:

Old placement remains authoritative.
New segment may be garbage collected.
State C
New segment durable
New placement durable
Manifest not published

Recovery policy MUST be deterministic.

Recommended:

Old manifest remains authoritative.
New artifacts are garbage collected.
State D
Manifest published
Old segments still exist

Recovery:

New placement authoritative.
Old segments may be cleaned later.
25. Location Generation

Every location update should include a generation/version.

pub struct PhysicalLocation {
    segment_id: SegmentId,
    block_id: BlockId,
    entry_offset: u32,
    generation: u64,
}

Purpose:

✓ stale location detection
✓ debugging
✓ future replication
✓ relocation ordering
✓ crash diagnostics

Invariant:

Newer placement generation must never be replaced
by an older placement generation.
26. Future Distributed Compatibility

No distributed implementation is required.

However, the design must support:

LogicalId
       │
       ├── Node A → ReplicaId A
       │
       ├── Node B → ReplicaId B
       │
       └── Node C → ReplicaId C

The MVP implementation:

LogicalId
       │
       ▼
LocalNodeId
       │
       ▼
ReplicaId

The database core must not assume:

one LogicalId == one PhysicalLocation

This assumption would block replication later.

27. API Boundaries

The MVP should expose internal APIs similar to:

pub trait IdentityResolver {
    fn resolve(
        &self,
        object_id: ObjectId,
    ) -> Result<Option<LogicalId>, StorageError>;
}
pub trait ReplicaDirectory {
    fn resolve_local(
        &self,
        logical_id: LogicalId,
    ) -> Result<Option<ReplicaId>, StorageError>;
}
pub trait PlacementResolver {
    fn resolve(
        &self,
        replica_id: ReplicaId,
    ) -> Result<Option<PhysicalLocation>, StorageError>;
}

These APIs may evolve.

However, the MVP implementation must use them internally.

Unused abstractions are prohibited.

28. Rust Design Requirements
28.1 Strong Newtypes

The implementation MUST use distinct types.

ObjectId
LogicalId
ReplicaId
NodeId
SegmentId
BlockId

Do not use:

u64

for all identifiers.

28.2 Type Safety Test

The following conceptual misuse must be impossible:

fn lookup(_: ReplicaId) {}

let logical = LogicalId(42);

lookup(logical);

This must fail at compile time.

28.3 Ownership

Readers should preferably operate on immutable snapshots.

Avoid:

Global write lock

for every read.

Target:

Immutable state
      │
      ▼
Arc snapshot

Compaction:

Build new state
      │
      ▼
Atomic publication
28.4 Locking

Avoid adding multiple global locks to every read.

The coder must measure:

Mutex contention
RwLock contention
allocation
cache misses

for the new directory layer.

29. Recommended Module Structure
crates/storage/aikoql-v2/src/

identity/
├── mod.rs
├── object_id.rs
├── logical_id.rs
├── replica_id.rs
└── node_id.rs

placement/
├── mod.rs
├── location.rs
├── directory.rs
├── resolver.rs
└── generation.rs

topology/
├── mod.rs
├── local.rs
└── descriptor.rs

db.rs
memtable.rs
segment.rs
compaction.rs
manifest.rs
wal.rs

The actual module structure may be simplified.

Avoid creating excessive files merely to match this diagram.

30. TDD Implementation Rules

The AI coder MUST follow this order:

RED
 ↓
Write failing test

GREEN
 ↓
Minimal implementation

REFACTOR
 ↓
Improve implementation

BENCHMARK
 ↓
Verify performance

COMMIT
 ↓
One logical milestone

Do not implement multiple milestones before tests.

31. TDD Milestone 1 — Strong Identity Types
Tests
Test ID-001
ObjectId equality works.
Test ID-002
LogicalId equality works.
Test ID-003
ReplicaId equality works.
Test ID-004
Different ID types cannot be substituted.
Test ID-005
IDs can be persisted and recovered byte-exactly.
Acceptance Gate
All identity tests PASS.
No existing storage tests regress.
32. TDD Milestone 2 — Identity Directory
Test ID-010
New ObjectId receives a LogicalId.
Test ID-011
Resolving the same ObjectId returns the same LogicalId.
Test ID-012
Restart preserves ObjectId → LogicalId mapping.
Test ID-013
Two ObjectIds never receive the same LogicalId.
Test ID-014
LogicalIds are never reused after deletion.
Test ID-015
Crash during identity persistence does not create
duplicate or ambiguous mappings.
33. TDD Milestone 3 — Local Replica Directory
Test RP-001
LogicalId resolves to exactly one local ReplicaId.
Test RP-002
Same LogicalId always resolves to same ReplicaId.
Test RP-003
Restart preserves LogicalId → ReplicaId.
Test RP-004
ReplicaId is never reused.
Test RP-005
LogicalId and ReplicaId remain distinct types.
34. TDD Milestone 4 — Placement Directory
Test PL-001
ReplicaId resolves to a PhysicalLocation.
Test PL-002
Unknown ReplicaId returns None.
Test PL-003
Placement survives restart.
Test PL-004
Placement update replaces location atomically.
Test PL-005
Older generation cannot overwrite newer generation.
35. TDD Milestone 5 — Memtable Integration
Test MT-001
New object write allocates stable identity.
Test MT-002
Update preserves ObjectId.
Test MT-003
Update preserves LogicalId.
Test MT-004
Update preserves ReplicaId.
Test MT-005
Memtable read returns correct value through identity path.
36. TDD Milestone 6 — Flush
Test FL-001
Memtable → Segment preserves ReplicaId.
Test FL-002
PhysicalLocation becomes resolvable after flush.
Test FL-003
Old Memtable location does not remain authoritative
after successful flush publication.
Test FL-004
Crash before flush publication preserves old state.
37. TDD Milestone 7 — Compaction Relocation
Test CP-001
Compaction preserves ObjectId.
Test CP-002
Compaction preserves LogicalId.
Test CP-003
Compaction preserves ReplicaId.
Test CP-004
Compaction may change PhysicalLocation.
Test CP-005
After compaction:

ReplicaId → new PhysicalLocation
Test CP-006
Old PhysicalLocation is never returned
after successful publication.
38. TDD Milestone 8 — Crash Injection

The coder MUST introduce deterministic crash/failure injection points.

At minimum:

FAIL_AFTER_SEGMENT_WRITE
FAIL_AFTER_SEGMENT_FSYNC
FAIL_AFTER_LOCATION_WRITE
FAIL_AFTER_LOCATION_FSYNC
FAIL_AFTER_MANIFEST_WRITE
FAIL_AFTER_MANIFEST_FSYNC
FAIL_AFTER_PUBLISH

Each failure must have:

reopen database
        │
        ▼
validate consistency
        │
        ▼
verify all objects
        │
        ▼
verify no invalid locations
39. TDD Milestone 9 — Recovery
Test RC-001
Database restart restores identity mappings.
Test RC-002
Database restart restores replica mappings.
Test RC-003
Database restart restores placement mappings.
Test RC-004
All committed objects remain readable.
Test RC-005
No uncommitted relocation becomes visible.
40. TDD Milestone 10 — Full Relocation Stress

Test:

Create 100,000 objects.

Record:

ObjectId
LogicalId
ReplicaId
PhysicalLocation
Value

Perform:

✓ updates
✓ flushes
✓ compactions
✓ restarts

Then verify:

ObjectId unchanged
LogicalId unchanged
ReplicaId unchanged

Value correct

PhysicalLocation may change.
41. TDD Milestone 11 — Randomized Testing

Generate random operations:

PUT
UPDATE
DELETE
FLUSH
COMPACT
RESTART
CRASH
RECOVER

Maintain independent oracle:

BTreeMap<ObjectId, ExpectedState>

Validate:

value
identity
version
existence
42. Required Oracle

The test oracle MUST not reuse storage engine logic.

Recommended:

std::collections::BTreeMap

Do not validate AIKOQL using another AIKOQL internal abstraction.

43. Performance Certification

Existing Storage V2 performance certification must be rerun.

At minimum:

W1 KO get
W2 head get
W3 version get
W4 relation traversal
W5 scan
W6 ingestion
W7 context
W8 mixed workload

New metrics:

identity lookup latency
replica lookup latency
placement lookup latency
total read latency
directory memory usage
directory persistence size
44. Performance Gates

Initial gates:

Metric	Gate
Hot read regression	≤ 10%
Warm read regression	≤ 10%
Cold read regression	≤ 15%
Identity resolution P50	Report
Placement resolution P50	Report
Directory memory growth	Report
Correctness	100%

If performance exceeds the regression gates:

DO NOT ACCEPT THE IMPLEMENTATION.

The coder must provide:

1. Baseline result
2. New result
3. Root cause
4. Proposed optimization
5. Alternative architecture
45. Memory Gate

The current Storage V2 has already observed significant RSS sensitivity.

Therefore identity and placement directories must have explicit memory tests.

Test:

1M objects

Measure:

Identity Directory RSS
Replica Directory RSS
Placement Directory RSS
Total Storage RSS
Bytes per object

Report:

bytes/object

This is mandatory.

46. Critical Design Challenge for the AI Coder

The AI coder MUST answer the following before finalizing implementation.

Question 1

Should ReplicaId be persisted in every physical record?

Evaluate:

YES
NO

Provide evidence.

Question 2

What is the exact bytes-per-object cost?

Report:

ObjectId
LogicalId
ReplicaId
PhysicalLocation
Directory overhead
Hash table overhead
Allocator overhead
Question 3

Can the placement directory remain memory resident?

Evaluate:

100K
1M
10M
100M

objects.

Question 4

What happens if placement metadata becomes larger than RAM?

Provide a future-compatible strategy.

Candidates:

paged directory
mmap directory
two-level directory
B-tree
LSM metadata
hash index

No implementation is required unless needed for MVP.

But the architecture must not block it.

Question 5

What is the compaction amplification?

Measure:

records relocated
location entries updated
bytes written
metadata write amplification
47. Recommended Future Direction — Paged Placement Directory

The implementation SHOULD evaluate a paged design.

Concept:

ReplicaId

    │

    ├── PageId
    │
    └── SlotId
Placement Directory

Page 0
Page 1
Page 2
Page 3

Each page:

Slot 0 → PhysicalLocation
Slot 1 → PhysicalLocation
Slot 2 → PhysicalLocation

Compaction:

ReplicaIds updated
       │
       ▼
Group by Page
       │
       ▼
Rewrite affected pages
       │
       ▼
Publish new pages

This avoids rewriting an entire directory.

The coder should evaluate this against a simple map implementation.

48. MVP Simplification Allowed

The MVP implementation MAY begin with:

Simple persistent directories

if the following are preserved:

✓ stable APIs
✓ stable ID semantics
✓ placement abstraction
✓ generation semantics
✓ crash correctness

The implementation must not hardwire:

HashMap forever

into public storage semantics.

49. Forbidden Shortcuts

The following shortcuts are prohibited.

Forbidden
LogicalId = SegmentId + Offset

Reason:

Physical location changes.

Forbidden
ReplicaId = PhysicalLocation

Reason:

Compaction invalidates it.

Forbidden
Delete then reuse IDs

Reason:

Future replication and references become unsafe.

Forbidden
Update allocates new LogicalId

Reason:

Logical identity breaks.

Forbidden
Compaction allocates new ReplicaId

Reason:

Stable local identity breaks.

Forbidden
Placement update without crash recovery tests

Reason:

Can produce dangling physical pointers.

50. Required Code Review Questions

Before merging each milestone, the coder must answer:

Correctness
Can an object become unreachable?
Stability
Can compaction change identity?
Recovery
Can crash recovery point to a deleted segment?
Memory
What is metadata bytes/object?
Performance
How much latency does identity resolution add?
Future
Would this API prevent multiple replicas later?
51. Definition of Done

This work is complete only when:

Correctness
✓ all identity tests pass
✓ all relocation tests pass
✓ all crash injection tests pass
✓ randomized tests pass
✓ recovery tests pass
✓ existing storage tests pass
Stability
✓ ObjectId stable
✓ LogicalId stable
✓ ReplicaId stable
✓ PhysicalLocation mutable
Compaction
✓ relocation works
✓ new location resolves correctly
✓ old location is retired safely
✓ crash cannot expose invalid location
Architecture
✓ LocalTopology implemented
✓ PlacementResolver implemented
✓ ReplicaDirectory implemented
✓ abstractions exercised by production code
✓ no replication implementation required
Performance
✓ workload benchmarks completed
✓ read regression gates evaluated
✓ memory usage reported
✓ directory overhead reported
52. Final Acceptance Gate

The feature is ACCEPTED only when the following are true:

┌────────────────────────────────────────────────┐
│ AIKOQL Storage V2                              │
│                                                │
│ Identity stable                     PASS        │
│ Physical relocation                 PASS        │
│ Flush correctness                   PASS        │
│ Compaction correctness              PASS        │
│ Crash recovery                      PASS        │
│ Restart recovery                    PASS        │
│ Existing storage certification      PASS        │
│ Performance gates                   PASS        │
│ Memory measurement                  REPORTED    │
│ Future replica extension point      PASS        │
│ Replication implementation          NOT NEEDED  │
└────────────────────────────────────────────────┘
53. Final Engineering Principle

The goal of this work is:

NOT

"Build a distributed database now."

The goal is:

"Build a storage engine whose identity and placement
model will not need to be destroyed when distributed
capabilities are introduced."

The MVP must remain:

Single Node
Single Replica
Local Storage

But internally it must already understand the distinction between:

WHAT an object is

and:

WHERE a particular materialized copy is stored.

The final invariant is:

Object Identity
       │
       │ NEVER changes because of storage operations
       │
       ▼
Logical Identity
       │
       │ Stable database identity
       │
       ▼
Replica Identity
       │
       │ Stable local materialization identity
       │
       ▼
Physical Location
       │
       │ May change at any time
       │
       ▼
Physical Storage
54. AI Coder Required Response

Before implementing each major milestone, the AI coder must provide:

1. Current code path affected

2. Proposed change

3. Why the change is required

4. Test to be written first

5. Expected RED state

6. Minimal GREEN implementation

7. Refactor plan

8. Performance impact

9. Memory impact

10. Risks or alternative designs

The coder is explicitly encouraged to challenge this specification where evidence from the existing AIKOQL Storage V2 implementation shows that a proposed abstraction would:

✓ increase latency materially
✓ increase memory materially
✓ duplicate existing functionality
✓ weaken crash safety
✓ complicate compaction unnecessarily

Any challenge must include:

code evidence
test evidence
benchmark evidence
alternative design

The implementation must remain evidence-driven and TDD-first.

END OF SPECIFICATION