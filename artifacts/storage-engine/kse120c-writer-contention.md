# KSE-120C — Writer Contention Scaling (certification §5)

Date: 2026-09-01 · seed 0x120c0000 · engine: AikoqlStorageEngine · build profile: release · workload: 20000 puts per scenario (unique keys, 256 B values) · test: kse120c_writer_contention.rs

| writers | readers | writes | writes/sec | write P50/P95/P99 ms | reads | reads/sec | read P50/P95/P99 ms | wall s | recovered == acked |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 0 | 20000 | 1544 | 0.59 / 0.92 / 1.09 | 0 | — | — | 13.0 | ✓ (asserted, byte-exact) |
| 1 | 32 | 20000 | 1541 | 0.59 / 0.92 / 1.13 | 16000 | 1233 | 0.00 / 0.00 / 0.01 | 13.0 | ✓ (asserted, byte-exact) |
| 2 | 32 | 20000 | 1517 | 1.13 / 2.93 / 5.30 | 16000 | 1214 | 0.00 / 0.00 / 0.01 | 13.2 | ✓ (asserted, byte-exact) |
| 4 | 32 | 20000 | 1500 | 1.08 / 7.96 / 14.71 | 16000 | 1200 | 0.00 / 0.00 / 0.01 | 13.3 | ✓ (asserted, byte-exact) |
| 8 | 32 | 20000 | 1465 | 0.77 / 21.13 / 39.53 | 16000 | 1172 | 0.00 / 0.00 / 0.01 | 13.7 | ✓ (asserted, byte-exact) |
| 16 | 32 | 20000 | 1498 | 0.65 / 48.40 / 91.43 | 16000 | 1198 | 0.00 / 0.00 / 0.01 | 13.4 | ✓ (asserted, byte-exact) |
| 32 | 32 | 20000 | 1491 | 0.65 / 99.39 / 180.43 | 16000 | 1193 | 0.00 / 0.00 / 0.01 | 13.4 | ✓ (asserted, byte-exact) |


## Proposed SLOs (reported, not asserted)

- 100% acknowledged-write recovery at every writer count — the only asserted gate (all scenarios, above)
- write P50 at 1 writer <= 0.9 ms (measured 0.59 ms; 1.5x headroom)
- throughput must not collapse: 32-writer rate >= 25% of the 1-writer rate (measured 1491/sec vs 1544/sec = 97%) — serialization is intentional (log Mutex across append+fsync+apply, KSE-13 120a), so plateau is expected; a collapse would signal lock or scheduling pathology


## NOT_MEASURED (metrics that cannot be measured here)

- lock/queue wait: the serialized section is engine-internal — write P50 vs the 1-writer baseline IS the contention proxy; a separate number would need production instrumentation
- WAL append time / fsync time: one serialized section, engine-internal — not separable without production instrumentation; the behavioral pin is KSE-13 KSE-120a (log order == commit order)
- CPU: single-machine wall time is the scenario column; per-thread CPU attribution is not separable
- RSS: steady-state memory is KSE-19/143's surface; the contention matrix adds no durable state

## Honest limits

- contention surface is within-process threads — the engine does not support multi-process sharing (documented), and the kernel's own pipeline is single-writer; the 32-writer row is deliberately beyond any real AIKOQL workload
- readers hammer random keys, hit rate grows during the run; read latency includes None gets
- write latency includes fsync (the serialized section) — it is durability cost, not lock cost
- debug builds inflate CPU but not the serialization shape; nightly rows should be produced in release
- wall times race sibling tests (kse19 convention); evidence, not gates
