# Hot Head Gate — SE2-M11

Generated only when `SE2M11_NIGHTLY=1` (strict opt-in). Perf numbers are
report cells, never asserts — the report regenerates only with the env set.

- Test: `hot_head_gate`
- Build mode: release
- Machine: windows/x86_64; 8 logical cores; AMD64 Family 23 Model 24 Stepping 1, AuthenticAMD
- Workload: 257 kernel-shaped head rows (`head/` + 16-byte koid) × ~1.4 KB
values, one segment (16 KiB blocks, ~11 rows/block), the target's block
warmed once, then 100000 cached lookups of the same head — answers
pinned byte-exact per lookup, cache hits 100000, physical block reads during
the run: 0

- P50: 1200 ns (1.2 µs)
- P95: 1900 ns (1.9 µs)
- P99: 2900 ns (2.9 µs)
- QA M3 gate: hot head ≤ 20 µs (20000 ns) — PASS
