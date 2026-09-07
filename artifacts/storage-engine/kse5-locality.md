# KSE-040 — KO Read Amplification (MRFC-KSE-001 §11)

Measured 2026-08-31 on X (debug build — indicative, not release numbers).
Dataset: 100 KOs × (create with 3 fact payload props + provenance marker, rels update with 3 outbound links, 2 version updates); retrieval = get + history + outbound_edges + inbound_edges; 20 reps × 100 KOs timed per backend.

| metric | redb | RocksDB | Aikoql |
|---|---|---|---|
| logical requests / retrieval | 2 gets + 3 scans (4 pairs, 1908 B returned) | 2 gets + 3 scans (4 pairs, 1908 B returned) | 2 gets + 3 scans (4 pairs, 1908 B returned) |
| logical writes (seed) | 401 batches, 2601 puts, 600 dels | same | same |
| physical records (read time) | 84 leaf pages | NOT_MEASURED (perf context off) | 0 (RAM; 401 WAL records replayed at open) |
| physical blocks | 84 leaf + 4 branch pages | NOT_MEASURED | 0 (RAM) |
| bytes read / retrieval | NOT_MEASURED (mmap, no IO tracing) | NOT_MEASURED | 0 (RAM after replay) |
| durable store bytes | 3686400 (live 231835) | 375169 | 360206 (live 231835, amplification 1.55×) |
| P50 / P95 / P99 (µs) | 233 / 325 / 375 | 437 / 611 / 779 | 57 / 71 / 115 |
| reopen cost | 0 (lazy mmap) | 0 | 21 ms replay of 401 WAL records (360206 bytes) |

## Read

- The kernel issues the SAME logical requests over every backend (pinned by the test) — locality is purely physical.
- Aikoql's read path is 0-disk (RAM) but it pays for that at open: the whole write history replays every restart (the unbounded-log ponytail in lib.rs). Amplification above = WAL bytes / live bytes; it grows with every version commit.
- redb pays lazily (page faults during reads) and keeps only live pages on disk.
- RocksDB per-read IO is unmeasured until perf-context counters are wired (feature `kse5-rocksdb` covers latency + resident bytes only).
