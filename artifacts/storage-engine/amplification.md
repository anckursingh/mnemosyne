# KSE-16..18 — Amplification (MRFC-KSE-001 §22-24)

Date: 2026-09-01 · seed 0x160000 · dataset: 100 KOs (create + rel update + 2 updates each) · debug build, numbers from this suite run

## KSE-16 — storage amplification

| metric | bytes |
|---|---:|
| logical (Σ ko/ value bytes — payload/versions/provenance packed) | 263040 |
| physical (WAL file) | 509486 |
| live store (Σ k+v of every row) | 372715 |
| rows | 1602 |
| space amplification (disk/logical) | 1.94× |
| in-memory amplification (live/logical) | 1.42× |
| encrypted physical (same dataset, EncryptedStore) | 619715 |
| encryption overhead | +21.64% |

Breakdown (prefix-level Σ(k+v)): head: 3800 B, 100 rows; ke: 65600 B, 400 rows; ko: 273840 B, 400 rows; meta: 75 B, 2 rows; reli: 13200 B, 300 rows; relo: 13200 B, 300 rows; type: 3000 B, 100 rows

Honest mapping: the §22 sub-rows relationships/provenance/evidence are INSIDE the packed ko/ value (codec-level) — the store-level split is ko/ payload vs the relo/reli index rows; a finer decomposition would need codec-level decoding. Evidence enters only via supersede, which is not part of this dataset.

## KSE-17 — write amplification (durable bytes around ONE op)

| op class | aikoql disk B | aikoql logical B | aikoql batches | redb disk B | rocksdb disk B |
|---|---|---:|---:|---:|---:|
| create | 660 | 278 | 1 | 0 | 633 |
| update | 1458 | 747 | 1 | 0 | 1401 |
| relationship update | 1626 | 811 | 1 | 0 | 1559 |
| temporal version | 1660 | 845 | 1 | 0 | 1593 |
| provenance update | 1718 | 879 | 1 | 0 | 1651 |
| evidence update (supersede) | 2170 | 1454 | 2 | 0 | 2106 |

MemoryEngine: 0 physical by definition (no durability). Honest rows: redb/rocksdb deltas are file-LENGTH deltas — they under-report in-page writes and jump on page/B-tree growth; aikoql deltas are exact WAL appends. The evidence-minting update path is supersede (the request surface has no evidence field) — 2 batches, pinned above.

## KSE-18 — read amplification (per workload: logical objects → engine records, bytes)

| workload | logical | gets | scans | pairs | bytes returned |
|---|---|---:|---:|---:|---:|
| get KO | 1 | 2 | 0 | 0 | 719 |
| get KO + facts | 4 | 6 | 1 | 3 | 2289 |
| get KO + neighbors | 3 | 2 | 2 | 6 | 983 |
| get history | 4 | 0 | 1 | 4 | 2724 |

The record counts are PINNED equal across Memory/redb/RocksDB/Aikoql (§32 — the kernel makes the same requests on every backend). Physical IO per backend: Aikoql reads 0 bytes at query time (all state in RAM; its durable cost is the open-time WAL replay — KSE-15); redb/RocksDB block reads NOT_MEASURED (no tracing wired, KSE-5 precedent). "Compile context" is the compiler crate's workload (QA2-CONC-001) — its storage footprint is exactly the get+facts+neighbors workloads above.
