# Block Cache Warm/Cold Matrix — SE2-M7

Generated only when `SE2M7_NIGHTLY=1` (strict opt-in). Perf numbers are
report cells, never asserts — the report regenerates only with the env set.

- Test: `warm_block_cache_speedup`
- Build mode: release
- Workload: 2000 keys × 200-byte values, one segment (64 KiB blocks),
2000 random-order gets per pass, answers pinned identical across passes

- Cold (cache off), 1 pass: 558 ms
- Warm (64 MiB cache), 2nd pass: 125 ms
- Warm hits/misses/evictions/bytes: 3993/7/0/438000
