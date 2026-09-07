# Backend Conformance (MRFC-KSE-001 §7 + §26)

Date: 2026-09-01 · the six KSE-1 asserts from one shared definition (`tests/common` `kse` module), run per backend as granular tests (`conformance.rs`, KSE-1) and as this matrix (`kse20_backend_conformance.rs`, KSE-20). All through `&dyn StorageEngine` — no backend-specific type above the boundary (§32).

| backend | KSE-001..006 | persistence (reopen) | physical format | read path |
|---|---|---|---|---|
| memory | 6/6 ✓ | none — RAM-only by definition | in-RAM BTreeMap (no file) | RAM mirror |
| redb | 6/6 ✓ | reopen ✓ | single B-tree file | storage (page cache) |
| aikoql | 6/6 ✓ | reopen ✓ | append-only WAL file | RAM mirror — 0 disk at query time (KSE-5/KSE-18) |
| rocksdb | 6/6 ✓ | reopen ✓ | LSM directory (WAL + SSTs) | storage (block cache) |

## Divergences — explicit documented capabilities

- persistence: MemoryEngine has none by definition (in-RAM only); the three durable backends served the reopen probe identically (write → drop handle → reopen → read).
- durability knobs: Aikoql fsyncs every batch (pinned by KSE-3 corruption/envelope tests, KSE-9 fault injection, KSE-15 real-kill recovery); redb/RocksDB durability is their own engine's knob, outside this conformance contract.
- physical format: redb = a single B-tree file (opens directly as redb — KSE-14); RocksDB = an LSM directory (WAL + SSTs); Aikoql = an append-only enveloped WAL (KSE-3); Memory = no file.
- read path: Memory/Aikoql serve reads from the in-RAM mirror (Aikoql's query-time disk IO measured at 0 — KSE-5/KSE-18); redb/RocksDB read from storage through their caches.
- concurrency: all four serialize writes at the engine boundary; Aikoql's contract under concurrent access is pinned behaviorally by KSE-13.

**No accidental semantic divergence found:** the six §7 asserts pass identically on all four backends, and every difference above is a documented capability of the engine's design.
