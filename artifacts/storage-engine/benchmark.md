# W1..W8 Workload Benchmarks — Comparison Matrix (MRFC-KSE-001 §27-28)

Date: 2026-09-01 · profile: release · seed 0x270000 · scale: 100000 KOs / 10000 deep × 10 versions / 20000 ops (M7_NIGHTLY — strict opt-in)

All workloads through the Kernel on `&dyn StorageEngine` (§32). One seeded dataset per backend.

## §28 matrix — throughput + latency

| workload | memory | redb | aikoql | rocksdb |
|---|---|---|---|---|
| KO get (W1) | 158977 ops/s · p50 6 µs · p95 9 · p99 13| 45034 ops/s · p50 19 µs · p95 44 · p99 121| 104206 ops/s · p50 7 µs · p95 13 · p99 40| 24082 ops/s · p50 38 µs · p95 73 · p99 128 |
| head get (W2) | 171963 ops/s · p50 6 µs · p95 7 · p99 10| 121517 ops/s · p50 8 µs · p95 10 · p99 13| 137439 ops/s · p50 7 µs · p95 11 · p99 15| 27293 ops/s · p50 36 µs · p95 64 · p99 107 |
| version lookup (W3) | 101033 ops/s · p50 9 µs · p95 14 · p99 21| 81234 ops/s · p50 12 µs · p95 16 · p99 23| 100873 ops/s · p50 10 µs · p95 12 · p99 18| 18401 ops/s · p50 43 µs · p95 118 · p99 198 |
| history (W3) | 27473 ops/s · p50 33 µs · p95 50 · p99 70| 23178 ops/s · p50 36 µs · p95 65 · p99 105| 23586 ops/s · p50 39 µs · p95 66 · p99 89| 10437 ops/s · p50 84 µs · p95 186 · p99 292 |
| relationship lookup F=10 (W4) | 8049 ops/s · p50 123 µs · p95 131 · p99 155| 5142 ops/s · p50 155 µs · p95 381 · p99 450| 6329 ops/s · p50 144 µs · p95 234 · p99 276| 3402 ops/s · p50 274 µs · p95 374 · p99 573 |
| relationship lookup F=100 (W4) | 2424 ops/s · p50 403 µs · p95 489 · p99 489| 1448 ops/s · p50 666 µs · p95 866 · p99 866| 2184 ops/s · p50 447 µs · p95 531 · p99 531| 625 ops/s · p50 1511 µs · p95 2303 · p99 2303 |
| relationship lookup F=1000 (W4) | 254 ops/s · p50 4005 µs · p95 4030 · p99 4030| 145 ops/s · p50 6635 µs · p95 7667 · p99 7667| 228 ops/s · p50 4391 µs · p95 4470 · p99 4470| 71 ops/s · p50 12578 µs · p95 22978 · p99 22978 |
| type scan (W5) | 80 ops/s · p50 5526 µs · p95 8356 · p99 40930| 59 ops/s · p50 7971 µs · p95 10188 · p99 19146| 88 ops/s · p50 5504 µs · p95 7091 · p99 16065| 20 ops/s · p50 22518 µs · p95 36100 · p99 66988 |
| context compilation (W7) | 12128 ops/s · p50 58 µs · p95 116 · p99 183| 8569 ops/s · p50 111 µs · p95 162 · p99 205| 14995 ops/s · p50 58 µs · p95 99 · p99 152| 2703 ops/s · p50 352 µs · p95 522 · p99 736 |
| mixed 70/20/10 (W8) | 110144 ops/s · p50 6 µs · p95 36 · p99 40| 1487 ops/s · p50 13 µs · p95 2757 · p99 30058| 1950 ops/s · p50 9 µs · p95 842 · p99 16848| 7719 ops/s · p50 46 µs · p95 754 · p99 1037 |
| ingestion (W6) | 42835 ops/s · p50 23 µs · p95 23 · p99 23| 448 ops/s · p50 2232 µs · p95 2232 · p99 2232| 1299 ops/s · p50 770 µs · p95 770 · p99 770| 1256 ops/s · p50 796 µs · p95 796 · p99 796 |

## §28 matrix — logical bytes read / written per workload

| workload | memory | redb | aikoql | rocksdb |
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
| memory | 6537 ms | NOT_SAMPLED | 0 B |
| redb | 625088 ms | 512.99 MiB | 1.00 GiB |
| aikoql | 215612 ms | 612.39 MiB | 435.44 MiB |
| rocksdb | 222942 ms | 123.00 MiB | 110.08 MiB |

## Reference rows (not re-measured here)

- snapshot: aikoql byte-exact restore equivalence + capture-is-one-instant sweep pinned (KSE-14); redb single-file opens as redb (KSE-14); rocksdb/memory NOT_MEASURED.
- recovery: aikoql real-kill child harness recovered seqs exactly 1..=n (KSE-15); cold start staged: replay 92.4 ms / 2,200 rows, kernel metadata 22.3 ms, first query 27 µs (10K dataset); redb/rocksdb NOT_MEASURED.
- concurrent mixed load: aikoql pinned behaviorally by KSE-13 (32-256 readers / 4-32 writers); other backends NOT_MEASURED. W8 above is the single-threaded mixed row.
- 1M/10M ingestion scale: aikoql 1M creates = 1242 s / 645 B per KO heap (KSE-19, measured); 10M = projection (6.45 GB heap). redb/rocksdb at 1M NOT_MEASURED.

## Honest metric mapping

- throughput/latency: per-op wall on one thread; percentiles over the instrumented pass (P50/P95/P99 in µs)
- bytes read: CountingEngine bytes returned over the workload (get + scan Σ k+v)
- bytes written: CountingEngine batch Σ put k+v (logical, pre-codec)
- W6 ingestion P50/P95/P99 = mean commit cost (the seed loop isn't per-op instrumented)
- CPU: seed wall, single-threaded (wall ≈ CPU); disk: file (redb/aikoql) or dir (rocksdb) at seed end; memory = none
- RSS: Windows-only WorkingSet64 poll on a loader child (peak is a lower bound — kse19); CI/ubuntu rows NOT_SAMPLED
- memory backend: RAM-only reference, not an adoption candidate
- W2 = the same storage leg as W1 (k.get is the kernel's only public head read — KSE-18 pins head+version rows); measured twice on fresh samples, not a faked second API
