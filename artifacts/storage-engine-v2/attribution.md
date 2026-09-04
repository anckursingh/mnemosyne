# Point-Read Cost Attribution — SE2-M21

Generated only when `SE2M21_ATTRIB=1` (strict opt-in). Perf numbers are report cells, never asserts.

- Test: `v2_attribution_probe`
- Build mode: release
- Machine: windows/x86_64; 8 logical cores; AMD64 Family 23 Model 24 Stepping 1, AuthenticAMD
- Date: 2026-09-04
- Dataset: one v2 database, 100000 KOs / 10000 deep × 10 versions / 20000 ops per leg, seeded through the Kernel over the adapter (SEED 0x270000); the mechanism legs run on a second small Db with the same row shape
- Reference: M19 warm W1/W2 P50 ≈ 37 µs ≈ 27 µs engine + 10 µs kernel; M17 hot-path 3.5 µs; M18 hot context 92.8 µs

Each row = P50/P95/P99 over 20000 ops; engine phases are per-op ReadPathStats deltas (SE2-M8 counters + the M21 lock_wait/bloom/get_wall closure), kernel overhead = external wall − engine get_wall. SE2-M22: the bloom row still covers all bloom work for a get — the key hash is computed once per get inside the first segment's probe timer (was once per segment).

## W1 kernel get (k.get)

the gate-5 leg: 2 engine gets per op

counters: lookups 40000 · memtable hits 1168 · segments 210722 · cache hits 13948 misses 25055 · blocks read 25055 · bytes read 406125340 · entries decoded 316584

| phase | p50 | p95 | p99 |
|---|---|---|---|
| wall (external) | 33.50 µs | 61.30 | 84.60 |
| get_wall (engine gets) | 30.10 µs | 57.00 | 79.50 |
| lock_wait | 0.10 µs | 0.20 | 0.20 |
| memtable lookup | 1.10 µs | 2.20 | 5.20 |
| bloom probe | 1.70 µs | 2.30 | 2.60 |
| index lookup | 2.00 µs | 3.40 | 4.80 |
| block cache lookup | 1.80 µs | 3.50 | 4.80 |
| block io | 18.70 µs | 44.60 | 66.00 |
| block decode | 2.50 µs | 4.30 | 5.20 |
| residual (engine untimed) | 2.40 µs | 3.90 | 4.80 |
| overhead (kernel + adapter) | 3.40 µs | 4.80 | 6.20 |

## W2 kernel get (fresh sample)

same storage leg, independent sample

counters: lookups 40000 · memtable hits 1238 · segments 208975 · cache hits 13956 misses 24971 · blocks read 24971 · bytes read 404778444 · entries decoded 314323

| phase | p50 | p95 | p99 |
|---|---|---|---|
| wall (external) | 32.80 µs | 60.60 | 82.70 |
| get_wall (engine gets) | 28.90 µs | 56.00 | 77.10 |
| lock_wait | 0.10 µs | 0.20 | 0.20 |
| memtable lookup | 1.30 µs | 2.30 | 5.50 |
| bloom probe | 1.80 µs | 2.30 | 2.70 |
| index lookup | 2.00 µs | 3.30 | 4.60 |
| block cache lookup | 1.80 µs | 3.70 | 4.90 |
| block io | 16.10 µs | 43.20 | 62.50 |
| block decode | 2.60 µs | 4.60 | 5.70 |
| residual (engine untimed) | 2.50 µs | 4.00 | 5.20 |
| overhead (kernel + adapter) | 3.80 µs | 4.80 | 7.60 |

## Engine leg — head/<koid>

the small row (KSE-18)

counters: lookups 20000 · memtable hits 609 · segments 110067 · cache hits 19291 misses 196 · blocks read 196 · bytes read 3231993 · entries decoded 157011

| phase | p50 | p95 | p99 |
|---|---|---|---|
| wall (external) | 4.60 µs | 6.70 | 15.40 |
| get_wall (engine gets) | 4.40 µs | 6.40 | 15.10 |
| lock_wait | 0.00 µs | 0.10 | 0.10 |
| memtable lookup | 0.40 µs | 0.60 | 2.20 |
| bloom probe | 0.70 µs | 1.10 | 1.20 |
| index lookup | 0.20 µs | 0.40 | 0.60 |
| block cache lookup | 1.40 µs | 2.10 | 2.70 |
| block io | 0.00 µs | 0.00 | 0.00 |
| block decode | 1.00 µs | 2.20 | 2.70 |
| residual (engine untimed) | 0.60 µs | 0.80 | 1.10 |
| overhead (kernel + adapter) | 0.20 µs | 0.30 | 0.40 |

## Engine leg — ko/<koid><ts>

the ~1.4 KB version row

counters: lookups 20000 · memtable hits 565 · segments 100406 · cache hits 2136 misses 17384 · blocks read 17384 · bytes read 280115754 · entries decoded 159066

| phase | p50 | p95 | p99 |
|---|---|---|---|
| wall (external) | 18.00 µs | 35.30 | 52.30 |
| get_wall (engine gets) | 17.70 µs | 35.00 | 51.90 |
| lock_wait | 0.00 µs | 0.10 | 0.10 |
| memtable lookup | 0.60 µs | 1.10 | 2.70 |
| bloom probe | 0.90 µs | 1.30 | 1.50 |
| index lookup | 1.40 µs | 2.10 | 2.60 |
| block cache lookup | 0.10 µs | 2.30 | 2.70 |
| block io | 12.50 µs | 29.10 | 45.60 |
| block decode | 0.70 µs | 1.60 | 2.40 |
| residual (engine untimed) | 1.40 µs | 2.20 | 3.40 |
| overhead (kernel + adapter) | 0.30 µs | 0.40 | 0.60 |

## Memtable hit (active memtable)

the M17 hot-path mechanism, same row shape

counters: lookups 20000 · memtable hits 20000 · segments 0 · cache hits 0 misses 0 · blocks read 0 · bytes read 0 · entries decoded 0

| phase | p50 | p95 | p99 |
|---|---|---|---|
| wall (external) | 0.80 µs | 1.30 | 1.50 |
| get_wall (engine gets) | 0.60 µs | 0.90 | 1.10 |
| lock_wait | 0.00 µs | 0.10 | 0.10 |
| memtable lookup | 0.40 µs | 0.60 | 0.80 |
| bloom probe | 0.00 µs | 0.00 | 0.00 |
| index lookup | 0.00 µs | 0.00 | 0.00 |
| block cache lookup | 0.00 µs | 0.00 | 0.00 |
| block io | 0.00 µs | 0.00 | 0.00 |
| block decode | 0.00 µs | 0.00 | 0.00 |
| residual (engine untimed) | 0.20 µs | 0.30 | 0.30 |
| overhead (kernel + adapter) | 0.30 µs | 0.40 | 0.40 |

## Cache hit (flushed + warmed block)

cached-block mechanism

counters: lookups 20000 · memtable hits 0 · segments 20000 · cache hits 20000 misses 0 · blocks read 0 · bytes read 0 · entries decoded 120235

| phase | p50 | p95 | p99 |
|---|---|---|---|
| wall (external) | 1.80 µs | 2.00 | 2.20 |
| get_wall (engine gets) | 1.50 µs | 1.80 | 1.90 |
| lock_wait | 0.00 µs | 0.10 | 0.10 |
| memtable lookup | 0.10 µs | 0.10 | 0.10 |
| bloom probe | 0.20 µs | 0.20 | 0.20 |
| index lookup | 0.10 µs | 0.20 | 0.20 |
| block cache lookup | 0.30 µs | 0.50 | 0.50 |
| block io | 0.00 µs | 0.00 | 0.00 |
| block decode | 0.40 µs | 0.60 | 0.70 |
| residual (engine untimed) | 0.40 µs | 0.50 | 0.60 |
| overhead (kernel + adapter) | 0.30 µs | 0.30 | 0.40 |

## Where a warm W1 `k.get` goes (M22 input)

- external wall P50 33.50 µs = engine get_wall 30.10 µs + kernel/adapter overhead 3.40 µs
- the kernel leg runs 2 engine gets per op; engine-leg P50s: head row 4.40 µs, version row 17.70 µs
- dominant engine phase at adoption scale: block io (18.70 µs of 30.10 µs get_wall); engine residual 2.40 µs
