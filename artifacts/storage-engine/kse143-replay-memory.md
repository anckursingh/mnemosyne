# KSE-143 — Large Replay Resource Stability (certification §7)

Date: 2026-09-01 · seed 0x14300000 · engine: AikoqlStorageEngine · build profile: release · sizes run: 1 MB/10 MB/100 MB · test: kse143_replay_memory.rs

Peak replay memory multiplier = **8.77x** (peak 112848896 B / final 12861440 B, at 100.00 MB WAL).

Beyond the ~9 MB process baseline, peak grows at 1.04 B per WAL byte (marginal slope, 100.00 MB row). Proposed deployment memory requirement: baseline + 1.04 B/WAL-byte x the operational WAL cap x 1.2 headroom — e.g. an operational 100 MB WAL implies ~134 MB RAM reserved for open().

| WAL (exact) | records | live keys | baseline RSS | peak RSS | final RSS | post-query RSS | peak/final | open ms |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1.00 MB | 449 | 2402 | 6316032 B | 7286784 B | 7282688 B | 7286784 B | 1.00x | 3.6 |
| 10.00 MB | 4487 | 8471 | 8921088 B | 12406784 B | 12402688 B | 12406784 B | 1.00x | 34.5 |
| 100.00 MB | 44864 | 8910 | 9277440 B | 112848896 B | 12861440 B | 12861440 B | 8.77x | 428.8 |


## Honest limits

- peak RSS is polled at 100 ms over the child's WHOLE window (baseline + open + query) — an upper bound on the replay peak and a lower bound on the true transient (spikes between samples are missed); a fast smoke child can outrun the sampler's startup, shown as NOT_SAMPLED
- baseline/final/post-query RSS are phase-anchored self-reports (one PowerShell call each, ~200 ms of child wall time — not replay time)
- open() materializes the WHOLE WAL plus the live store (KSE-19 §25 verdict) — the multiplier measures that design's startup cost, not a hidden cache
- the workload keeps a fixed 10K-key keyspace; peak memory is dominated by WAL bytes + final store, both linear in the reported dimensions
- RSS is Windows-only (PowerShell WorkingSet64); non-Windows rows carry timings with NOT_SAMPLED RSS
- child runs race sibling tests for CPU (kse19 convention); the memory numbers are process-isolated and not affected by siblings
