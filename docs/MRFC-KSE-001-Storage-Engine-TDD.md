# AIKOQL Knowledge Storage Engine — TDD Implementation Specification

**Document ID:** MRFC-KSE-001  
**Status:** TDD implementation specification  
**Target branch:** `main`  
**Audience:** AIKOQL Rust implementation agent / senior storage engineer  
**Objective:** Prototype and validate an AIKOQL-native storage engine without destabilizing the existing MVP.

---

# 1. Executive Decision

AIKOQL is **not** required to replace redb/RocksDB immediately.

The current `main` branch already has a clean storage boundary:

```text
Knowledge Kernel
      |
      v
StorageEngine trait
      |
  +---+--------+
  |            |
redb        RocksDB
```

The workspace explicitly includes both the kernel and `crates/storage/rocksdb`. The kernel declares that it depends only on the `StorageEngine` trait, while durable backends implement the same contract. The current abstraction provides `get`, prefix `scan`, atomic `write_batch`, constraint capability reporting, snapshot and restore operations.

The repository layer owns the persistent key schema and keeps key-layout details out of the transaction orchestrator. The current logical keyspace contains KO versions, heads, knowledge events, tombstones, idempotency records, subscriptions, bidirectional relationship indexes, type indexes and metadata.

The kernel commit pipeline currently serializes validation → OCC → HLC assignment → atomic write batch → acknowledgement through a single-writer path, while versions are stored using `(koid, commit_ts)` MVCC semantics.

Therefore:

> **Build and benchmark an AIKOQL-native storage engine behind the existing storage contract. Do not replace the current backend until measured evidence justifies replacement.**

---

# 2. Current Storage Baseline

## 2.1 Existing implementations

```text
StorageEngine
  |
  +-- MemoryEngine
  +-- RedbEngine
  +-- RocksDbEngine
```

### Memory

Reference implementation for conformance and deterministic tests.

### redb

Current pure-Rust embedded ACID backend using an ordered B-tree.

### RocksDB

Current LSM backend using atomic `WriteBatch`, synchronous writes and concurrent readers/writers.

The existing RocksDB implementation uses performance defaults including a 64 MB write buffer, four background jobs and Snappy compression.

---

# 3. Why Build a Custom Engine?

Only if it solves an AIKOQL-specific problem.

The custom engine should optimize for:

```text
KO point lookup
current-head lookup
version/history lookup
entity neighborhood
relationship traversal
type scan
fact/provenance lookup
evidence lookup
temporal access
incremental ingestion
context compilation
snapshot/restore
encrypted storage
```

The key architectural hypothesis is:

> **Knowledge locality can become a first-class storage optimization.**

A logical KO often requires:

```text
identity
+
current state
+
facts
+
relations
+
provenance
+
temporal metadata
+
evidence references
```

A future AIKOQL engine may colocate these efficiently instead of repeatedly traversing multiple independent KV/index structures.

---

# 4. Non-Goals

Do NOT implement in this wave:

- consensus
- sharding
- distributed transactions
- cross-node replication
- SQL
- vector indexing
- graph query planning
- custom filesystem
- custom allocator
- cloud KMS
- custom compression algorithms

Those belong to later milestones.

---

# 5. TDD Contract

Every feature MUST follow:

```text
RED
  ↓
write failing test
  ↓
implement minimum correct behavior
  ↓
GREEN
  ↓
regression
  ↓
property/stress test
  ↓
benchmark
```

The coding agent MUST NOT weaken assertions or modify expected results simply to make tests pass.

---

# 6. New Crate

Create an experimental crate:

```text
crates/storage/aikoql/
```

Recommended initial name:

```text
aikoql-storage
```

Recommended engine type:

```rust
AikoqlStorageEngine
```

Do not expose it as the production default until the adoption gate passes.

---

# 7. Phase KSE-1 — Storage Contract Conformance

These tests must pass for the custom engine exactly as they pass for existing backends.

## KSE-001 — Get

Insert:

```text
k1 -> v1
```

Expected:

```text
get(k1) == v1
```

## KSE-002 — Missing Key

Expected:

```text
get(missing) == None
```

## KSE-003 — Prefix Scan

Insert:

```text
a/1
a/2
a/3
b/1
```

`scan("a/")` must return:

```text
a/1
a/2
a/3
```

sorted ascending.

## KSE-004 — Atomic Batch

One batch containing puts and deletes must become visible atomically.

## KSE-005 — Empty Batch

Empty batch produces no state change.

## KSE-006 — Conflicting Put/Delete

Define and test deterministic semantics for the same key appearing in puts and deletes.

All backends must agree.

---

# 8. Phase KSE-2 — Preserve Existing AIKOQL Key Semantics

The current repository defines logical prefixes such as:

```text
ko/
head/
ke/
tomb/
idem/
sub/
relo/
reli/
type/
meta/
```

The custom engine must preserve their semantic ordering and lookup behavior.

## KSE-010 — Version Ordering

For:

```text
KO-1:
v1@t1
v2@t2
v3@t3
```

ordered version access must be deterministic.

## KSE-011 — Current Head

`head/KO-1` resolves to the latest valid version.

## KSE-012 — Historical Read

Given a timestamp, the correct historical version is returned.

## KSE-013 — Tombstone

Deleted objects cannot appear as current active objects.

## KSE-014 — Idempotency

Same idempotency key commits one logical operation only.

## KSE-015 — Outbound Relationship Index

```text
A -> R -> B
```

must be discoverable from A.

## KSE-016 — Inbound Relationship Index

The same relationship must be discoverable from B inbound.

## KSE-017 — Type Index

Type scans return the correct live candidate objects.

---

# 9. Phase KSE-3 — Record Envelope

Define a versioned physical record envelope.

Recommended conceptual layout:

```text
RecordEnvelope
├── magic
├── format_version
├── namespace
├── flags
├── key_length
├── value_length
├── sequence/version
├── checksum
├── payload
└── optional encryption metadata
```

## KSE-020 — Encode/Decode

```text
decode(encode(record)) == record
```

for every supported record type.

## KSE-021 — Corrupt Payload

Flip one payload bit.

Expected:

```text
deterministic corruption error
```

No corrupted data may be returned as valid.

## KSE-022 — Truncated Record

Expected safe error; no uncaught panic.

## KSE-023 — Unsupported Format Version

Expected explicit incompatibility error.

---

# 10. Phase KSE-4 — Block Abstraction

Start simple.

Recommended block:

```text
Block
├── header
├── sorted key directory
├── value area
└── checksum
```

## KSE-030 — Block Round Trip

Write/read all records successfully.

## KSE-031 — Sorted Keys

All keys are strictly ordered.

## KSE-032 — Point Lookup

Use binary search or equivalent indexed lookup.

## KSE-033 — Prefix Range

Prefix query never returns out-of-range keys.

---

# 11. Phase KSE-5 — Knowledge Locality

This is the first custom-engine-specific optimization.

Investigate physical locality for:

```text
KO
 ├── head/current state
 ├── facts
 ├── relationship adjacency
 ├── provenance references
 └── temporal/version metadata
```

Do not duplicate full payloads unnecessarily.

## KSE-040 — KO Read Amplification

Retrieve:

```text
KO
+
facts
+
relationships
+
provenance
```

Measure:

```text
logical requests
physical records
physical blocks
bytes read
P50/P95/P99
```

Compare:

```text
redb
RocksDB
AikoqlStorageEngine
```

---

# 12. Phase KSE-6 — Relationship Locality

The current repository uses outbound and inbound relationship indexes.

Prototype a knowledge-aware adjacency structure.

## KSE-050 — Neighbor Lookup

Benchmark objects with:

```text
1 neighbor
10 neighbors
100 neighbors
1,000 neighbors
10,000 neighbors
```

Measure:

```text
latency
records read
bytes read
allocations
```

## KSE-051 — Typed Neighbor Lookup

Retrieve only one relationship type.

## KSE-052 — Bidirectional Traversal

Verify outbound and inbound consistency.

---

# 13. Phase KSE-7 — Temporal Locality

The current kernel uses MVCC and HLC-based commit timestamps.

## KSE-060 — Current Version

Current state should be accessible without scanning all history.

## KSE-061 — Historical Version

Retrieve a specific historical version.

## KSE-062 — Full History

Return all versions in timestamp order.

## KSE-063 — Temporal Range

Return only:

```text
t1 <= commit_ts < t2
```

---

# 14. Phase KSE-8 — Transaction Compatibility

The custom engine MUST remain compatible with the existing kernel semantics.

## KSE-070 — Atomic Multi-KO Commit

Create:

```text
Customer
Account
Relationship
```

in one logical transaction.

Expected: all-or-nothing.

## KSE-071 — Rollback

Inject failure.

Expected: no partial logical state.

## KSE-072 — OCC Conflict

Two updates target the same KO.

Expected: documented OCC winner/conflict behavior.

## KSE-073 — Independent Transactions

Updates to unrelated KOs should not unnecessarily conflict at the storage level.

## KSE-074 — Snapshot Read

A reader with a pinned snapshot observes a stable view according to the kernel contract.

---

# 15. Phase KSE-9 — Crash Consistency

Add storage-level fault injection.

Inject failures:

```text
before append
after append
before flush
after flush
before commit marker
after commit marker
before index publication
after index publication
```

## KSE-080 — Crash Before Commit

Expected old state or valid rollback state.

## KSE-081 — Crash After Commit

Expected committed state survives restart.

## KSE-082 — Crash During Index Update

Canonical knowledge remains valid.

Index can be rebuilt.

## KSE-083 — Recovery

After crash:

```text
open
recover
validate
query
```

must succeed without invalid logical state.

---

# 16. Phase KSE-10 — Derived Index Rebuild

Canonical state must remain authoritative.

## KSE-090 — Full Index Rebuild

Rebuild all derived indexes.

Expected equivalent logical query results.

## KSE-091 — Partial Index Loss

Delete ~10% of derived entries.

Rebuild.

Expected complete correctness restored.

## KSE-092 — Corrupt Index

Expected detection and rebuild/invalidation, not incorrect knowledge.

---

# 17. Phase KSE-11 — Encryption Boundary

Reuse the existing AIKOQL encryption architecture.

Do not create a second incompatible encryption model.

## KSE-100 — Encrypted Write/Read

Round trip succeeds.

## KSE-101 — Wrong Key

Expected fail closed.

## KSE-102 — Corrupt Ciphertext

Expected deterministic error.

## KSE-103 — Key Rotation

New writes use new key version.

Old data remains readable according to policy.

## KSE-104 — Crash During Rotation

Expected recoverable state and no plaintext fallback.

---

# 18. Phase KSE-12 — Property-Based Testing

Generate random operation sequences:

```text
create
update
delete
restore
supersede
relate
unrelate
snapshot
restore
```

After every sequence assert:

```text
no orphan relationships
no duplicate logical IDs
provenance completeness
temporal consistency
transactional atomicity
index consistency
```

Run at least:

```text
10,000 generated sequences
```

in nightly CI.

Increase only after stability is demonstrated.

---

# 19. Phase KSE-13 — Concurrency

The current kernel has a single-writer semantic pipeline, so distinguish:

```text
kernel transaction serialization
```

from:

```text
storage concurrent access
```

Do not claim that RocksDB concurrency automatically changes AIKOQL transaction semantics.

## KSE-120 — Mixed Read/Write Stress

Run:

```text
32–256 readers
4–32 writers
```

Workloads:

```text
KO lookup
relationship traversal
history
type scan
ingestion
update
delete
context compilation
```

Expected:

- no deadlocks
- no corruption
- no invalid logical reads
- no duplicate commits
- no authorization bypass

Collect:

```text
P50
P95
P99
throughput
CPU
RSS
IO
contention
```

---

# 20. Phase KSE-14 — Snapshot and Restore

The storage contract already supports snapshot/restore semantics.

## KSE-130 — Snapshot Equivalence

Snapshot source database and restore to a clean database.

Expected equivalent:

```text
KOs
facts
relations
provenance
temporal state
constraints
```

## KSE-131 — Snapshot With Active Readers

Expected internally consistent point-in-time snapshot.

## KSE-132 — Snapshot With Active Writers

Expected a documented point-in-time consistency guarantee.

Recommended requirement:

```text
snapshot represents one valid database state
```

not a mixed state.

---

# 21. Phase KSE-15 — Startup and Recovery

## KSE-140 — Cold Startup

Measure:

```text
open
metadata initialization
index initialization
ready
```

## KSE-141 — Crash Recovery

Measure:

```text
crash
restart
recovery
first successful query
```

Record recovery time.

---

# 22. Phase KSE-16 — Storage Amplification

For representative datasets record:

```text
logical bytes
physical bytes
```

Calculate:

```text
space amplification = physical / logical
```

Break down:

```text
KO payload
versions
relationships
provenance
evidence
indexes
encryption
```

---

# 23. Phase KSE-17 — Write Amplification

Measure physical writes caused by:

```text
one KO create
one KO update
one relationship update
one temporal version
one provenance update
one evidence update
```

Compare all backends.

The custom engine must demonstrate whether knowledge-aware physical layout actually reduces write amplification.

---

# 24. Phase KSE-18 — Read Amplification

For each logical operation measure:

```text
logical objects requested
physical records touched
blocks touched
bytes read
```

Required workloads:

```text
get KO
get KO + facts
get KO + neighbors
get history
compile context
```

This metric is more important than raw generic KV ops/sec.

---

# 25. Phase KSE-19 — Resource Usage

Measure at:

```text
100K KOs
1M KOs
10M KOs
```

Record:

```text
RSS
heap
cache memory
index memory
peak allocation
disk footprint
CPU
```

The custom engine must not require the entire knowledge graph in memory.

---

# 26. Phase KSE-20 — Backend Conformance

The custom engine MUST pass the same conformance suite as:

```text
MemoryEngine
RedbEngine
RocksDbEngine
AikoqlStorageEngine
```

Any difference must be explained by an explicit, documented capability rather than an accidental semantic divergence.

---

# 27. AIKOQL-Specific Performance Workloads

Do not use generic:

```text
SET/GET benchmark
```

as the main justification.

Use:

### W1 — KO Point Lookup

```text
1M KOs
random reads
```

### W2 — Current Head Lookup

```text
1M KOs
random current-state reads
```

### W3 — Version Lookup

```text
1M KOs
10+ versions each
```

### W4 — Relationship Traversal

```text
10 / 100 / 1000 neighbors
```

### W5 — Type Scan

```text
1M KOs
100 types
```

### W6 — Knowledge Ingestion

```text
100K
1M
10M logical operations
```

### W7 — Context Compilation

```text
entity
+
facts
+
relations
+
provenance
+
temporal state
```

### W8 — Mixed workload

```text
70% reads
20% relationship access
10% writes
```

---

# 28. Comparison Matrix

The benchmark harness MUST generate:

| Workload | Memory | redb | RocksDB | AIKOQL Storage |
|---|---:|---:|---:|---:|
| KO get | | | | |
| head get | | | | |
| version lookup | | | | |
| history | | | | |
| relationship lookup | | | | |
| type scan | | | | |
| ingestion | | | | |
| context compilation | | | | |
| concurrent mixed load | | | | |
| snapshot | | | | |
| recovery | | | | |

For each workload record:

```text
throughput
P50
P95
P99
bytes read
bytes written
CPU
RSS
disk size
```

---

# 29. Adoption Gate

AIKOQL must NOT replace redb/RocksDB merely because the custom engine wins a microbenchmark.

Adopt it only if all are true:

## Correctness

```text
P0 = 100%
P1 >= 98%
```

## Reliability

```text
0 unrecoverable crash cases
0 unexplained corruption
```

## Performance

At least one important AIKOQL-specific workload demonstrates approximately:

```text
>= 2x improvement
```

against the relevant current backend.

Examples:

```text
KO retrieval
relationship traversal
temporal access
context compilation
ingestion
```

with no unacceptable regressions in other core workloads.

## Resource efficiency

No unacceptable regression in:

```text
RAM
CPU
disk
write amplification
read amplification
```

## Maintainability

The custom engine must not create an unjustified operational/maintenance burden.

If these gates do not pass:

> **KEEP THE CURRENT BACKEND.**

That is a valid and successful outcome.

---

# 30. TDD Deliverables

The coding agent must produce:

```text
1. AikoqlStorageEngine prototype
2. Unit tests
3. Conformance tests
4. Property tests
5. Fault-injection tests
6. Concurrency tests
7. Recovery tests
8. Encryption tests
9. AIKOQL-specific benchmarks
10. redb comparison
11. RocksDB comparison
12. storage-amplification results
13. read/write amplification results
14. resource-usage results
15. final adoption recommendation
```

---

# 31. Required Reports

Generate:

```text
artifacts/storage-engine/
├── tdd-report.md
├── conformance.md
├── crash-recovery.md
├── concurrency.md
├── corruption.md
├── encryption.md
├── benchmark.md
├── amplification.md
├── resource-usage.md
└── adoption-decision.md
```

`adoption-decision.md` MUST end with exactly one:

```text
KEEP REDB
KEEP ROCKSDB
USE HYBRID
ADOPT AIKOQL STORAGE ENGINE
```

with evidence supporting the decision.

---

# 32. Important Architectural Rule

Do not allow the custom storage engine to leak backend-specific types above the storage boundary.

Forbidden:

```rust
fn foo(db: &rocksdb::DB) ...
```

inside kernel/domain code.

Required:

```rust
fn foo(store: &dyn StorageEngine) ...
```

The kernel must remain backend-independent.

---

# 33. Storage Engine Success Criteria

The project should consider the custom storage engine successful even if the final decision is:

```text
KEEP ROCKSDB
```

provided the experiment proves:

- AIKOQL has a stable storage abstraction.
- Its semantic model is not coupled to a vendor.
- AIKOQL-specific workload characteristics are understood.
- A future native engine has a measurable design target.

The goal is not to own storage for its own sake.

The goal is to own storage **only where ownership creates a material advantage for Knowledge Objects and agent/enterprise workloads.**
