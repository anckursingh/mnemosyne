# V2 Adoption Decision (design §26 + MRFC-KSE-001 V2-Adopt)

Scale: adoption-scale (`V2ADOPT_NIGHTLY=1`, release, 100K KOs / 10K deep × 10 versions / 20K ops, ~56 min) + smoke (2K/2K/2K) for the earlier rows. All correctness asserts real; the perf verdict below is the adoption-scale one.

## Gate evidence

| gate (§26) | result | evidence |
|---|---|---|
| conformance: six KSE-1 asserts × 4 backends | PASS | `kse20_backend_conformance_v2` — memory/redb/aikoql/aikoql-v2 all 6/6, reopen probes served identically on the three durable backends (artifacts/storage-engine-v2/conformance.md); granular suite tests/engine.rs green |
| 1. recovery bounded by the active WAL | PASS | SE2-M3 — artifacts/storage-engine-v2/recovery-independence.md: replay only the active WAL, orphan/missing-segment policies; real-kill recovery suites M3/M4/M6 green |
| 2. dataset larger than RAM remains queryable | PASS | `v2_gate2_3_dataset_larger_than_ram` — ~820 KB dataset under a 64 KiB memtable + zero cache: served from ≥2 on-disk segments, full scan byte-exact, spot gets byte-exact, identical after reopen |
| 3. memory limits configurable | PASS | the same probe pins both knobs: `memtable_bytes=64 KiB` forced the flushes; `cache_bytes=0` detaches the cache (silent stats), a 4 KiB cap is consulted (misses) yet holds nothing (oversize block never retained) |
| 4. group commit improves concurrent throughput without weakening Sync | NOT_EVIDENCED | weakening Sync: PASS by the SE2-M6 suite (Sync baseline reproduced byte-exactly, apply-before-ack, every acked batch present under 8-writer load — asserted). throughput gain: NOT evidenced — the shipped `SE2M6_NIGHTLY=1` matrix (release, artifacts/storage-engine-v2/group-commit.md) shows GC 1-writer ≈ Sync (233 vs 229 ms) and 200/200 fsyncs on the 8-writer row: at 25 batches/writer with 5 ms windows the groups never coalesce, so the one-fsync-per-group win has nothing to show. The mechanism (one fsync per group, ack-after-apply) is asserted green; only the throughput half lacks a coalescing matrix. |
| 5. KO lookup competitive with the MVP baseline (v1) | **FAIL** | adoption-scale: W1 p50 399 µs vs v1 6 µs (**72.53×**), W2 p50 391 µs vs 6 µs (**69.79×**) — bound ≤ 2×. Smoke was favorable (1.15×/1.49×) only because the 2K dataset sat inside cache + page cache. |

## What the adoption-scale run showed (100K, release, `V2ADOPT_NIGHTLY=1`)

Point reads collapse the moment the working set outgrows the block cache:

| workload | v2 p50 | v1 p50 | ratio | redb p50 | memory p50 |
|---|---|---|---|---|---|
| KO get (W1) | 399 µs | 6 µs | 72.5× | 19 µs | 6 µs |
| head get (W2) | 391 µs | 6 µs | 69.8× | 8 µs | 7 µs |
| version lookup (W3) | 466 µs | 9 µs | 51.8× | 12 µs | 12 µs |
| history (W3) | 504 µs | 34 µs | 14.8× | 37 µs | 41 µs |
| relationship F=10 (W4) | 3725 µs | 117 µs | 31.8× | 155 µs | 121 µs |
| relationship F=100 (W4) | 21268 µs | 416 µs | 51.1× | 676 µs | 516 µs |
| relationship F=1000 (W4) | 215791 µs | 4160 µs | 51.9× | 6678 µs | 4717 µs |
| type scan (W5) | 328465 µs | 5503 µs | 59.7× | 8194 µs | 6531 µs |
| context compilation (W7) | 5421 µs | 53 µs | 102.3× | 101 µs | 68 µs |
| mixed 70/20/10 (W8) | 838 µs | 9 µs | 93.1× | 15 µs | 7 µs |
| ingestion (W6) | 950 ops/s | 1328 ops/s | 0.7× (1.4× slower) | 334 ops/s | 36077 ops/s |

Every read workload is 15–102× slower than v1. W1 is also ~21× slower than redb. Only ingestion is competitive (within 1.4× of v1, 2.8× faster than redb).

Resources at 100K (the countervailing wins, all measured):

| backend | CPU (seed wall) | RSS (peak, loader child) | disk |
|---|---|---|---|
| redb | 838661 ms | 513.94 MiB | 1.00 GiB |
| aikoql (v1) | 210822 ms | 613.56 MiB | 435.44 MiB |
| aikoql-v2 | 294863 ms | 496.93 MiB | 354.36 MiB |

## Diagnosis — why the collapse

The dataset anatomy pins it: at seed end the 100K dataset is **7 segments** (37.0–57.2 MB, ≈304 MB total) + a 54.25 MB active WAL (the live memtable), i.e. a ~354 MB working set against the default **8 MiB block cache** — cache:working-set ≈ 1:44. The harness samples uniformly with replacement (20K ops over 100K keys ⇒ each key touched ~0.2×), so essentially every point get is cold:

- **One positioned 64 KiB block read per get dominates.** A get walks the memtable (RAM) then bloom-probes up to 7 segments (RAM, cheap) and does exactly one on-disk block read at the hit — ~400 µs p50 on this machine. Smoke looked fine (42 µs) because the 2K dataset was one ~40 MB segment + memtable: after first touch it lived in cache + OS page cache, and the p50 was RAM.
- **v1 pays zero disk at query time by design** — the RAM mirror IS the store (KSE-5/KSE-18 architecture). Its 6 µs p50 is memory. The ≤2× bound was always a RAM-vs-disk bound, and a uniformly-sampled cold working set is the worst case for any disk-backed design.
- **The 64 KiB block target is the direct multiplier.** A head row is ~1.4 KB; each get fetches and checksums 64 KiB to read it. redb's 4 KiB B-tree pages (upper levels staying in page cache) make its cold reads ~20× cheaper than v2's. This also explains W5: 1.44 MB of logical type rows scattered across 7 segments cost ~161 cold block reads per scan = 328 ms.
- W4/W7 trace to the same root: they are N gets per op, so the per-op cost is ~N × one cold block read.

## Feasibility — what v2 is, and is not, viable for

Viable today, at measured 100K numbers:

- **Bounded-memory deployment profile** — lowest RSS of the durable backends (496.93 MiB, less than v1's 613.56) with working knobs (gates 2+3 PASS: memtable and cache both pin the bound, cache detachable).
- **Smallest disk footprint** — 354.36 MiB vs v1's 435.44 and redb's 1.00 GiB, with no compaction ever run.
- **Bounded recovery** — open cost ≈ active WAL regardless of segment count (gate 1, recovery-independence.md).
- **Ingestion and mixed load** — 950 ops/s seed (2.8× redb), W8 mixed 910 ops/s (v1 1377; v2 p50 838 µs is update-fsync-bound, not read-bound).

Not viable as shipped: **the default random-read engine**. Gate 5 fails 70× over the bound, and the failure is architectural, not a tuning slip — the design's bounded-RAM trade buys latency with disk, and the ≤2×-of-RAM-mirror bar is unreachable at scale without giving the trade back.

Remediation paths, each with its honest ceiling:

- **Runtime compaction (L0→L1)** — 7 segments → 1 removes the multi-segment walk and most of the scan amplification (W5, history, versions improve sharply); W1 improves only ~2–4× because one cold block read per get remains. Not enough for ≤2×.
- **Smaller block target (4 KiB)** — shrinks the per-get fetch by the row:block ratio, ~4–8× on point reads; W1 lands ~50–100 µs. Still 10×+ over v1.
- **Cache sized to the working set** — closes the gap completely, by converging on v1's RAM-mirror design. Correct, and the admission that the bounded-RAM win and the ≤2×-read gate pull in opposite directions at this dataset size.
- **Profile-specific deployment** — the actual answer: keep v1 as the default (query-heavy, RAM-affordant) and offer v2 (AIKOQL_BACKEND=aikoql-v2) for bounded-memory/bounded-recovery/disk-constrained profiles. A follow-up milestone could pair compaction + smaller blocks and re-measure W1; even optimistic arithmetic (7× from blocks, 3× from compaction ≈ 20×) does not reach the 36× needed for the bound, so a re-run is only worth scheduling if the goal is shrinking the gap for the profile case, not ADOPT.

## SE2-M19 re-matrix — 2026-09-04, post-M18 code (page-cache-warm)

The adoption-scale matrix was re-run on the post-M18 engine (M15 publish pipeline, M16 tiered compaction, M17/M18 tiered read path; the harness now stamps the run's actual date and smoke runs no longer clobber workloads.md). Fresh cells, same seed:

| workload | v2 p50 | v1 p50 | redb p50 | v2 ops/s |
|---|---|---|---|---|
| KO get (W1) | 37 µs | 6 µs | 16 µs | 25955 |
| head get (W2) | 36 µs | 5 µs | 8 µs | 27404 |
| version lookup (W3) | 45 µs | 8 µs | 12 µs | 20986 |
| history (W3) | 69 µs | 31 µs | 40 µs | 14045 |
| relationship F=10 (W4) | 203 µs | 119 µs | 186 µs | 4225 |
| relationship F=100 (W4) | 1202 µs | 401 µs | 790 µs | 838 |
| relationship F=1000 (W4) | 10972 µs | 3905 µs | 9941 µs | 88 |
| type scan (W5) | 22627 µs | 5232 µs | 7992 µs | 24 |
| context compilation (W7) | 252 µs | 56 µs | 112 µs | 3882 |
| mixed 70/20/10 (W8) | 43 µs | 8 µs | 12 µs | 7917 |
| ingestion (W6) | 792 µs commit | 822 µs commit | 2358 µs | 1263 vs 1216 |

**Gate 5 still FAILs — now in both page-cache regimes.** This run is page-cache-warm (the harness seeds, then immediately reads: the ~350 MB dataset sits in the OS page cache, so W1's 37 µs ≈ M17's tier-depth raw get of 27 µs plus kernel overhead — tiered-read.md). The 09-01 run's 399 µs was page-cache-cold (the nightly child read behind the parent's freshly seeded ~6 GB, so every point get paid a physical 64 KiB block read). The harness's gate cell: **W1 6.53× v1, W2 6.81× v1** (a second warm run earlier the same day gave 7.09×/8.52× — run variance inside the same regime). The ≤2× bound is missed ~3.3× even in the best-case warm regime and ~35× in the cold regime. Nothing in M14–M18 changed that shape: the warm number is the honest best case, and it still does not reach the bound.

The countervailing wins did move, from M15/M16:

| backend | seed wall | RSS | disk |
|---|---|---|---|
| aikoql-v2 (09-01) | 294863 ms | 496.93 MiB | 354.36 MiB |
| aikoql-v2 (09-04) | 221646 ms | 428.05 MiB | 347.99 MiB |
| aikoql v1 (09-04) | 230219 ms | 611.22 MiB | 435.44 MiB |

v2 seeding is now ≈ v1's (221.6 s vs 230.2 s, −3.7%), RSS is 30% below v1's, and disk stays the smallest. The bounded-memory/bounded-recovery profile case is stronger than at the 09-01 verdict — the read-gate case is not. The remediation arithmetic stands unchanged: blocks + compaction would shrink the gap for the profile case, not reach ≤2× of a RAM mirror.

## Verdict

**VERDICT: NOT ADOPT. v2 stays OPT-IN (default remains aikoql v1).** Conformance and gates 1–3 PASS with committed evidence — v2 is a qualified bounded-memory / bounded-recovery profile engine, and the SE2-M19 re-matrix strengthens that profile (seed ≈ v1, RSS 30% below v1). Gate 5 FAILs the ≤2× bound in both measured page-cache regimes — 6.53×/6.81× warm (09-04, post-M18) and 72.53×/69.79× cold (09-01) — which per §26 disqualifies it as the production default. Gate 4's throughput half remains NOT_EVIDENCED (mechanism asserted green in SE2-M6; no coalescing matrix exists yet).

## SE2-M22 amendment (2026-09-05)

**Gate 5 re-bound: ≤2× → ≤8× v1 (user decision, after the M22 evidence).** The ≤2× bar was RAM-vs-RAM — v1's mirror pays zero disk by design — and this document's own remediation section (09-01) already records that no bounded-RAM path reaches it ("the ≤2×-of-RAM-mirror bar is unreachable at scale without giving the trade back"). M22 shipped the miss-path levers (no-copy first-touch checksum8, one bloom hash pair per get shared across segment probes) and re-probed: W1 P50 39.5 → 33.5 µs, of which block io 18.7 µs (62% of the engine get_wall) is the bounded-RAM floor — positional read + soft sha256 ~10 µs/16 KiB (fast backends surveyed and rejected on MSVC: 0.10 `compress` is a stub, `asm` ships GAS `.S` sources MSVC cannot assemble, 0.11 `x86-sha` crashes non-SHA CPUs). Expected ratio 5.6–6.7×, inside ≤8× with 1.2–1.5× headroom; a real regression (≈13×+) still fails the gate. Enforced at `GATE5_SLOWDOWN_BOUND` in `kse_m7_v2_workloads.rs`.

The NOT ADOPT verdict above stands as the record through M19. The re-bound makes gate 5 a bounded-RAM design gate rather than an adoption gate: if the next certified matrix passes all gates under the amended bound, the adoption question re-opens for a user decision — the re-bound does not by itself flip the verdict.
