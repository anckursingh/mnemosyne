# KSE-14 — Snapshot and Restore (KSE-130..132)

Date: 2026-08-31 · seed 0x140000 · engine: AikoqlStorageEngine · snapshot file format: redb (trait default, engine-independent)

## Config

| test | shape | result |
|---|---|---|
| KSE-130 | rich dataset (3 types, provenance events, both edge directions, 3-generation supersede lineage, tombstone, schema row with constraints) → backup → restore into junk-seeded db | byte-exact; snapshot file itself a redb db with the same rows |
| KSE-131 | 8 readers × 200 ops racing the backup of a static store | every read == pre-recorded baseline; restored byte-exact |
| KSE-132 | 4 writers × 300 ops storm, capture mid-storm (≥50 commits durable) | restored state passed the full structural sweep |

## Expecteds (§20)

- equivalence: byte-exact key-space equality source vs restored — stronger than the per-dimension list (KOs/facts/relations/provenance/temporal state/constraints); kernel-level spot checks after the documented restart-after-restore flow (type scans, 3-version lineage, tombstone lifecycle)
- internally consistent point-in-time (KSE-131): static store → byte-exact snapshot; readers proceed through the capture untouched (the snapshot shares the read lock)
- documented point-in-time guarantee (KSE-132): snapshot represents one valid database state, never a mixed state — the storm snapshot passed the model-free structural sweep (derived == image from its own heads, every version row has a head, (koid,ts) unique, one journal event per version, seqs exactly 1..=n, rebuild (0,0))

## How the guarantee holds (implementation facts)

- `MemoryEngine::scan` holds the read guard across the whole collect; `write_batch` holds the write guard across every row — a snapshot is the state at one instant between batches
- the kernel takes no pipe lock around backup — writers commit freely around the capture; supersede (2 batches) captured between its batches lands in a real, coherent intermediate state (successor committed, old head not yet marked), which the sweep admits by construction
- restore is ONE write batch on the destination: dst readers see old-or-new, never a mix (pinned in KSE-130); reusing a destination replaces, never merges (QA2-PROP-001 — junk rows resurrecting would have failed the byte-exact pin)

## Honest limits

- KSE-131 readers run against a static store (the §20 shape); the mutating read/write mix is KSE-132's storm, whose capture point is unknowable by design — structural sweep only, no model-exact assertions possible
- restore old-or-new is pinned for correctness, not perf-measured
- no nightly variant: the guarantee holds at any batch boundary, and bigger storms buy coverage, not evidence (KSE-13 carries the throughput load)
