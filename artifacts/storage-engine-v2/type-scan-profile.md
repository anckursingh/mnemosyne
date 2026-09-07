# Type Scan Profile (W5) — SE2-M26

Generated only when `SE2M26_NIGHTLY=1` (strict opt-in). Perf numbers are report cells, never asserts.

- Test: `v2_m26_scan_profile`
- Build mode: release
- Machine: windows/x86_64; 8 logical cores; AMD64 Family 23 Model 24 Stepping 1, AuthenticAMD
- Date: 2026-09-05
- Dataset: one v2 database, 100000 KOs / 10000 deep × 10 versions (SEED 0x270000); one W5 op = `k.scan_by_type` = 1 engine prefix scan over the type index (empty values) + 1 head_object per candidate (2 engine point gets — head + ~1.4 KiB version row — + wire decode + type/Deleted checks + authz read-lock)
- Index shape (capture-pinned): m7_0 → 100000 rows → 100000 returned (harness phase-2 `rmv(.., "m7_0")` restated every KO to m7_0); m7_1..99 → 1000 rows → 0 returned (stale phase-1 entries, rejected by the payload re-check after full decode — stale entries kept by design, kernel.rs:1282); mean candidates per matrix op = 1990
- Matrix reference (09-05 workloads.md, warm): W5 v2 27451 µs vs v1 5534 µs — the cell mixes both shapes via TYPE_ROUND: 10 rounds × 100 types = 1% m7_0 ops + 99% stale-type ops
- Decision-tree thresholds (fixed before the run): scan share < 15% → no index (W5 is get-bound); 15–40% → block-summary investigation opens; > 40% → scan-shape work (posting lists); kernel residual > 30% → kernel-side profiling follow-up

| leg | p50 | p95 | p99 | max | throughput |
|---|---|---|---|---|---|
| W5 kernel op — scan_by_type (rotating) | 27036 µs | 31522 µs | 37491 µs | 1350929 µs | 24 ops/s (mean 41147 µs) |
| engine scan — type/m7_t/ (rotating) | 264 µs | 348 µs | 671 µs | 38483 µs | 1518 ops/s (mean 659 µs) |
| kernel gets — k.get over scan candidates | 27498 µs | 36782 µs | 47821 µs | 1025095 µs | 26 ops/s (mean 38680 µs) |
| hot-type ceiling — m7_0 × 10 | 1304648 µs | 1355672 µs | 1355672 µs | 1355672 µs | 1 ops/s (mean 1307812 µs) |

| leg | lookups/op | cache hits/op | cache misses/op | blocks read/op | bytes read/op | entries decoded/op | get_wall/op |
|---|---|---|---|---|---|---|---|
| W5 kernel op — scan_by_type (rotating) | 3980 | 2512.2 | 1372.5 | 1372.5 | 22228133 | 33997 | 32017 µs |
| engine scan — type/m7_t/ (rotating) | 0 | 1.0 | 4.3 | 4.3 | 70149 | 2564 | 0 µs |
| kernel gets — k.get over scan candidates | 3980 | 2512.2 | 1367.2 | 1367.2 | 22141775 | 31433 | 32270 µs |
| hot-type ceiling — m7_0 × 10 | 200000 | 186269.0 | 8927.0 | 8927.0 | 143148109 | 1736241 | 773443 µs |

## Decomposition (sums over the legs)

- engine prefix scan: 659 µs of the 41147 µs mean W5 op (1.6%) — leg 2 runs the same rotation on the same prefix
- engine point gets: 32017 µs/op (77.8%) — get_wall accumulated by the gets inside the W5 op (mean 1990 candidates × 2 gets; the mean op includes the 1% m7_0 giant)
- kernel residual: 8471 µs/op (20.6%) = W5 wall − scan − engine gets (decode + type/Deleted checks + authz + assembly)
- per-candidate kernel check: 4.3 µs per candidate in the W5 op vs 3.2 µs per plain k.get in leg 3 (+32.2% per candidate beyond a plain get)
- hot-type ceiling: 1304648 µs p50 when the polluted m7_0 (100_000 KOs) is re-scanned (leg 4, cache-served) vs 27036 µs rotating
- bimodality: p50–p99 are ALL stale-type ops (1000 candidates → 0 returned); the 1% m7_0 op (100_000 candidates → 100_000 returned) is the max column — invisible to p99 but 35% of the mean wall


## Verdict

- scan share 1.6%: no type index / no posting lists / no block summaries — W5 is candidate-bound, not scan-bound (the index already resolves candidates; the cost is the per-candidate head_object); its warm gate-5 cell (27451/5534 = 4.96× v1, 09-05) sits inside the amended ≤8× bound.
- kernel residual 20.6%: no kernel instrumentation — the per-candidate work matches a plain get.
- stale-index note: 99% of matrix ops decode 1000 stale candidates and return 0 — wasted work by design (kernel keeps stale entries); m7_0's 100_000-row scan carries the tail. The harness shape is unchanged (matrix cells are the certification reference).
