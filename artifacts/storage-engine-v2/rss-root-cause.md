# Seed RSS amplification — root cause

Evidence-led follow-up to the SE2-M14 finding (DS-PERF-L seed peak
**16.21 GiB** vs a **2.12 GiB** dataset; adoption loader 496.93 → 1.35 GiB
vs the 09-01 nightly). No code changed in this pass — this is the root
cause with citations, reconciliation, and fix options. Options 1-4 were
then implemented and re-measured in SE2-M15 — the outcome section at the
end supersedes the fix-options framing.

## Verdict

The peak is not the memtable and not the WAL. It is
`SegmentWriter::publish()` (`segment.rs:126-323`), which assembles an
entire segment in RAM while holding **five simultaneous copies of the
data**, multiplied by SE2-M10's L0 trigger (`db.rs:373-389`), which runs a
KeepAll merge of **all** segments — and therefore publish of the whole
accumulated dataset — every time four flushes accumulate. At L the final
such merge publishes the full 2.12 GiB through the five-copy pipeline.

## The five-copy stack inside publish()

All alive at once (nothing is dropped until `publish_atomic` returns):

| # | buffer | site | size at L |
|---|---|---|---|
| 1 | `writer.entries: Vec<SegmentEntry>` — owned keys+values (merge pushes decoded entries, compaction.rs:116; flush clones from the memtable, db.rs:634-635) | segment.rs:167 | 32.0M × ~125 B ≈ **4.0 GiB** |
| 2 | `let mut entries = self.entries.clone();` — deep clone for the sort (publish takes `&self`, so the writer also *keeps* its copy afterwards) | segment.rs:135 | **4.0 GiB** |
| 3 | `blocks: Vec<Pending>` — every block's encoded payload | segment.rs:166 | **2.1 GiB** |
| 4 | `data_blocks` — each block re-wrapped in a fresh Vec (`p`, segment.rs:251) + block header | segment.rs:240 | **2.1 GiB** |
| 5 | `out` — the assembled whole-file buffer | segment.rs:224, 313-317 | **2.1 GiB** |

Plus small terms (bloom 40 MB, skeleton headers, readers' index+bloom
≈ 50 MB, active memtable+immutables). Predicted peak ≈ **14.4 GiB**,
measured **16.21 GiB** — the remainder is allocator overhead on 32M
entries and RSS sampling granularity. Per-entry real RAM is estimated
(Vec headers + Windows allocator slack); the estimate is ~125 B/entry
against ~71 B/entry accounted by the memtable.

After publish returns, `compact()` reads the just-published segment back
whole for the manifest checksum (`std::fs::read`, db.rs:740) — another
transient 2.1 GiB, and another reason the write path stays elevated.

## Reconciliation across all three measurements

| measurement | dataset | compactions | predicted peak | measured |
|---|---|---|---|---|
| DS-PERF-M (2.7M rows) | ~192 MB | none (3 flushes < trigger 4) | ~0.5-0.7 GiB | **469.93 MiB** ✓ |
| adoption 100K, 09-01 (pre-M10) | ~348 MB | none — compaction did not exist | ~0.5 GiB | **496.93 MiB** ✓ |
| adoption 100K, 09-03 (post-M10) | ~348 MB | yes — crosses 4 flushes, merges ≈ 256-348 MB | ~1.5-1.6 GiB | **1.35 GiB** ✓ |
| DS-PERF-L (32.0M rows) | 2.12 GiB | 33 flushes; merge sizes 256 MB → 2.12 GiB, quadratic | ~14.4 GiB | **16.21 GiB** ✓ |

The 496.93 → 1.35 GiB change between the 09-01 adoption nightly and the
M14 re-run is the same mechanism, not a separate regression: M10 added
the trigger; the 100K seed (7 segments pre-M10) now merges, and each
merge pays the five-copy pipeline. The M row confirms the flush-only
baseline (no trigger crossed → ~0.5 GiB).

## The 29-minute seed wall (same finding, separate axis)

Sync durability = one fsync per batch = 1,000,002 fsyncs, plus the
quadratic KeepAll policy: each trigger merges **all** L0+L1, so the bulk
seed re-reads and re-writes the entire accumulated dataset at every 4th
flush — Σ merged ≈ 34 GiB written + re-read at L. The M10 policy assumes
incremental arrival with a bounded dataset; a monotonically growing bulk
seed makes it quadratic. I/O, not memory, is the wall here.

## Fix options (not implemented — next milestone)

Ordered by savings vs. diff size:

1. **Publish in place.** `publish(&mut self)` + `std::mem::take(&mut
   self.entries)` instead of the clone; all callers (flush db.rs:640,
   merge compaction.rs:146/151, archive) own local `mut` writers, so
   this is a signature change. Kills copy #2 (−4.0 GiB at L, ≈ −28%) and
   frees the writer's dead entries after publish. Byte-identical output;
   the existing format goldens are the correctness net.
2. **Stream publish to the temp file.** Blocks are already fully encoded
   when they complete; restart offsets and bloom bits (m = 10·n, n known
   from `entries.len()`) are incremental. Write header, then blocks as
   they fill, then index, bloom, footer. Kills copies #3-#5 (−6.3 GiB at
   L) — publish becomes O(largest block), the real fix. Byte-identical
   layout; moderate diff inside publish/publish_atomic.
3. **Drop the read-back checksum in `compact()` (db.rs:740).** The
   segment's own footer already carries the skeleton checksum
   (segment.rs:293-321); compute the manifest record checksum from the
   same skeleton publish has in hand instead of re-reading 2.1 GiB.
4. **Move, don't clone, at flush (db.rs:634-635).** Draining the
   memtable by `into_iter` moves keys/values into the writer instead of
   cloning — removes the flush-path double copy (visible in the M row's
   baseline).
5. **Compaction policy for bulk loads.** KeepAll merge-everything is
   quadratic on a growing seed. Candidates: size-tiered trigger (merge
   L0 only when its size is a material fraction of L1), or expose the
   existing `l0_compact_trigger = 0` knob to the seed harness. Policy
   decision, not a one-liner — belongs with the next milestone's scope.

Options 1-4 together take the L peak from ~16 GiB to the merge
writer's entries — one materialized copy, ≈ 4 GiB at L — plus one block
plus index/bloom, so ≈ 4-4.5 GiB at L, not < 1 GiB: the header's
variable-length key range forces the two-pass entries design, so the
merge writer cannot fully stream. < 1 GiB would need a format change or
a streaming merge (out of scope).

## SE2-M15 — fixes shipped, measured outcome (2026-09-03)

Options 1-4 implemented: `publish(&mut self)` + `std::mem::take` (the
sort clone is gone), streaming two-pass publish (dry pass for block
boundaries; blocks written and dropped as they complete — the
whole-file buffer and the per-block copies are gone), publish returns
`(file_size, checksum8)` from a streaming `sha2::Sha256` so flush and
compact drop the whole-file read-backs, and `Memtable::into_entries`
moves keys/values at flush. Byte-identity with the pre-rewrite writer
is pinned by machine-captured hex fixtures — both the v2 and the v1
multi-block formats, the latter captured from the original writer at
00e2270 in a worktree and cmp-verified (`segment_golden.rs`).

Re-measured on the same machine (windows/x86_64, 8 cores, 30 GiB RAM),
polling `Get-Process -Id` WorkingSet64 at 500 ms — the same method
`measure_rss` uses, so the pre- and post-fix cells are comparable:

| cell | pre-fix (M14 nightly) | post-fix (M15) |
|---|---|---|
| DS-PERF-L peak working set | 16.21 GiB | **5.10 GiB** (commit 5.48 GiB) |
| DS-PERF-L seed wall | 1,750.4 s | **1,234.2 s** |

Peak **−3.2×**; the wall fell **29.5%** — the five-copy pipeline and
the whole-file read-backs were wall-clock too, not just RSS.

**Where the peak sits now.** The post-fix peak is the merge writer's
`entries` at the final KeepAll merge — one materialized copy, ≈ 4.5 GiB
of 32M `SegmentEntry`s, plus block/index/bloom scratch ≈ 0.5 GiB. Commit
tracks WS nearly 1:1 (5.48 vs 5.10 GiB), so file-cache pages contribute
nothing material to the working set at this scale — the peak is the
engine's own allocations. The "≈ 4-4.5 GiB at L" prediction in the
options paragraph was conservative by ~15% but correct in mechanism.

**The pre-fix 16.21 GiB cell stands.** One intermediate run in this
follow-up used a process-NAME-matched sampler and reproduced 16.21 GiB,
but its trace is impossible for the loader (instant multi-GiB working-set
drops, a constant ~0.5 GiB baseline for minutes at a time, a 50% longer
wall) — the name match aliased a concurrent same-named process. The same
binary PID-sampled produces the clean staircase to 5.10 GiB above. PID
sampling is the certified method; the M14 16.21 GiB cell remains the
pre-fix baseline (it was PID-sampled by `measure_rss` on the five-copy
code, whose predicted peak it matched). `scale-certification.md`
regenerates at the next nightly and will pick up the 5.10 GiB cell; this
doc is the interim evidence.

**Remaining headroom.** The residual ~5 GiB is option-5 territory: the
quadratic KeepAll policy still re-reads and re-writes the whole dataset
every fourth flush (Σ ≈ 34 GiB at L), and the merge writer still
materializes the full output once — the format's variable-length header
forces the two-pass entries design. Size-tiered triggering or a streaming
merge is the next lever; policy or format change, out of M15's scope.

**New mid row.** `DS-PERF-S` (100K KOs × 10 versions, 3.2M rows) added to
the loader as the trigger-crossing scale: 86 s wall, peak 0.48 GiB WS
(0.58 GiB commit) at its single 4-L0 merge — the same merge-only shape at
mid scale, between the flush-only M row (469.93 MiB) and L.

## SE2-M16 — size-tiered compaction, measured outcome (2026-09-03)

Option 5 implemented: `maybe_compact` gains a size tier above the M10
count floor — the merge fires only when L0 holds at least
`l0_compact_trigger` segments AND L0's bytes are at least L1's bytes
divided by `l0_tier_ratio` (default 1; L1 empty always merges; 0
restores count-only). A monotonically growing bulk seed now merges at
L0 counts 4, 8, 16, 32… instead of every 4th flush — merge write
amplification drops from quadratic to ~O(n log n). Three unit tests
(`db_tiered_compact.rs`) pin the schedule: first merge at 4 with L1
empty, skip while L0 < L1, ratio 2 fires earlier, ratio 0 and trigger
0 preserve the M10 modes.

Re-measured on the same machine (windows/x86_64, 8 cores, 30 GiB RAM),
PID-sampled WorkingSet64 at 500 ms — the same method as the M15 cells:

| cell | M15 | M16 |
|---|---|---|
| DS-PERF-L seed wall | 1,234.2 s | **1,035 s** (−16.1%) |
| DS-PERF-L peak working set | 5.10 GiB | **4.02 GiB** (−21.2%) |

**The tier schedule at L** (56 flushes, 37.4M rows, 680,064 rows and
48.36 MB per flush): merges at L0 counts 4 (L1 empty), 4 again (L1 was
exactly 4 flushes, so the count floor immediately satisfies the ratio —
193.42 MB ≥ 193.20 MB), 9 (L1 was 8 flushes; the ~47-KB-per-flush
output shrink from header collapse means 9 flushes, not 8, to match it
— 435.2 MB ≥ 386.9 MB), 17 (L1 was 17 flushes, and here the shrink
and the growth cancel — it fires by 0.8 MB: 822.1 ≥ 821.3). The next
merge would need L0 ≥ 1.64 GB ≈ 34 flushes; the seed ended with 16 L0
segments ≈ 776 MB, so the gate held and the pointless 5th merge never
happened. Merge walls 5.16 / 9.76 / 20.59 / 44.77 s (80.3 s total);
outputs 193.2 / 386.9 / 821.3 / 1640.1 MB — Σ 3.04 GB of merge I/O vs
KeepAll's 12.77 GB (−76%).

**The peak moved with the merges.** Merge peaks at 2.72M / 5.44M /
11.56M / 23.12M entries measured 0.5 / 1.0 / 2.1 / 4.0 GiB WS — the
1.27-1.3× entries footprint M9's bounded decode promises, with clean
release to the ~80 MB baseline after each. M15's 5.10 GiB peak was the
FINAL KeepAll merge over all 32M rows; the tier deferred that merge
past seed end, so the M16 peak is the 23.1M-entry merge at L0 = 17.

**Fast/slow cycles reconciled.** The 10-cluster staircase of the
pre-M15 binary (run-1: 16.21 GiB / 1,952 s) was retired as M14-era
code — exe forensics showed run-1 executed the 00:19 build that
predates the M15 fix, and its signature ~78 s flush cycles once L0 ≥
5-6 DO NOT recur on the M16 code: cycle times stay 16-27 s at every L0
depth, writers never block, and the gate fires mid-write. The residual
slow windows (26-38 s, ~8 of 56 cycles, scattered) decompose to
periodic fsync stalls — 13-14 ms on ~a quarter of batches for 30-40 s
windows, plus occasional 3.7-4.5 s publishes — OS-level periodic
interference, engine-independent, already present in the M15 wall's
shape. The per-component timing instrumentation was diagnostic-only and
reverted before commit.

**Read-path consequence.** The steady state is no longer one L1 + the
active L0: L0 piles to ≤17 segments before the tier fires, and a get()
considers up to 18 segments vs ≤2 under count-only. That fan-out is
the design trade for −76% merge I/O; the read-side cost at this depth
was unmeasured here and is now certified in M17 — see tiered-read.md:
every QA read gate passes at depth 17 with 3.7–5.7× headroom. The
hot-head / read-path gates pin their own
count-only configs so their cells stand. DS-PERF-S (7 flushes) is
unchanged by the tier — its single merge is the L1-empty one. Remaining
headroom is the merge writer's full-output materialization (the
variable-length header's two-pass shape), out of M16's scope.

## Caveats

- Per-entry real RAM is estimated; the 14.4 vs 16.21 GiB gap is
  allocator overhead, not an unaccounted buffer.
- No re-run was done for this report: the mechanism is read from the
  code, and the three existing measurements bracket it (no-compaction
  baseline at M and pre-M10 100K, trigger-active at post-M10 100K and L).
