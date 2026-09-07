# KSE-13 — Concurrency (KSE-120)

Date: 2026-08-31 · seed 0x130000 · engine: AikoqlStorageEngine

## Config

| leg | threads | ops/thread | result |
|---|---|---|---|
| KSE-120a raw-engine write_batch | 32 | 200 | live == replay (byte-equal), 0 lost puts |
| KSE-120b readers | 32 | 250 | all shape pins held every read |
| KSE-120b writers | 4 | 400 | all commits exactly once |

## Latency (KSE-120b, this machine)

| op class | count | P50 / P95 / P99 |
|---|---|---|
| auth_probe | 278 | 26 / 36 / 66 µs |
| create | 96 | 4556 / 6082 / 8202 µs |
| delete | 70 | 4503 / 6457 / 6775 µs |
| history | 550 | 120 / 291 / 413 µs |
| lookup | 2735 | 20 / 36 / 75 µs |
| relate | 226 | 4905 / 7112 / 10064 µs |
| supersede | 186 | 5954 / 8958 / 13289 µs |
| traversal | 1086 | 5 / 10 / 30 µs |
| type_scan | 851 | 127 / 254 / 423 µs |
| unrelate | 152 | 4825 / 7568 / 8312 µs |
| update | 870 | 4816 / 7075 / 10988 µs |
| **throughput** | 9600 ops | 4530 ops/s wall |

## Expecteds (§19)

- deadlocks: none — all threads joined; a hang fails the CI timeout
- corruption: none — post-storm sweep (derived set byte-equal, head
uniqueness, version rows, journal count, rebuild (0,0))
- invalid logical reads: none — readers pinned head/event/edge/scan
shape on every op
- duplicate commits: none — final versions and lineages match the
model exactly
- authorization bypass: none — bob's KOs invisible on every shape

## Honest limits

- CPU/RSS/IO: NOT_MEASURED (no counting allocator or IO tracing)
- contention: NOT_MEASURED directly; the kernel's single-writer
pipeline serializes commits by design (§19), visible in writer
P95/P99 vs reader latency
- context compilation: out of the engine's reach — concurrent leg
QA2-CONC-001 in `crates/ingestion/tests/qa2_concurrency.rs`
