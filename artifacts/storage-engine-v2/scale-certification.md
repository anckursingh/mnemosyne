# Scale Certification — SE2-M14

Generated only when `SE2M14_NIGHTLY=1|2` (strict opt-in — any other
value panics). Perf numbers are report cells, never asserts; the pins
are answer correctness and cache state.

- Test: `scale_certification`
- Date: 2026-09-02 · Build mode: release
- Machine: windows/x86_64; 8 logical cores; AMD64 Family 23 Model 24 Stepping 1, AuthenticAMD
- Mode: SE2M14_NIGHTLY=2 (DS-PERF-M + DS-PERF-L)

## Datasets

| dataset | KOs | versions/KO | rows | batches | seed wall | fsyncs | disk | RSS peak | segments |
|---|---|---|---|---|---|---|---|---|---|
| DS-PERF-M | 100000 | 5 | 2702200 | 100002 | 77505 ms | 100002 | 157.77 MiB | 469.93 MiB | 1 |
| DS-PERF-L | 1000000 | 10 | 32002200 | 1000002 | 1750373 ms | 1000002 | 2.12 GiB | 16.21 GiB | 1 |

## QA M8 gates — DS-PERF-M

Gate verdicts are machine-relative (machine spec above) — the QA doc's
rule. Verdict on the v2 row; redb is the parity reference, no verdict.

| gate | row | v2 P50 | redb P50 | threshold | verdict |
|---|---|---|---|---|---|
| cold point | head get · cold | 22.0 µs | 3.2 µs | 100 µs | PASS |
| warm point | head get · warm | 4.6 µs | 2.4 µs | 50 µs | PASS |
| hot head | head get · hot | 2.3 µs | — | 20 µs | PASS |
| fanout F=10 | fanout F=10 · warm | 33.8 µs | — | 1000 µs | PASS |
| fanout F=100 | fanout F=100 · warm | 314.9 µs | — | 10000 µs | PASS |
| fanout F=1000 | fanout F=1000 · warm | 11216.4 µs | 2403.8 µs | 50000 µs | PASS |
| hot context | context · hot | 48.1 µs | — | 100 µs | PASS |
| group commit beats Sync where batching is possible | — | PASS (cited) | — | — | SE2-M13 matrix — artifacts/storage-engine-v2/group-commit.md: 8 writers × 25 wait=0 → 37 fsyncs, 0.23 ms/batch vs Sync 0.80 ms/batch |

## Matrix — DS-PERF-M

| workload · state | ops | ops/s | P50 µs | P95 µs | P99 µs | bytes read | blocks | segs/op | hits | misses |
|---|---|---|---|---|---|---|---|---|---|---|
| head get · cold | 400 | 41566 | 22.0 | 43.0 | 59.6 | 6.36 MiB | 410 | 2.69 | 0 | 0 |
| version get · cold | 400 | 34141 | 27.9 | 50.1 | 65.9 | 6.41 MiB | 409 | 2.69 | 0 | 0 |
| history · cold | 400 | 9519 | 96.3 | 177.4 | 205.6 | 24.33 MiB | 1553 | 0.00 | 0 | 0 |
| fanout F=10 · cold | 200 | 8452 | 118.6 | 159.2 | 193.2 | 46.75 MiB | 3000 | 40.00 | 0 | 0 |
| fanout F=100 · cold | 20 | 587 | 1649.7 | 2326.1 | 2417.7 | 33.87 MiB | 2180 | 272.00 | 0 | 0 |
| fanout F=1000 · cold | 4 | 77 | 14974.8 | 16128.8 | 16128.8 | 63.53 MiB | 4092 | 2688.00 | 0 | 0 |
| type scan · cold | 20 | 1552 | 631.8 | 817.5 | 865.8 | 1.88 MiB | 119 | 0.00 | 0 | 0 |
| context · cold | 100 | 1689 | 554.9 | 915.0 | 1510.5 | 29.90 MiB | 1917 | 44.00 | 0 | 0 |
| head get · warm | 400 | 211193 | 4.6 | 6.1 | 9.6 | 0 B | 0 | 2.69 | 410 | 0 |
| version get · warm | 400 | 170140 | 5.7 | 7.7 | 12.4 | 0 B | 0 | 2.69 | 409 | 0 |
| history · warm | 400 | 14416 | 69.6 | 111.1 | 149.8 | 21.03 MiB | 1342 | 0.00 | 211 | 1342 |
| fanout F=10 · warm | 200 | 29292 | 33.8 | 34.3 | 43.5 | 0 B | 0 | 40.00 | 3000 | 0 |
| fanout F=100 · warm | 20 | 2789 | 314.9 | 458.8 | 732.4 | 0 B | 0 | 272.00 | 2180 | 0 |
| fanout F=1000 · warm | 4 | 93 | 11216.4 | 11302.4 | 11302.4 | 42.85 MiB | 2760 | 2688.00 | 1332 | 2760 |
| type scan · warm | 20 | 2152 | 463.4 | 484.5 | 496.0 | 0 B | 0 | 0.00 | 119 | 0 |
| context · warm | 100 | 3682 | 265.0 | 369.7 | 426.9 | 17.31 MiB | 1106 | 44.00 | 811 | 1106 |
| head get · hot | 100000 | 403863 | 2.3 | 2.7 | 4.0 | 0 B | 0 | 4.00 | 100000 | 0 |
| context · hot | 2000 | 20561 | 48.1 | 49.0 | 59.3 | 0 B | 0 | 44.00 | 40000 | 0 |
| mixed 70/20/10 · warm | 400 | 73 | 31.1 | 598.4 | 889.0 | 152.97 MiB | 9754 | 0.74 | 63 | 9754 |

redb parity rows (— = redb exposes no block stats):

| workload · state | ops | ops/s | P50 µs | P95 µs | P99 µs | bytes read | blocks | segs/op | hits | misses |
|---|---|---|---|---|---|---|---|---|---|---|
| head get · cold | 400 | 241999 | 3.2 | 5.3 | 26.9 | — | — | — | — | — |
| head get · warm | 400 | 393314 | 2.4 | 2.9 | 3.1 | — | — | — | — | — |
| fanout F=1000 · cold | 4 | 423 | 2175.5 | 2800.6 | 2800.6 | — | — | — | — | — |
| fanout F=1000 · warm | 4 | 414 | 2403.8 | 2483.1 | 2483.1 | — | — | — | — | — |

## Matrix — DS-PERF-L

| workload · state | ops | ops/s | P50 µs | P95 µs | P99 µs | bytes read | blocks | segs/op | hits | misses |
|---|---|---|---|---|---|---|---|---|---|---|
| head get · cold | 400 | 31264 | 30.1 | 55.1 | 66.5 | 6.59 MiB | 422 | 4.88 | 0 | 0 |
| version get · cold | 400 | 30564 | 31.2 | 50.5 | 72.9 | 6.52 MiB | 416 | 4.88 | 0 | 0 |
| history · cold | 400 | 8916 | 108.3 | 161.1 | 186.7 | 31.72 MiB | 2025 | 0.00 | 0 | 0 |
| fanout F=10 · cold | 200 | 6733 | 142.4 | 156.0 | 242.6 | 50.12 MiB | 3200 | 50.00 | 0 | 0 |
| fanout F=100 · cold | 20 | 493 | 1918.9 | 2210.0 | 3674.3 | 34.40 MiB | 2200 | 500.00 | 0 | 0 |
| fanout F=1000 · cold | 4 | 49 | 19182.0 | 27137.0 | 27137.0 | 65.50 MiB | 4192 | 4885.00 | 0 | 0 |
| type scan · cold | 20 | 146 | 6813.1 | 7766.1 | 9146.4 | 8.20 MiB | 521 | 0.00 | 0 | 0 |
| context · cold | 100 | 1772 | 559.3 | 704.2 | 710.2 | 33.80 MiB | 2158 | 55.00 | 0 | 0 |
| head get · warm | 400 | 118280 | 7.4 | 11.1 | 13.2 | 0 B | 0 | 4.88 | 422 | 0 |
| version get · warm | 400 | 106516 | 9.0 | 11.4 | 17.9 | 0 B | 0 | 4.88 | 416 | 0 |
| history · warm | 400 | 10538 | 88.4 | 151.6 | 211.6 | 25.87 MiB | 1651 | 0.00 | 374 | 1651 |
| fanout F=10 · warm | 200 | 25044 | 39.4 | 40.3 | 51.9 | 0 B | 0 | 50.00 | 3200 | 0 |
| fanout F=100 · warm | 20 | 2672 | 356.9 | 397.4 | 633.3 | 0 B | 0 | 500.00 | 2200 | 0 |
| fanout F=1000 · warm | 4 | 50 | 20503.8 | 20521.0 | 20521.0 | 62.75 MiB | 4016 | 4885.00 | 176 | 4016 |
| type scan · warm | 20 | 201 | 4850.7 | 5390.7 | 5414.9 | 0 B | 0 | 0.00 | 521 | 0 |
| context · warm | 100 | 2169 | 453.1 | 558.4 | 638.6 | 31.51 MiB | 2012 | 55.00 | 146 | 2012 |
| head get · hot | 100000 | 394550 | 2.4 | 2.9 | 3.6 | 0 B | 0 | 5.00 | 100000 | 0 |
| context · hot | 2000 | 19658 | 49.9 | 51.7 | 73.2 | 0 B | 0 | 55.00 | 42000 | 0 |
| mixed 70/20/10 · warm | 400 | 2 | 29.1 | 577.9 | 716.6 | 2.07 GiB | 135039 | 0.75 | 10 | 135039 |

redb parity rows (— = redb exposes no block stats):

| workload · state | ops | ops/s | P50 µs | P95 µs | P99 µs | bytes read | blocks | segs/op | hits | misses |
|---|---|---|---|---|---|---|---|---|---|---|
| head get · cold | 400 | 76234 | 14.2 | 34.7 | 46.8 | — | — | — | — | — |
| head get · warm | 400 | 264183 | 3.8 | 4.7 | 4.9 | — | — | — | — | — |
| fanout F=1000 · cold | 4 | 202 | 3236.7 | 9067.1 | 9067.1 | — | — | — | — | — |
| fanout F=1000 · warm | 4 | 305 | 3044.2 | 3186.2 | 3186.2 | — | — | — | — | — |

## Adoption matrix re-run

`v2_m7_workloads` child with `V2ADOPT_NIGHTLY=1` — the same harness;
`workloads.md` regenerated this run, child exit 0 (asserted). The §26
verdict stays per `adoption-decision.md`; the ≤2×-of-v1 bound stays
out of scope (RAM-vs-disk physics, priced in by the 2026-09-01
verdict) — the comparison that matters is the redb parity above.

## Honest metric mapping

- cold = the block-cache-miss path: a separate Db open with
cache_bytes=0 (the detached cache is pinned by assert) — every get
reads its block from disk. The OS page cache is NOT flushed (needs
admin tooling on Windows) — the same caveat the adoption matrix's
cold rows carry; first touches after the seed still benefit from it.
- warm = an uncounted pre-warm of the same ops, then the timed pass.
Point rows (head, version, fanout F ≤ 100) carry the exact pin
(asserted per row): zero cache misses and zero block reads during
the timed pass — the sample (400 evenly spaced KOs ≈ 6.4 MiB of
blocks) fits the 8 MiB default cache. Scan rows (history, type,
context) carry no cache pin: SE2-M12's k-way scan cursors walk one
block per overlapping segment (~5 at M scale), so a scan working
set is ~5× the cache by construction — warm there means the second
(page-cache) pass, and the hits/misses cells report the thrash
honestly.
- hot = repeated same-key reads; pins (asserted): cache hits ≥ lookups
and zero block reads during the run.
- fanout F=10/100 warm rows carry the warm pin; F=1000 does not — its
~16 MiB head working set exceeds the 8 MiB default cache, so the
honest cells show the thrash (cache sizing for big fan-outs is an
M14 finding, not a hidden knob).
- W8 mixed = one warm row; its write leg lands in the active memtable
and runs last (nothing after it reads the mutated heads).
- redb parity = first pass / second pass on the same open (redb has no
block-cache knob).
- RSS = Windows WorkingSet64 poll on a loader child re-seeding the same
dataset (peak is a lower bound); NOT_SAMPLED elsewhere.
- CPU = seed wall, single-threaded (wall ≈ CPU); fsync count = the
seed's (Sync durability, one fsync per batch); disk = dataset dir at
seed end.
- group commit gate = cited, not re-measured (SE2-M13).
- gate verdicts on DS-PERF-M; DS-PERF-L cells are the scale check.
- regression fixed by this milestone's nightly: v2 restart points on
equal-key runs (the kernel's RMW restatements accumulate (key, seq)
versions in the memtable, and one flush publishes the whole run).
The writer now skips restarts on equal keys; intervals over a
multi-version run exceed RESTART_INTERVAL, so a head lookup inside
a long run decodes it (the honest version-lookup cost — hot-head
rows report it).
