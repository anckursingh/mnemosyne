# AIKOQL Storage Engine v2 — Production-Grade Architecture Proposal

**Status:** Proposed  
**Perspective:** Senior database storage-engine architect  
**Baseline:** AIKOQL PR #2 and MVP certification closure

## 1. Executive Decision

Do not keep extending the current **single append-only WAL → full replay → RAM mirror** architecture indefinitely.

The MVP engine is a strong foundation. Production scale should preserve its proven strengths:

- checksummed WAL records,
- fail-closed corruption behavior,
- torn-tail recovery,
- ordered commit semantics,
- concurrent readers,
- `StorageEngine` abstraction,
- AIKOQL kernel semantics.

Replace its structural limits:

```text
Unbounded WAL
+
Full replay at open
+
Whole active dataset reconstructed into RAM
```

### Target architecture

```text
Kernel
  │
  ▼
StorageEngine
  │
  ▼
Commit Coordinator
  │
  ├── WAL ───────────────┐
  │                      │
  ▼                      │
Memtable                 │
  │ flush                │ recovery
  ▼                      │
Immutable Segments ◄─────┘
  │
  ▼
Manifest / CURRENT
  │
  ▼
Compaction
```

**Design choice:** build a segmented, LSM-inspired engine, but not a generic RocksDB clone.

AIKOQL should become a **Knowledge Object-aware, version-aware, graph-index-aware storage engine**.

---

# 2. Evidence Driving v2

The MVP evidence establishes:

- `StorageEngine` contract conformance.
- Checksummed WAL.
- Fail-closed corruption handling.
- Torn-tail recovery.
- Ordered single-writer commit path.
- Concurrent reads.
- Full semantic recovery tests.
- Recovery scaling proportional to WAL size.
- No compaction.
- Full historical replay at open.
- Memory-first query path.

Therefore the v2 seam is:

```text
MVP
WAL → replay everything → BTreeMap

V2
Active WAL → Memtable → Immutable Segment → Manifest
```

The WAL becomes a **recovery log**, not permanent database storage.

---

# 3. Production Goals

## Required

- Multi-GB datasets.
- Bounded recovery.
- Larger-than-RAM operation.
- Crash-safe checkpoints.
- Immutable durable segments.
- Online compaction.
- Snapshot-consistent reads.
- Concurrent reads.
- Configurable durability.
- Strong corruption detection.
- Format versioning.

## Explicitly not in v2 core

- Distributed consensus.
- Multi-node replication.
- Generic SQL transaction manager.
- Generic OLTP competition.
- Replacing AIKOQL kernel semantics.

---

# 4. Architecture Principles

## P1 — Physical storage generic, policy KO-aware

Bytes remain opaque at the storage boundary, but layout and compaction may understand stable AIKOQL key families:

```text
HEAD
KO
VERSION
REL_OUT
REL_IN
TYPE
META
PROVENANCE
```

## P2 — WAL is bounded

```text
WAL → recover recent writes
Segment → durable historical state
Manifest → authoritative topology
```

## P3 — Immutable files simplify correctness

Published segments are never modified.

## P4 — Manifest publication defines visibility

```text
segment durable
→ validate
→ fsync
→ publish manifest atomically
→ visible
```

## P5 — Compaction must preserve AIKOQL semantics

Compaction may rewrite bytes but must not alter:

- visible heads,
- version history,
- temporal answers,
- relationships,
- KO identity,
- provenance.

---

# 5. On-Disk Layout

```text
database/
├── CURRENT
├── MANIFEST-000001
├── wal/
│   ├── WAL-000001.log
│   └── WAL-000002.log
├── segments/
│   ├── SEG-000001.aq
│   └── SEG-000002.aq
├── tmp/
└── snapshots/
```

---

# 6. CURRENT and Manifest

## CURRENT

Stores:

```text
format version
manifest generation
checksum
```

Publication:

```text
write temp
→ fsync
→ atomic rename
→ fsync directory where supported
```

## Manifest

Tracks:

```text
segment ID
level
key range
sequence range
record count
file size
checksum
generation
```

The manifest is the authoritative topology.

---

# 7. WAL v2

Retain the proven checksummed envelope but add explicit generations and sequence numbers.

```text
MAGIC
FORMAT_VERSION
FRAME_TYPE
SEQUENCE
PAYLOAD_LENGTH
PAYLOAD
CRC
```

A WAL commit is acknowledged only after its configured durability boundary.

Durability modes:

```text
Sync
GroupCommit
Async
```

No mode may silently downgrade durability.

---

# 8. Commit Pipeline

```text
Client
  ↓
Commit Coordinator
  ↓
Assign sequence
  ↓
Append WAL
  ↓
Durability boundary
  ↓
Apply Memtable
  ↓
Acknowledge
```

Ordering authority remains explicit.

---

# 9. Group Commit

Production path:

```text
Writers
  ↓
Commit Queue
  ↓
Append WAL frames
  ↓
One fsync
  ↓
Apply / acknowledge group
```

Configuration:

```text
max_batch_ops
max_batch_bytes
max_wait_duration
```

Sync mode remains the correctness baseline.

---

# 10. Memtable

```text
Active Memtable
      ↓ threshold
Immutable Memtable
      ↓
Background Flush
      ↓
Immutable Segment
```

The initial implementation should prioritize deterministic correctness over lock-free sophistication.

---

# 11. Segment Format

```text
Segment
├── Header
├── Data Blocks
├── Index Block
├── Bloom Filter
├── Metadata
└── Footer
```

Initial default block target:

```text
64 KiB, configurable
```

Each block:

```text
magic
format version
block type
entry count
compressed size
uncompressed size
checksum
```

Entries use:

```text
prefix-compressed key
value length
value
sequence
flags
```

Flags:

```text
PUT
DELETE
VERSION
TOMBSTONE
```

---

# 12. Read Path

```text
Read
 ↓
Memtable
 ↓ miss
Block Cache
 ↓ miss
Bloom Filter
 ↓ possible
Segment Index
 ↓
Data Block
```

Optimize AIKOQL-native workloads:

```text
head lookup
version lookup
history
relationship scan
type scan
```

---

# 13. Internal Key Design

Use stable key families and sequence ordering.

Conceptually:

```text
InternalKey =
KeyFamily
+ LogicalKey
+ DescendingSequence
```

This allows version-aware lookups without full history scans.

---

# 14. Temporal / Version Retention

This is a critical AIKOQL differentiator.

Compaction must not assume:

```text
old version = garbage
```

Retention decisions must respect:

```text
head
temporal query requirements
version lookup
supersede lineage
audit/provenance
legal retention
configured policy
```

Compaction receives:

```text
KEEP
DROP
ARCHIVE
```

from a semantic retention policy.

---

# 15. Compaction

Start simple:

```text
L0 immutable flush segments
        ↓
background merge
        ↓
L1 sorted segments
```

Do not implement deep multi-level compaction until measurements justify it.

Compaction pipeline:

```text
select inputs
↓
write tmp output
↓
validate
↓
fsync
↓
publish manifest
↓
release obsolete files after readers drain
```

Use `Arc<Segment>` handles initially for safe reader lifetime.

---

# 16. Recovery

Target:

```text
Open
↓
Read CURRENT
↓
Load manifest
↓
Validate referenced segments
↓
Open active WAL
↓
Replay only active WAL
↓
Ready
```

The key property:

> Recovery cost must be bounded by active WAL, not historical database size.

---

# 17. Buffer / Cache

Do not begin with a complex generic buffer pool.

Start with:

```text
Segment metadata cache
+
bounded block cache
```

Required metrics:

```text
cache bytes
hit rate
miss rate
evictions
```

Cache must never affect correctness.

---

# 18. Snapshot

Use manifest-based snapshots.

```text
Capture manifest generation
↓
Pin referenced segments
↓
Capture WAL boundary
↓
Write snapshot manifest
```

Immutable segments allow efficient snapshots without global write locks.

---

# 19. Multi-Process Policy

Phase 1:

```text
One process owns one database directory.
```

Use an OS-level lock.

Do not accidentally permit multiple independent writers.

---

# 20. Corruption Model

Every durable object must be independently validated.

### WAL

```text
frame checksum
length
sequence
type
```

### Segment

```text
header checksum
block checksum
footer checksum
```

### Manifest

```text
generation
checksum
atomic publication
```

Explicitly classify:

```text
torn tail
complete corruption
missing segment
manifest corruption
orphan segment
partial compaction output
```

Every class must have a tested recovery policy.

---

# 21. Observability

Expose:

```text
wal_bytes
active_memtable_bytes
immutable_memtable_count
segment_count
segment_bytes
compaction_pending_bytes
last_compaction_time
recovery_time
wal_replay_bytes
cache_hit_rate
cache_bytes
write_queue_depth
group_commit_size
fsync_latency
```

These should be consumable by:

```text
Studio
CLI
metrics endpoint
logs
```

---

# 22. Storage API Evolution

Do not break `StorageEngine` unnecessarily.

Use optional capabilities:

```rust
trait StorageStats {
    fn stats(&self) -> StorageMetrics;
}

trait StorageAdmin {
    fn checkpoint(&self) -> Result<Checkpoint>;
    fn compact(&self) -> Result<CompactionResult>;
}
```

Keep CRUD semantics separate from administration.

---

# 23. MVP WAL Migration

Migration:

```text
Open legacy WAL
↓
Validate
↓
Build v2 segments
↓
Create manifest
↓
Atomically publish CURRENT
↓
Reopen and verify
↓
Only then retain/remove legacy according to policy
```

Never delete the source WAL before semantic verification.

---

# 24. Implementation Roadmap

## SE2-M0 — Format Contracts

Deliver:

```text
CURRENT
Manifest
segment IDs
format versions
checksums
```

Acceptance:

- [ ] Golden byte-format tests.
- [ ] Unknown version fails closed.
- [ ] Manifest corruption detected.
- [ ] Atomic CURRENT publication tested.
- [ ] Existing StorageEngine semantics unchanged.

## SE2-M1 — Immutable Segments

Deliver:

```text
segment writer
segment reader
blocks
index
checksums
```

Acceptance:

- [ ] Segment round-trip.
- [ ] Random lookup.
- [ ] Prefix scan.
- [ ] Corrupted block fails closed.
- [ ] Published segment is immutable.

## SE2-M2 — Memtable and Flush

Acceptance:

- [ ] Writes remain visible during flush.
- [ ] Crash before publication recovers from WAL.
- [ ] Crash after publication does not duplicate state.
- [ ] Memory threshold bounds active memtable.

## SE2-M3 — Bounded Recovery

Acceptance:

- [ ] Historical WAL is not replayed after flush.
- [ ] Only active WAL replays.
- [ ] Recovery is bounded by active WAL.
- [ ] Existing corruption tests remain green.

## SE2-M4 — Compaction

Acceptance:

- [ ] Logical state before == after compaction.
- [ ] Crash injection covers every publication stage.
- [ ] Readers continue during compaction.
- [ ] Obsolete segments survive while referenced.

## SE2-M5 — Version-Aware Compaction

Acceptance:

- [ ] Head preserved.
- [ ] Temporal queries preserved.
- [ ] Required versions preserved.
- [ ] Tombstones retained until safe.
- [ ] Supersede lineage preserved.

## SE2-M6 — Group Commit

Acceptance:

- [ ] No acknowledged commit lost.
- [ ] Commit order deterministic.
- [ ] Sync semantics unchanged.
- [ ] Throughput improves under multi-writer load.
- [ ] Crash injection covers group boundaries.

## SE2-M7 — Cache / Bloom Filters

Acceptance:

- [ ] Bounded cache.
- [ ] Cache cannot change correctness.
- [ ] Bloom false negatives forbidden.
- [ ] Measured random-read improvement.

---

# 25. Critical TDD Tests

## Recovery Independence Test

```text
10 GB historical segments
+
100 MB active WAL
```

Expected:

```text
Open cost depends primarily on active WAL,
not total historical database size.
```

## Crash Matrix

Inject failure:

```text
before WAL append
after WAL append
before fsync
after fsync
before memtable apply
during flush
before segment fsync
after segment fsync
before manifest publication
after manifest publication
during compaction
before old-file deletion
```

Expected:

```text
No acknowledged write lost.
No phantom commit.
No duplicate logical state.
```

## Compaction Semantic Equivalence

```text
Create KO
Update ×100
Supersede
Add relationships
Remove relationships
Query temporal states
Compact repeatedly
```

Gate:

```text
All logical answers before compaction
==
All logical answers after compaction
```

This is mandatory.

---

# 26. Performance Acceptance

These are architecture gates, not current claims.

- [ ] Recovery bounded by active WAL.
- [ ] Dataset larger than RAM remains queryable.
- [ ] Memory limits configurable.
- [ ] Group commit improves concurrent write throughput without weakening Sync durability.
- [ ] KO lookup remains competitive with MVP baseline.

---

# 27. Definition of Done

## Correctness

- [ ] Existing MVP semantic tests pass.
- [ ] WAL corruption tests pass.
- [ ] Segment corruption tests pass.
- [ ] Crash matrix passes.
- [ ] Compaction semantic equivalence passes.

## Durability

- [ ] Acknowledged commits survive crashes.
- [ ] Published segments survive reopen.
- [ ] Manifest publication is atomic.

## Scalability

- [ ] Historical state does not require full replay.
- [ ] WAL lifecycle is bounded by flush/checkpoint.
- [ ] Dataset may exceed RAM.
- [ ] Memory is bounded/configurable.

## AIKOQL semantics

- [ ] KO head semantics preserved.
- [ ] Version history preserved.
- [ ] Temporal queries preserved.
- [ ] Relationship indexes preserved.
- [ ] Provenance/evidence semantics preserved.

## Operations

- [ ] Storage statistics exposed.
- [ ] WAL growth observable.
- [ ] Compaction backlog observable.
- [ ] Recovery time measurable.
- [ ] On-disk format versioned.

---

# 28. Final Recommendation

Do not build:

```text
"Our own RocksDB"
```

Build:

```text
AIKOQL Native Knowledge Storage Engine
```

The strategic differentiators are:

```text
Knowledge Object-aware storage
+
version-aware persistence
+
temporal correctness
+
graph-friendly key layout
+
agent-memory workload optimization
+
bounded recovery
```

The evolution path is:

```text
MVP WAL
↓
Segmented WAL
↓
Memtable
↓
Immutable blocks
↓
Manifest
↓
Compaction
↓
Bounded recovery
```

This creates a credible path from:

```text
Embedded MVP storage
```

to:

```text
Production knowledge-native storage engine
```

without prematurely turning AIKOQL into a generic database project.
