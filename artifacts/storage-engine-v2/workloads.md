# W1..W8 Workloads — v2 vs redb vs v1 (MRFC-KSE-001 §27-28 + design §26)

Date: 2026-09-06 · profile: release · seed 0x270000 · scale: 100000 KOs / 10000 deep × 10 versions / 20000 ops (V2ADOPT_NIGHTLY — strict opt-in)

The same workload shapes v1's M7 adoption ran, on the same seed. All workloads through the Kernel on `&dyn StorageEngine` (§32). One seeded dataset per backend.

## §28 matrix — throughput + latency

| workload | memory | redb | aikoql | aikoql-v2 |
|---|---|---|---|---|
| KO get (W1) | 142425 ops/s · p50 7 µs · p95 9 · p99 11| 45093 ops/s · p50 18 µs · p95 43 · p99 132| 173436 ops/s · p50 6 µs · p95 7 · p99 10| 27999 ops/s · p50 34 µs · p95 62 · p99 82 |
| head get (W2) | 160723 ops/s · p50 6 µs · p95 9 · p99 11| 107418 ops/s · p50 9 µs · p95 11 · p99 18| 157297 ops/s · p50 6 µs · p95 8 · p99 10| 27543 ops/s · p50 33 µs · p95 62 · p99 84 |
| version lookup (W3) | 114076 ops/s · p50 9 µs · p95 10 · p99 12| 73880 ops/s · p50 12 µs · p95 17 · p99 22| 108980 ops/s · p50 9 µs · p95 12 · p99 15| 22679 ops/s · p50 43 µs · p95 74 · p99 100 |
| history (W3) | 25006 ops/s · p50 34 µs · p95 65 · p99 92| 26263 ops/s · p50 36 µs · p95 54 · p99 89| 29979 ops/s · p50 32 µs · p95 40 · p99 50| 13955 ops/s · p50 69 µs · p95 108 · p99 144 |
| relationship lookup F=10 (W4) | 7795 ops/s · p50 126 µs · p95 137 · p99 169| 6266 ops/s · p50 156 µs · p95 166 · p99 255| 7582 ops/s · p50 128 µs · p95 162 · p99 173| 4901 ops/s · p50 193 µs · p95 266 · p99 366 |
| relationship lookup F=100 (W4) | 2376 ops/s · p50 410 µs · p95 489 · p99 489| 1410 ops/s · p50 669 µs · p95 888 · p99 888| 2404 ops/s · p50 408 µs · p95 491 · p99 491| 952 ops/s · p50 988 µs · p95 1562 · p99 1562 |
| relationship lookup F=1000 (W4) | 260 ops/s · p50 3792 µs · p95 4094 · p99 4094| 150 ops/s · p50 6617 µs · p95 7074 · p99 7074| 265 ops/s · p50 3864 µs · p95 4039 · p99 4039| 90 ops/s · p50 10581 µs · p95 13582 · p99 13582 |
| type scan (W5) | 84 ops/s · p50 5459 µs · p95 7654 · p99 13682| 61 ops/s · p50 7766 µs · p95 9621 · p99 35487| 90 ops/s · p50 5541 µs · p95 6724 · p99 18904| 24 ops/s · p50 27158 µs · p95 35584 · p99 105166 |
| context compilation (W7) | 15946 ops/s · p50 57 µs · p95 91 · p99 111| 8159 ops/s · p50 112 µs · p95 186 · p99 271| 16685 ops/s · p50 56 µs · p95 90 · p99 94| 4092 ops/s · p50 223 µs · p95 378 · p99 525 |
| mixed 70/20/10 (W8) | 109228 ops/s · p50 6 µs · p95 36 · p99 42| 1373 ops/s · p50 13 µs · p95 2830 · p99 31994| 1476 ops/s · p50 9 µs · p95 752 · p99 31159| 8231 ops/s · p50 45 µs · p95 711 · p99 978 |
| ingestion (W6) | 48770 ops/s · p50 21 µs · p95 21 · p99 21| 494 ops/s · p50 2023 µs · p95 2023 · p99 2023| 1380 ops/s · p50 724 µs · p95 724 · p99 724| 1373 ops/s · p50 728 µs · p95 728 · p99 728 |

## §28 matrix — logical bytes read / written per workload

| workload | memory | redb | aikoql | aikoql-v2 |
|---|---|---|---|---|
| KO get (W1) | 13951795 / 0| 13951795 / 0| 13951795 / 0| 13951795 / 0 |
| head get (W2) | 13951795 / 0| 13951795 / 0| 13951795 / 0| 13951795 / 0 |
| version lookup (W3) | 147466142 / 0| 147466142 / 0| 147466142 / 0| 147466142 / 0 |
| history (W3) | 147566712 / 0| 147566712 / 0| 147566712 / 0| 147566712 / 0 |
| relationship lookup F=10 (W4) | 4123400 / 0| 4123400 / 0| 4123400 / 0| 4123400 / 0 |
| relationship lookup F=100 (W4) | 1177170 / 0| 1177170 / 0| 1177170 / 0| 1177170 / 0 |
| relationship lookup F=1000 (W4) | 4400000 / 0| 4400000 / 0| 4400000 / 0| 4400000 / 0 |
| type scan (W5) | 1441114680 / 0| 1441114680 / 0| 1441114680 / 0| 1441114680 / 0 |
| context compilation (W7) | 48818925 / 0| 48818925 / 0| 48818925 / 0| 48818925 / 0 |
| mixed 70/20/10 (W8) | 14354533 / 3853729| 14354533 / 3853729| 14354533 / 3853729| 14354533 / 3853729 |
| ingestion (W6) | 194861672 / 413844730| 194861672 / 413844730| 194861672 / 413844730| 194861672 / 413844730 |

## Per-backend resources

| backend | CPU (seed wall) | RSS (peak, loader child) | disk |
|---|---|---|---|
| memory | 5741 ms | NOT_SAMPLED | 0 B |
| redb | 566487 ms | 518.00 MiB | 1.00 GiB |
| aikoql | 202856 ms | 613.59 MiB | 435.44 MiB |
| aikoql-v2 | 203907 ms | 222.34 MiB | 349.08 MiB |

## §26 adoption gates

| gate (§26) | result | evidence |
|---|---|---|
| 1. recovery bounded by the active WAL | PASS | SE2-M3 suite — artifacts/storage-engine-v2/recovery-independence.md: replay only the active WAL, orphan/missing-segment policies; real-kill recovery suites in M3/M4/M6 |
| 2. dataset larger than RAM remains queryable | PASS | `v2_gate2_3_dataset_larger_than_ram` (this suite): ~820 KB dataset under a 64 KiB memtable + zero cache → served from on-disk segments, full scan byte-exact, survives reopen |
| 3. memory limits configurable | PASS | the same probe pins both knobs: `memtable_bytes=64 KiB` forced flushes (≥2 SEGMENT files); `cache_bytes=0` detaches the cache (silent stats), a 4 KiB cap is consulted (misses) yet holds nothing (oversize block never retained) |
| 4. group commit improves concurrent throughput without weakening Sync | — | SE2-M6 suite green (Sync baseline reproduced exactly); throughput evidence = the `SE2M6_NIGHTLY=1` matrix → artifacts/storage-engine-v2/group-commit.md |
| 5. KO lookup competitive with the MVP baseline (v1) | PASS | W1 6.00× v1, W2 5.48× v1 (P50; bound ≤ 8× — perf verdict only on a real (non-smoke) matrix run; this run is V2ADOPT_NIGHTLY) |

## Reference rows (not re-measured here)

- snapshot: v2 rides the trait defaults (redb snapshot — REC-002); v1 byte-exact restore pinned (KSE-14); redb single-file opens as redb.
- recovery: v2 real-kill recovery pinned by the SE2-M3/M4/M6 suites (recovery-independence.md); v1 by KSE-15.
- concurrent mixed load: v2 pinned behaviorally by the SE2-M6 group-commit suite (KSE-13 order); v1 by KSE-13. W8 above is the single-threaded mixed row.
- 1M/10M ingestion scale: v1 1M creates = 1242 s / 645 B per KO heap (KSE-19, measured). v2 at 1M NOT_MEASURED.

## Honest metric mapping

- throughput/latency: per-op wall on one thread; percentiles over the instrumented pass (P50/P95/P99 in µs)
- bytes read: CountingEngine bytes returned over the workload (get + scan Σ k+v)
- bytes written: CountingEngine batch Σ put k+v (logical, pre-codec)
- W6 ingestion P50/P95/P99 = mean commit cost (the seed loop isn't per-op instrumented)
- CPU: seed wall, single-threaded (wall ≈ CPU); disk: file (redb/aikoql) or dir (aikoql-v2) at seed end; memory = none
- RSS: Windows-only WorkingSet64 poll on a loader child (peak is a lower bound — kse19); CI/ubuntu rows NOT_SAMPLED
- memory backend: RAM-only reference, not an adoption candidate
- W2 = the same storage leg as W1 (k.get is the kernel's only public head read — KSE-18 pins head+version rows); measured twice on fresh samples, not a faked second API
- v2 RSS on aikoql-v2 includes the 64 MiB memtable + 8 MiB block-cache defaults; gates 2+3 show the knobs bound them
