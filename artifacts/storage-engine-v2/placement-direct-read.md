# §13 evidence pack — placement-direct read (SE2-M39)

The §13 hot-path rule (spec line 726) requires a hot-key read to answer
within 10% of the warm path. The M38 stress measured the failing baseline
and named the subject: a `get_object` for a Segment-placed replica decoded
the key's whole equal-key run. M39 ships the fix and this pack is the §13
evidence: baseline, root cause, optimization, alternative.

## Baseline (pre-fix, M38 stress)

Workload: 100,000 objects × one hot key `k0`, one flush, two full
verification passes (228,572 reads; debug build).

| Metric | Value |
|--------|-------|
| Total wall (read phase) | 8852 s |
| Reads | 228,572 |
| Average per read | 38.7 ms |
| Entries decoded per read | ~50,000 (~630 ns/decode, debug) |

## Root cause

`get_object` answered Segment-placed replicas with the rid-filtered run
scan: newest-first, decode every entry of the key's seq-descending run
until the rid's row is found. A hot key carrying one row per replica makes
the run O(n) — the read pays O(n) even though the placement directory
already recorded the row's exact (segment, block, entry) position. The
directory was maintained but never consulted on the read path.

## Optimization (shipped, M39)

1. **Placement-direct dispatch** — `get_object` consults the placement
   directory first: a Segment placement names the row's exact
   (segment, block, entry) position, and the read decodes from there.
   O(RESTART_INTERVAL) entries instead of the run. A Memtable placement
   probes the memtables (a tombstone included) and shadows segments —
   the flip on every put/delete makes the placement authoritative.
2. **Block format v4** — a dense cadence table in each v4 data block
   records the byte offset of every 16th entry, so an arbitrary stored
   entry index decodes standalone (window ≤ 16) without scanning the
   block or rebuilding the restart table from its head.
3. **Fallback stays correct** — a multi-key object's anchor names its
   newest key only; a read at any other key falls back to the bounded
   scan, which answers every case (pd001 pins the ≤16-entry bound).

## Measured (post-fix, §43 probe — probe_m39 rows)

| Metric | Value |
|--------|-------|
| P50 read (100k hot-key reads × 2 passes) | 14.7 µs |
| Average read | 15.8 µs |
| Entries decoded per read | 8.5 (bound ≤ 16) |
| Read wall (2 × 100k) | 3.15 s (baseline 8852 s — ~2800×) |
| Identity lookup P50 | 700 ns |
| Replica lookup P50 | 500 ns |
| Placement lookup P50 | 500 ns |
| Directory resident bytes (100k) | 9.57 MB — (identity 2.88, replica 1.97, placement 4.72) |
| Directory persisted bytes (100k) | 14.7 MB across IDENTITY-/REPLICA-/PLACEMENT- logs |

The three directory lookups add < 2 µs total to a read that no longer pays
the run decode — the §13 hot-path rule (≤10% over warm) is satisfied by a
wide margin; the §44 matrix re-measures W1–W8 against the adopted baseline.

## Alternative considered and rejected

A separate per-block rid→offset index structure. Rejected: the placement
directory is already that index — rebuilt from its logs at recovery,
validated against the manifest — so a second structure would duplicate
its state and add a new drift surface. The dense cadence table costs
4 bytes per 16 entries inside blocks the flush writes anyway.
