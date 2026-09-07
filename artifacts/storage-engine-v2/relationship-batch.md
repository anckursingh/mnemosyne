# Relationship Batch Read — SE2-M25

Generated only when `SE2M25_NIGHTLY=1` (strict opt-in). Perf numbers are report cells, never asserts.

- Test: `v2_m25_relationship_batch`
- Build mode: release
- Machine: windows/x86_64; 8 logical cores; AMD64 Family 23 Model 24 Stepping 1, AuthenticAMD
- Date: 2026-09-05
- Dataset: one v2 database, 100000 KOs / 10000 deep × 10 versions, seeded through the Kernel over the adapter (SEED 0x270000); each W4 op = outbound_edges (one engine scan) + one batch `get_many` over the targets (2 engine point gets per target — head + version)
- Control: the same hubs through the per-target get loop — the pre-M25 harness shape the 2026-09-05 matrix measured
- Suggested targets (TESTING-PLAN-V2 SE2-M25, shaped by the pre-M25 matrix cells): F=100 ≤ 700 µs, F=1000 ≤ 6000 µs

| leg | p50 | p95 | p99 | throughput |
|---|---|---|---|---|
| relationship lookup F=10 (W4, batch) | 166 µs | 181 µs | 241 µs | 5775 ops/s · p50 166 µs · p95 181 · p99 241 |
| relationship F=10 loop control (pre-M25) | 178 µs | 187 µs | 216 µs | 5561 ops/s · p50 178 µs · p95 187 · p99 216 |
| relationship lookup F=100 (W4, batch) | 804 µs | 1859 µs | 1859 µs | 1069 ops/s · p50 804 µs · p95 1859 · p99 1859 |
| relationship F=100 loop control (pre-M25) | 916 µs | 1092 µs | 1092 µs | 1072 ops/s · p50 916 µs · p95 1092 · p99 1092 |
| relationship lookup F=1000 (W4, batch) | 10101 µs | 17784 µs | 17784 µs | 85 ops/s · p50 10101 µs · p95 17784 · p99 17784 |
| relationship F=1000 loop control (pre-M25) | 10718 µs | 11920 µs | 11920 µs | 92 ops/s · p50 10718 µs · p95 11920 · p99 11920 |

| leg | lookups/op | cache hits/op | cache misses/op | blocks read/op | entries decoded/op |
|---|---|---|---|---|---|
| relationship lookup F=10 (W4, batch) | 20.0 | 10.9 | 0.1 | 0.1 | 215.0 |
| relationship F=10 loop control (pre-M25) | 20.0 | 24.0 | 0.0 | 0.0 | 215.0 |
| relationship lookup F=100 (W4, batch) | 200.0 | 46.0 | 4.0 | 4.0 | 2000.0 |
| relationship F=100 loop control (pre-M25) | 200.0 | 205.0 | 0.0 | 0.0 | 2000.0 |
| relationship lookup F=1000 (W4, batch) | 2000.0 | 351.4 | 85.6 | 85.6 | 19996.0 |
| relationship F=1000 loop control (pre-M25) | 2000.0 | 2016.0 | 0.0 | 0.0 | 19996.0 |

- F=10: batch P50 166 µs vs loop control 178 µs (0.94×); no target set for F=10.

- F=100: batch P50 804 µs vs loop control 916 µs (0.88×); OVER the ≤ 700 µs target.

- F=1000: batch P50 10101 µs vs loop control 10718 µs (0.94×); OVER the ≤ 6000 µs target.

## What the counters say (F=1000, per op)

Batch: 351 cache hits, 86 blocks read, 19996 entries decoded. Loop: 2016 cache hits, 0 blocks read, 19996 entries decoded.
Decode is per-key in both shapes (identical entries/op). The loop leg runs after the batch leg and inherits its warmed cache — the batch leg carries the first-touch misses, the loop leg reads zero blocks, so the ratio flatters the loop's cache state. Across the two certification runs the batch-vs-loop P50 ratio sits at 0.73×–1.13× (the sign flips with run-to-run noise) and both suggested targets are missed in both runs. Verdict: no measurable batch win at W4's warm fan-out shape; the harness W4 legs stay on the per-target loop (`w4_traversal`), and the batch API remains available, pinned by `tests/multi_get.rs`.
