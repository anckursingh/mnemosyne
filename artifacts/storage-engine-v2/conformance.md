# Backend Conformance — v2 (MRFC-KSE-001 §7 + §26)

Date: 2026-09-06 · the six KSE-1 asserts from one shared definition (`tests/common` `kse` module, copied verbatim from v1's harness), run per backend as granular tests (`tests/engine.rs`, V2-Adopt) and as this matrix (`kse20_backend_conformance.rs`, KSE-20). All through `&dyn StorageEngine` — no backend-specific type above the boundary (§32).

| backend | KSE-001..006 | persistence (reopen) | physical format | read path |
|---|---|---|---|---|
| memory | 6/6 ✓ | none — RAM-only by definition | in-RAM BTreeMap (no file) | RAM mirror |
| redb | 6/6 ✓ | reopen ✓ | single B-tree file | storage (page cache) |
| aikoql | 6/6 ✓ | reopen ✓ | append-only WAL file | RAM mirror — 0 disk at query time (KSE-5/KSE-18) |
| aikoql-v2 | 6/6 ✓ | reopen ✓ | bounded WAL + immutable segments + manifest (dir) | memtable + segment readers (bloom-skipped, block-cached) |

## Divergences — explicit documented capabilities

- persistence: MemoryEngine has none by definition (in-RAM only); the three durable backends served the reopen probe identically (write → drop handle → reopen → read).
- durability knobs: aikoql-v2 fsyncs every Sync batch (pinned by the SE2-M2 WAL goldens, the M3/M4/M6 child-kill recovery suites); redb/RocksDB durability is their own engine's knob; v1 aikoql fsyncs every batch (KSE-9/KSE-15).
- physical format: redb = a single B-tree file (opens directly as redb — KSE-14); aikoql = an append-only enveloped WAL (KSE-3); aikoql-v2 = a database directory — bounded WAL, immutable segments, manifest/CURRENT (SE2-M0..M5); Memory = no file.
- read path: Memory/aikoql serve reads from the in-RAM mirror; aikoql-v2 reads the memtable and, per segment, seeks by index, skips via the bloom pre-check, and caches decoded blocks within `cache_bytes` (SE2-M7); redb/RocksDB read from storage through their caches.
- concurrency: all four serialize writes at the engine boundary; aikoql-v2 additionally offers GroupCommit mode (committer thread, one fsync per group — SE2-M6) behind the same Sync baseline.

**No accidental semantic divergence found:** the six §7 asserts pass identically on all four backends, and every difference above is a documented capability of the engine's design.
