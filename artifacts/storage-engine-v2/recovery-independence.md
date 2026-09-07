# Recovery Independence Test — SE2-M3

Generated only when `SE2M3_NIGHTLY=1` (strict opt-in). Perf numbers are
report cells, never asserts — the report regenerates only with the env set.

- Test: `recovery_independence_10gib_segments_100mib_wal`
- Build mode: debug
- Environment: windows (fabricated values — not a real AIKOQL workload)
- Historical segments: 10.00 GiB across 20 segments
- Active WAL: 100.0 MiB
- Open with segments: 3801 ms
- Open without segments (control, same WAL): 4323 ms
- Segment overhead: 0 ms

Verdict: PASS — open cost is dominated by the active WAL (reported,
not asserted). Known limitation: fabricated dataset, not an AIKOQL
production shape.
