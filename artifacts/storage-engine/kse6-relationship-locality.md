# KSE-050..052 — Relationship Locality (MRFC-KSE-001 §12)

Measured 2026-08-31 on X (debug build — indicative, not release numbers).
Dataset per backend: 5 hubs with 1/10/100/1,000/10,000 outbound neighbors (interleaved "links"/"cites"), 11,111 leaf KOs, one database per backend, 10 timed reps per lookup.
Allocations: NOT_MEASURED (no counting-allocator instrumentation wired).

| fan-out | op | redb P50/P95/P99 (µs) | redb engine reqs | RocksDB P50/P95/P99 (µs) | Aikoql P50/P95/P99 (µs) | Aikoql engine reqs |
|---|---|---|---|---|---|---|
| 1 | all | 53 / 140 / 140 | 0 gets + 1 scans (1 pairs, 44 B returned) | 18 / 112 / 112 | 4 / 44 / 44 | 0 gets + 1 scans (1 pairs, 44 B returned) |
| 1 | links | 52 / 53 / 53 | 0 gets + 1 scans (1 pairs, 44 B returned) | 15 / 16 / 16 | 4 / 5 / 5 | 0 gets + 1 scans (1 pairs, 44 B returned) |
| 1 | cites | 42 / 50 / 50 | 0 gets + 1 scans (0 pairs, 0 B returned) | 12 / 13 / 13 | 3 / 3 / 3 | 0 gets + 1 scans (0 pairs, 0 B returned) |
| 10 | all | 96 / 103 / 103 | 0 gets + 1 scans (10 pairs, 440 B returned) | 32 / 34 / 34 | 10 / 17 / 17 | 0 gets + 1 scans (10 pairs, 440 B returned) |
| 10 | links | 70 / 71 / 71 | 0 gets + 1 scans (5 pairs, 220 B returned) | 23 / 35 / 35 | 6 / 7 / 7 | 0 gets + 1 scans (5 pairs, 220 B returned) |
| 10 | cites | 70 / 72 / 72 | 0 gets + 1 scans (5 pairs, 220 B returned) | 23 / 24 / 24 | 7 / 7 / 7 | 0 gets + 1 scans (5 pairs, 220 B returned) |
| 100 | all | 569 / 612 / 612 | 0 gets + 1 scans (100 pairs, 4400 B returned) | 191 / 198 / 198 | 66 / 126 / 126 | 0 gets + 1 scans (100 pairs, 4400 B returned) |
| 100 | links | 305 / 311 / 311 | 0 gets + 1 scans (50 pairs, 2200 B returned) | 100 / 110 / 110 | 34 / 34 / 34 | 0 gets + 1 scans (50 pairs, 2200 B returned) |
| 100 | cites | 328 / 1214 / 1214 | 0 gets + 1 scans (50 pairs, 2200 B returned) | 100 / 156 / 156 | 33 / 43 / 43 | 0 gets + 1 scans (50 pairs, 2200 B returned) |
| 1000 | all | 5329 / 7076 / 7076 | 0 gets + 1 scans (1000 pairs, 44000 B returned) | 1713 / 1837 / 1837 | 625 / 762 / 762 | 0 gets + 1 scans (1000 pairs, 44000 B returned) |
| 1000 | links | 2653 / 2828 / 2828 | 0 gets + 1 scans (500 pairs, 22000 B returned) | 939 / 1339 / 1339 | 298 / 324 / 324 | 0 gets + 1 scans (500 pairs, 22000 B returned) |
| 1000 | cites | 2770 / 3835 / 3835 | 0 gets + 1 scans (500 pairs, 22000 B returned) | 849 / 910 / 910 | 297 / 302 / 302 | 0 gets + 1 scans (500 pairs, 22000 B returned) |
| 10000 | all | 54858 / 66679 / 66679 | 0 gets + 1 scans (10000 pairs, 440000 B returned) | 18058 / 20242 / 20242 | 7924 / 8240 / 8240 | 0 gets + 1 scans (10000 pairs, 440000 B returned) |
| 10000 | links | 26816 / 34795 / 34795 | 0 gets + 1 scans (5000 pairs, 220000 B returned) | 9042 / 9873 / 9873 | 3818 / 4133 / 4133 | 0 gets + 1 scans (5000 pairs, 220000 B returned) |
| 10000 | cites | 27024 / 28639 / 28639 | 0 gets + 1 scans (5000 pairs, 220000 B returned) | 9300 / 12202 / 12202 | 3561 / 3784 / 3784 | 0 gets + 1 scans (5000 pairs, 220000 B returned) |

## Consistency (KSE-052)

All 11,111 edges verified bidirectionally on every measured backend: for each outbound (hub -type-> leaf), inbound_edges(leaf, type) contains the hub. Zero divergences — the relo/reli index pair is symmetric over AikoqlStorageEngine exactly as over redb/RocksDB.

## Adjacency-structure verdict

The existing layout IS the knowledge-aware adjacency: every KO's neighbors live in one contiguous key range (relo/<hub>/…), so a lookup is one seek + one linear-in-range scan — see the engine-reqs column (single scan, pairs == fan-out). A custom packed adjacency would save at most the per-row key overhead, and only by moving the write path off the kernel's own index rows. No prototype built; revisit only if a release-build profile shows the per-row key copy dominating.
