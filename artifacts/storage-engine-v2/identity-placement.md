# Identity & Placement Report — SE2-M38 (§40 stress + §45 memory + §46 Q2/Q5)

Generated only when `SE2M38_NIGHTLY=1` (strict opt-in). Perf/memory numbers
are report cells, never asserts — the report regenerates only with the env set.

- Tests: `identity_stress::st001_relocation_stress` (stress), `identity_stress::mem001_directory_memory_report` (memory)
- Build mode: debug
- Machine: windows/x86_64; 8 logical cores; AMD64 Family 23 Model 24 Stepping 1, AuthenticAMD
- Engine shape measured: one L0 flush per quarter batch + one compact; identity/replica/placement directories are log-published (§23 order), in-memory HashMaps rebuilt from logs + WAL at open; placements move Memtable → Segment on flush/compact

## §40 stress — 100,000 objects — PASS

Cycle: 100,000 objects created in four flush batches (k0), every 7th object
updated (k1), flush, compact, restart, full verification, restart again, full
verification. Pinned: ObjectId → LogicalId → ReplicaId unchanged across every
restart (verified 100,000 × 2 restarts). Values: every committed value answers
byte-exact after compaction and both restarts (114,286 reads per pass, two
passes). PhysicalLocation: allowed to move — sampled placements (step 997)
resolve to live entries carrying the replica's id in the merged segment.

Wall: 8,852 s (debug build) — dominated by the rid-filtered point-read scan
below, not the engine's write path.

## §45 memory gate — 1M objects (mandatory)

WorkingSet64 plateaus at the half (500k) and full (1M) markers inside a
3-second park; marginal = the second half-million.

- Identity directory: 46,137,360 B = 46.1 B/object (capacity-exact: slots × 24 B + control bytes)
- Replica directory: 31,457,296 B = 31.5 B/object (capacity-exact: slots × 16 B + control bytes)
- Placement directory: 75,497,488 B = 75.5 B/object (capacity-exact: slots × 40 B + control bytes)
- Directories total: 153.1 B/object
- Total storage RSS: half 216,121,344 B → full 425,897,984 B; marginal 209,776,640 B for 500,000 objects
- Residual (memtable rows + allocator + WAL buffers): 266 B/object
- **Bytes per object: 419 B/object (measured)**

## §46 Q2 — bytes-per-object cost

| Component | Size |
|---|---|
| ObjectId | 16 B |
| LogicalId | 8 B |
| ReplicaId | 8 B |
| PhysicalLocation (wire: segment 8 + block 4 + offset 4 + generation 8) | 24 B |
| Placement entry in memory (PhysicalLocation + variant tag, 8-aligned) | 32 B |
| Identity map entry (ObjectId+LogicalId pair) | 24 B |
| Replica map entry (LogicalId+ReplicaId pair) | 16 B |
| Placement map entry (ReplicaId+Placement pair) | 40 B |
| Per-map control bytes | capacity/14 × 16 B |
| Allocator + residual (measured total − directories − memtable) | see below |

The per-directory capacity accounting above is the exact decomposition of the
measured 419 B/object.

## §46 Q5 — compaction amplification (100k stress)

Measured from the stress directory after the compact:

- Records relocated (compact entries_out): 114,286 of 114,286 entries in (5 segments → 1) — the per-(key,rid) merge loses nothing: every (key,rid) winner survives
- Location entries updated (placement records, all generations): 314,286 (10,371,594 B) — births (100k) + flush moves (114,286) + compact relocations (100k)
- Identity records: 100,000 (2,400,104 B); replica records: 100,000 (2,400,104 B)
- Live segment bytes: 3,686,129 B (32.3 B/entry with identity)
- Metadata write amplification (directory log bytes / live bytes): 4.116× cumulative; the compact's own relocation (100k records ≈ 3.3 MB) vs its 3.69 MB output ≈ 0.9×

## Known ceiling — rid-filtered point read scans the key's whole run

One row per replica makes a hot key's equal-key run long (100,000 entries in
the stress), and the v2 restart table bounds decode only across distinct keys:
a `get_object` for a replica whose row sits at the tail of its key's
seq-descending run decodes the entire run (~50,000 entries average in the
stress — ~630 ns/entry in a debug build; the stress's two verification passes
are ~2× 5·10⁹ decodes). Correctness is complete; the cost is the §43 latency
metric's subject. The fix (M39, §13 evidence pack): the placement directory
itself became the index — a get_object for a Segment-placed replica decodes
its stored (block, entry) position directly through a dense per-block cadence
table (block format v4), O(RESTART_INTERVAL) entries instead of the key's
whole run. No second index structure; the placement directory that recovery
rebuilds is the one index, so nothing extra can drift.

## Bug fixes this milestone exposed

The §40 stress is the first workload where one key carries many replicas
(one row per replica — a hot key's run is long). Two defects, both found by
the stress's own verification, both fixed TDD:

1. **Compaction merge dropped all but one row per key** — the byte-API
   last-writer-wins rule leaked into the replica space, silently destroying
   rows whose (key, rid) had more than one survivor. Fix: per-(key, rid)
   winner semantics — each rid's newest entry survives.
2. **Block restarts landed mid-run** — the v2 writer skips equal-key restart
   candidates, so a run starting mid-cadence got its restart at the next
   cadence point, mid-run, hiding the run-head entries from every lookup.
   Fix: writer repositions the restart to the run head (re-encoding the head
   with its full key); the reader's `get_by_rid` falls through block
   boundaries while an equal-key run spills.
