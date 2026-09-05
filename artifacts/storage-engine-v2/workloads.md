# W1..W8 Workloads — v2 vs redb vs v1 (MRFC-KSE-001 §27-28 + design §26)

Date: 2026-09-05 · profile: release · seed 0x270000 · scale: 100000 KOs / 10000 deep × 10 versions / 20000 ops (V2ADOPT_NIGHTLY — strict opt-in)

The same workload shapes v1's M7 adoption ran, on the same seed. All workloads through the Kernel on `&dyn StorageEngine` (§32). One seeded dataset per backend.

## §28 matrix — throughput + latency

| workload | memory | redb | aikoql | aikoql-v2 |
|---|---|---|---|---|
| KO get (W1) | 162595 ops/s · p50 6 µs · p95 8 · p99 10| 53281 ops/s · p50 15 µs · p95 37 · p99 104| 179426 ops/s · p50 5 µs · p95 7 · p99 9| 26130 ops/s · p50 35 µs · p95 66 · p99 90 |
| head get (W2) | 172944 ops/s · p50 6 µs · p95 8 · p99 10| 127427 ops/s · p50 7 µs · p95 11 · p99 12| 157305 ops/s · p50 6 µs · p95 8 · p99 10| 26925 ops/s · p50 34 µs · p95 64 · p99 88 |
| version lookup (W3) | 113311 ops/s · p50 9 µs · p95 10 · p99 13| 80790 ops/s · p50 12 µs · p95 16 · p99 26| 110268 ops/s · p50 9 µs · p95 11 · p99 14| 21933 ops/s · p50 44 µs · p95 77 · p99 106 |
| history (W3) | 29054 ops/s · p50 32 µs · p95 46 · p99 54| 24449 ops/s · p50 36 µs · p95 60 · p99 88| 28749 ops/s · p50 32 µs · p95 46 · p99 62| 13747 ops/s · p50 71 µs · p95 110 · p99 148 |
| relationship lookup F=10 (W4) | 7874 ops/s · p50 125 µs · p95 136 · p99 173| 6290 ops/s · p50 154 µs · p95 178 · p99 250| 7893 ops/s · p50 125 µs · p95 134 · p99 154| 5129 ops/s · p50 189 µs · p95 206 · p99 237 |
| relationship lookup F=100 (W4) | 2427 ops/s · p50 403 µs · p95 495 · p99 495| 1488 ops/s · p50 659 µs · p95 813 · p99 813| 2407 ops/s · p50 408 µs · p95 477 · p99 477| 981 ops/s · p50 961 µs · p95 1479 · p99 1479 |
| relationship lookup F=1000 (W4) | 223 ops/s · p50 4452 µs · p95 5626 · p99 5626| 151 ops/s · p50 6561 µs · p95 7017 · p99 7017| 216 ops/s · p50 4814 µs · p95 5789 · p99 5789| 90 ops/s · p50 10508 µs · p95 13600 · p99 13600 |
| type scan (W5) | 82 ops/s · p50 5464 µs · p95 7874 · p99 39784| 59 ops/s · p50 7730 µs · p95 9920 · p99 25275| 89 ops/s · p50 5534 µs · p95 6949 · p99 10440| 24 ops/s · p50 27451 µs · p95 31861 · p99 61818 |
| context compilation (W7) | 15038 ops/s · p50 57 µs · p95 108 · p99 152| 9206 ops/s · p50 102 µs · p95 149 · p99 181| 15359 ops/s · p50 57 µs · p95 91 · p99 131| 4239 ops/s · p50 222 µs · p95 341 · p99 440 |
| mixed 70/20/10 (W8) | 106577 ops/s · p50 6 µs · p95 36 · p99 45| 1495 ops/s · p50 13 µs · p95 2588 · p99 30399| 724 ops/s · p50 10 µs · p95 831 · p99 47974| 8295 ops/s · p50 45 µs · p95 716 · p99 966 |
| ingestion (W6) | 42511 ops/s · p50 24 µs · p95 24 · p99 24| 384 ops/s · p50 2605 µs · p95 2605 · p99 2605| 1359 ops/s · p50 736 µs · p95 736 · p99 736| 1320 ops/s · p50 758 µs · p95 758 · p99 758 |

## §28 matrix — logical bytes read / written per workload

| workload | memory | redb | aikoql | aikoql-v2 |
|---|---|---|---|---|
| KO get (W1) | 13951795 / 0| 13951795 / 0| 13951795 / 0| 13913136 / 0 |
| head get (W2) | 13951795 / 0| 13951795 / 0| 13951795 / 0| 13913136 / 0 |
| version lookup (W3) | 147466142 / 0| 147466142 / 0| 147466142 / 0| 147466142 / 0 |
| history (W3) | 147566712 / 0| 147566712 / 0| 147566712 / 0| 147566712 / 0 |
| relationship lookup F=10 (W4) | 4123400 / 0| 4123400 / 0| 4123400 / 0| 4067200 / 0 |
| relationship lookup F=100 (W4) | 1177170 / 0| 1177170 / 0| 1177170 / 0| 1144870 / 0 |
| relationship lookup F=1000 (W4) | 4400000 / 0| 4400000 / 0| 4400000 / 0| 4279640 / 0 |
| type scan (W5) | 1441114680 / 0| 1441114680 / 0| 1441114680 / 0| 1437125780 / 0 |
| context compilation (W7) | 48818925 / 0| 48818925 / 0| 48818925 / 0| 48719559 / 0 |
| mixed 70/20/10 (W8) | 14354533 / 3853729| 14354533 / 3853729| 14354533 / 3853729| 14320634 / 3849734 |
| ingestion (W6) | 194861672 / 413844730| 194861672 / 413844730| 194861672 / 413844730| 194861672 / 413840735 |

## Per-backend resources

| backend | CPU (seed wall) | RSS (peak, loader child) | disk |
|---|---|---|---|
| memory | 6587 ms | NOT_SAMPLED | 0 B |
| redb | 729508 ms | 512.58 MiB | 1.00 GiB |
| aikoql | 206111 ms | 613.43 MiB | 435.44 MiB |
| aikoql-v2 | 212203 ms | 206.17 MiB | 348.02 MiB |

## §26 adoption gates

| gate (§26) | result | evidence |
|---|---|---|
| 1. recovery bounded by the active WAL | PASS | SE2-M3 suite — artifacts/storage-engine-v2/recovery-independence.md: replay only the active WAL, orphan/missing-segment policies; real-kill recovery suites in M3/M4/M6 |
| 2. dataset larger than RAM remains queryable | PASS | `v2_gate2_3_dataset_larger_than_ram` (this suite): ~820 KB dataset under a 64 KiB memtable + zero cache → served from on-disk segments, full scan byte-exact, survives reopen |
| 3. memory limits configurable | PASS | the same probe pins both knobs: `memtable_bytes=64 KiB` forced flushes (≥2 SEGMENT files); `cache_bytes=0` detaches the cache (silent stats), a 4 KiB cap is consulted (misses) yet holds nothing (oversize block never retained) |
| 4. group commit improves concurrent throughput without weakening Sync | — | SE2-M6 suite green (Sync baseline reproduced exactly); throughput evidence = the `SE2M6_NIGHTLY=1` matrix → artifacts/storage-engine-v2/group-commit.md |
| 5. KO lookup competitive with the MVP baseline (v1) | PASS | W1 6.54× v1, W2 5.61× v1 (P50; bound ≤ 8× — perf verdict only at V2ADOPT_NIGHTLY=1, this run is V2ADOPT_NIGHTLY) |

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
