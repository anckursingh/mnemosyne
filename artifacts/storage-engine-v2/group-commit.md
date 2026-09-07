# Group Commit Effectiveness — SE2-M13

Generated only when `SE2M6_NIGHTLY=1` (strict opt-in). Perf numbers are
report cells, never asserts — the report regenerates only with the env set.

- Test: `group_commit_effectiveness`
- Build mode: release
- Machine: windows/x86_64; 8 logical cores; AMD64 Family 23 Model 24 Stepping 1, AuthenticAMD
- Workload: 200 single-op batches TOTAL, 128-byte values, 1 MiB+ memtable
(no flush during the run); arm (c) = 8 writers × 25 DISTINCT batches —
the M6 matrix's 8-writer arm ran 200 per writer and labeled it ×25,
hiding the coalescing; cells name `batches_submitted` so a row cannot
lie again.

- Sync, 1 writer, batches_submitted=200: 159 ms, 200 fsyncs, 0.80 ms/batch
- GroupCommit, 1 writer, wait=0, batches_submitted=200: 164 ms, 200 fsyncs, 0.82 ms/batch
- GroupCommit, 8 writers × 25, wait=0, batches_submitted=200: 45 ms, 37 fsyncs, 0.23 ms/batch, avg group 5.4

- Pipelining ceiling (SE2-M13, documented): in-flight = 1 per writer by
design (the blocking ack); coalescing = concurrent-submitter count, not
the wait window — under the blocking API the window is dead time (the M6
wait=5ms arm — 1600 batches, mislabeled ×25 — measured 3131 ms wall with
200 groups: ~5 ms window tax per group for zero extra coalescing,
2026-09-02), so the default stays ZERO. Upgrade path, if a workload ever
needs window-filling: a non-blocking submit API.
