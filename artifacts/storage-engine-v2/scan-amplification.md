# Scan Amplification Report — SE2-M12

Generated only when `SE2M12_NIGHTLY=1` (strict opt-in). Perf numbers are
report cells, never asserts — the report regenerates only with the env set.

- Test: `scan_amplification_report`
- Build mode: release
- Machine: windows/x86_64; 8 logical cores; AMD64 Family 23 Model 24 Stepping 1, AuthenticAMD
- Scanner: the SE2-M12 k-way merged iterator — per-segment lazy cursors
decode one entry at a time from cache-served raw blocks (restart-table
seek), memtable heads merge in layer order (newest wins, tombstones
suppress). No whole-block Vec, no BTreeMap of every prefix key.
- Answers pinned byte-exact on the cold scan (prefix, ascending, warm ==
cold).

## W4 shape — entity out-edges (relo/<src>/..., 10 entities × 500 rows)
  rows 500, bytes_returned 24000
- cold: decoded 506, blocks 2, bytes_read 33023, wall 0.3 ms
- warm: decoded 506, blocks 0, bytes_read 0, wall 0.2 ms
  per-scan decode amp 1.01x, cold I/O amp 1.38x

## W5 shape — type rows (type/<name>/<koid>, 4 types × 500 koids)
  rows 500, bytes_returned 14000
- cold: decoded 504, blocks 2, bytes_read 33046, wall 0.2 ms
- warm: decoded 504, blocks 0, bytes_read 0, wall 0.2 ms
  per-scan decode amp 1.01x, cold I/O amp 2.36x
