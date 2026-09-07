# Directory growth certification (SE2-M40)

- date: 2026-09-07
- harness: `ckp008_growth_probe` with `SE2M40_NIGHTLY=1`
- workload per scale: 10,000 objects seeded (one key each), then
  N updates (a new key per update, round-robin over the objects),
  flush every 2,000 updates, Async durability; `checkpoint_bytes`
  = 0 (the pre-M40 regime) vs 2 MiB
- measured per arm: build wall, directory metadata bytes on disk
  (delta logs + checkpoints; segments/manifest/WAL excluded — identical
  across arms), and warm reopen wall (min of 3, page cache warm)

| updates | checkpoint | build s | metadata bytes | checkpoint bytes | log files | warm open ms |
|---|---|---|---|---|---|---|
| 100000 | off | 2.4 | 8071378 | 0 | 53 | 54.5 |
| 100000 | 2 MiB | 2.2 | 2526372 | 810034 | 13 | 23.4 |
| 300000 | off | 5.9 | 21273978 | 0 | 153 | 143.9 |
| 300000 | 2 MiB | 6.5 | 942060 | 810034 | 1 | 35.1 |
| 600000 | off | 12.5 | 41077878 | 0 | 303 | 347.6 |
| 600000 | 2 MiB | 14.3 | 1734216 | 810034 | 7 | 75.0 |

## The M12 gate

Recovery must be proportional to the checkpoint plus the deltas AFTER
it, never to the full metadata history. The rows above show the
off-arm's open climbing with the update count (every placement log
ever published is decoded) while the checkpoint arm's open stays
flat at the live-state size (~10K identity/replica records + the
trigger window of placement records).
