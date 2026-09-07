# Tier-depth read certification — SE2-M17/M18

2026-09-03 (M17) and 2026-09-04 (M18), not pushed (SE2 flow: commit per
milestone, user pushes).
Machine: windows/x86_64, 8 cores (AMD64 Family 23 Model 24), 30 GiB RAM.
Config under test: M16 defaults (`l0_compact_trigger = 4`, `l0_tier_ratio = 1`),
8 MiB decoded-block cache, 64 KiB blocks.

## What M16 traded, and what this closes

M16's size tier lets L0 pile to ≤17 segments before the merge fires (measured
end state: 16 L0 + L1) — the trade for −76% merge I/O. A get() at that depth
walks memtable → newest-first segments (range-skip → bloom → index → block),
so the open question was the read cost at depth ~17 vs 2 under count-only.
This certifies it.

## The mechanical claim — pinned (not measured)

`crates/storage/aikoql-v2/tests/db_tiered_read.rs` (2 tests, both green):

1. `tier_depth_answers_match_oracle` — ≥11 segments, sequential flush ranges:
   every get and the full scan byte-exact vs an independent BTreeMap oracle
   AND vs a count-only twin (ratio 0) fed the same ops; an absent key past
   the pile: every segment range-skipped, zero blooms probed, zero blocks
   read.
2. `tier_depth_fanout_absorbed` — the adversarial shape (every segment's
   range spans the target, the range-skip cannot prune): the bloom absorbs
   the fan-out — ≤3 index searches over ≥11 segments, and block I/O ≤ the
   false-positive count, never the depth.

Perf numbers below are report cells (the QA doc's rule); the asserts above
are the honesty pins.

## Measured cells — the SE2M17_READS loader phase

DS-PERF-S (3,202,200 rows, seed 87.2 s, tier state = L1 + 1 L0 = depth 2):

    head get · cold · tier   ops=    400 wall=      8.9ms p50= 21800 p95= 36500 p99= 52100 | blocks=402 bytes=6595398 segs=741 hits=0 misses=0
    version get · cold · tier ops=    400 wall=     10.3ms p50= 23000 p95= 41600 p99= 52500 | blocks=405 bytes=6640904 segs=741 hits=0 misses=0
    absent get · cold · tier ops=    400 wall=      0.2ms p50=   400 p95=   400 p99=   500 | blocks=0 bytes=0 segs=800 hits=0 misses=0
    type scan · cold · tier  ops=     20 wall=      8.8ms p50=440500 p95=496900 p99=544600 | blocks=82 bytes=1355162 segs= 0 hits=0 misses=0
    head get · warm · tier   ops=    400 wall=      2.0ms p50=  4900 p95=  6100 p99=  8700 | blocks=0 bytes=0 segs=741 hits=402 misses=0
    head get · hot · tier    ops= 100000 wall=    219.2ms p50=  2000 p95=  2300 p99=  4000 | blocks=0 bytes=0 segs=200000 hits=100000 misses=0

DS-PERF-L (32,002,200 rows, seed 1073.2 s, tier state = 16 L0 + L1 = depth 17):

    head get · cold · tier   ops=    400 wall=     12.9ms p50= 27300 p95= 59500 p99= 82300 | blocks=450 bytes=7360009 segs=5230 hits=0 misses=0
    version get · cold · tier ops=    400 wall=     13.3ms p50= 28400 p95= 58800 p99= 80600 | blocks=452 bytes=7413063 segs=5230 hits=0 misses=0
    absent get · cold · tier ops=    400 wall=      0.7ms p50=  1700 p95=  1700 p99=  1800 | blocks=0 bytes=0 segs=6000 hits=0 misses=0
    type scan · cold · tier  ops=     20 wall=    123.5ms p50=6226400 p95=7132500 p99=8026100 | blocks=749 bytes=12364161 segs= 0 hits=0 misses=0
    head get · warm · tier   ops=    400 wall=      3.9ms p50=  9500 p95= 15000 p99= 19400 | blocks=0 bytes=0 segs=5230 hits=450 misses=0
    head get · hot · tier    ops= 100000 wall=    365.6ms p50=  3500 p95=  4000 p99=  4600 | blocks=0 bytes=0 segs=1500000 hits=100000 misses=0

(p50/p95/p99 in ns — the probe's own units; ops = lookups, one scan row =
`n / N_TYPES` rows each.)

## Verdicts vs the QA read gates

| gate | S cell | L cell | verdict |
|---|---|---|---|
| cold point ≤ 100 µs | 21.8 µs | 27.3 µs | PASS — 3.7× headroom at L |
| warm ≤ 50 µs | 4.9 µs | 9.5 µs | PASS — 5.3× headroom at L |
| hot head ≤ 20 µs | 2.0 µs | 3.5 µs | PASS — 5.7× headroom at L |
| absent walk / type scan | 0.4 µs / 440.5 µs | 1.7 µs / 6226.4 µs | report cells (no gates) |

## What the cells say

- Depth costs what the mechanics promise. At L, an absent get = 15.0
  segments considered, 0 blocks, 0 bytes, 1.7 µs — the walk is pure range
  checks. A cold point get = ~1.125 blocks at 27.3 µs vs M14's count-only
  22.0 µs: +5.3 µs for ~8× the segments.
- The type scan at depth 17 lands UNDER M14's count-only depth-2 cell
  (6226 vs 6813 µs). No claim beyond the cells: the tier's single fat L1
  index is at least not worse for scans.
- Warm/hot rows are cache-served: 0 block reads, hits ≈ ops × 1.125 — the
  same 1.125 ratio the cold rows' block reads show, i.e. ~12.5% of head
  values straddle a block boundary and cost a second block.

## Honest notes

- "Cold" = the decoded-block cache is detached (`cache_bytes = 0` reopen),
  not page-cache-cold — the OS page cache still serves the files; M14's
  caveat applies unchanged.
- The hot row is cache-hot, not memtable-hot: it pre-warms one key then
  repeats it, so it certifies the full-depth walk (segs/op = depth)
  served from cache — M14's hot-head row (memtable head, flat 2.4 µs at
  L) stands for the memtable path.
- S is depth 2 (five flushes → L1 + one L0): the L cells carry the
  tier-depth certification; S is the mid-scale row.
- Seed walls S 87.2 s / L 1073.2 s — the L seed is the same band as
  M16's 1035 s (run variance, same exe).

## Verdict

No read-side fix needed. The size tier's fan-out is absorbed by the
range-skip → bloom walk, and every gated cell passes at depth 17 with
3.7–5.7× headroom. The M16 trade (−76% merge I/O for a ≤17-segment walk)
is certified as made.

---

# SE2-M18 — fan-out + hot context at tier depth

2026-09-04. M17 certified the point-get / scan rows; the QA matrix also
gates fan-out traversals (F=10/100/1000 ≤ 1/10/50 ms) and the hot context
(≤ 100 µs), whose cells were M14 count-only. This extends the probe with
those rows at the tier steady state — the same shapes, the same pins
(warm rows carry the zero-miss pin; F=1000's working set exceeds the
8 MiB cache and reports its thrash; the hot context asserts no block
reads and cache-served hits).

## Gate verdicts

| gate | S cell (depth 2) | L cell (depth 17) | verdict |
|---|---|---|---|
| fanout F=10 ≤ 1 ms | 24.7 µs | 59.1 µs | PASS — 16.9× headroom at L |
| fanout F=100 ≤ 10 ms | 274.5 µs | 521.0 µs | PASS — 19.2× at L |
| fanout F=1000 ≤ 50 ms | 10.43 ms | 21.50 ms | PASS — 2.3× at L |
| hot context ≤ 100 µs | 39.7 µs | 92.8 µs | PASS — 1.08× at L |

## The new rows, verbatim

DS-PERF-S (3,202,200 rows, seed 88.3 s):

    fanout F=10 · cold · tier ops=    200 wall=     24.7ms p50=111200 p95=190600 p99=216800 | blocks=2400 bytes=39475600 segs=4000 hits=0 misses=0
    fanout F=100 · cold · tier ops=     20 wall=     28.2ms p50=1368800 p95=2076900 p99=2297700 | blocks=2100 bytes=34439200 segs=3720 hits=0 misses=0
    fanout F=1000 · cold · tier ops=      4 wall=     42.7ms p50=9832100 p95=14018500 p99=14018500 | blocks=4072 bytes=66786544 segs=7404 hits=0 misses=0
    fanout F=10 · warm · tier ops=    200 wall=      5.0ms p50= 24700 p95= 26400 p99= 35500 | blocks=0 bytes=0 segs=4000 hits=2400 misses=0
    fanout F=100 · warm · tier ops=     20 wall=      5.7ms p50=274500 p95=292200 p99=443000 | blocks=0 bytes=0 segs=3720 hits=2100 misses=0
    fanout F=1000 · warm · tier ops=      4 wall=     43.1ms p50=10427400 p95=12933900 p99=12933900 | blocks=2724 bytes=44678376 segs=7404 hits=1348 misses=2724
    context · hot · tier     ops=   2000 wall=     84.1ms p50= 39700 p95= 58500 p99=101400 | blocks=0 bytes=0 segs=44000 hits=30000 misses=0

DS-PERF-L (32,002,200 rows, seed 969.1 s):

    fanout F=10 · cold · tier ops=    200 wall=     42.7ms p50=190200 p95=334900 p99=577600 | blocks=5200 bytes=85571600 segs=30000 hits=0 misses=0
    fanout F=100 · cold · tier ops=     20 wall=     41.0ms p50=2000000 p95=2091000 p99=3611700 | blocks=2500 bytes=40983760 segs=29660 hits=0 misses=0
    fanout F=1000 · cold · tier ops=      4 wall=     90.9ms p50=20589000 p95=29924300 p99=29924300 | blocks=4632 bytes=75766636 segs=52632 hits=0 misses=0
    fanout F=10 · warm · tier ops=    200 wall=     12.2ms p50= 59100 p95= 72900 p99=103000 | blocks=0 bytes=0 segs=30000 hits=5200 misses=0
    fanout F=100 · warm · tier ops=     20 wall=     11.1ms p50=521000 p95=730400 p99=956500 | blocks=0 bytes=0 segs=29660 hits=2500 misses=0
    fanout F=1000 · warm · tier ops=      4 wall=     84.7ms p50=21501400 p95=21934200 p99=21934200 | blocks=4432 bytes=72497028 segs=52632 hits=200 misses=4432
    context · hot · tier     ops=   2000 wall=    199.2ms p50= 92800 p95=133300 p99=172500 | blocks=0 bytes=0 segs=330000 hits=88000 misses=0

The M17 rows in both runs reproduce in the same band (cold head 27.0 vs
27.3 µs, absent 1.7 µs, warm 9.9 vs 9.5 µs, hot 3.6 vs 3.5 µs at L).

## What the cells say

- Fan-out scales ~linearly with F, on the depth walk: cold blocks per
  dst get ≈ 1.16 at every F (26/125/1158 per traversal at L), and the
  warm rows stay cache-served through the gate band (F=10/100 zero
  misses, 0 blocks).
- The hot context is the gate the tier tightens most: 39.7 µs at depth 2
  → 92.8 µs at depth 17 — each context is ~44 cache-served block hits
  walked across the full depth (segs/op = 165 at L). It PASSES with
  1.08× headroom, the thinnest margin in the matrix. If the depth or the
  cache:working-set ratio grows, this is the first gate to watch — for
  now the gate is pass/fail and it passes, so no change.
- F=1000 warm ≈ cold (21.5 vs 20.6 ms at L): the ~16 MiB fan working set
  exceeds the 8 MiB cache and thrashes — the M14 finding, reported not
  hidden (misses 4432 at L). Cache sizing for big fan-outs is a
  documented knob, not a hidden one.
- The S type scan spread 440–738 µs across the two S runs is run
  variance (both far under any scan gate; the L scans agree at
  5.76–6.23 ms).

## Verdict

Every QA read gate now has a tier-depth cell, and all seven pass: cold
point, warm point, hot head (M17) + fanout F=10/100/1000 and hot context
(M18). The tier is certified against the complete QA read matrix with no
read-side fix needed; the honest watch item is the hot context's 1.08×
margin at L.
