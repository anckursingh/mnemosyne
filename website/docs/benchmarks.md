---
title: Benchmarks
description: Certified cross-engine benchmark matrix (memory, redb, aikoql v1, aikoql-v2)
---

# Benchmarks

All numbers below are measured in this repository — nothing is estimated.
Source: the certified matrix `artifacts/storage-engine-v2/workloads.md`
(2026-09-06, release build), the M40 checkpoint probe
(`directory-checkpoint.md`, 2026-09-07), and the v1-adoption M7 matrix.

## Methodology

- **Dataset:** 100,000 knowledge objects / 20,000 ops per workload cell
- **Regime:** page-cache-warm, **p50** latency unless noted
- **Column order:** memory (no-op engine) · redb · aikoql v1 · aikoql-v2
- **Harness:** `kse_m7_v2_workloads` (v2) and the M7 suite (v1, redb)

## Workload Matrix (W1–W8)

| workload | memory | redb | aikoql (v1) | aikoql-v2 |
|---|---|---|---|---|
| W1 KO get | 6 µs | 18 µs | 7 µs | 34 µs |
| W2 head get | 6 µs | 9 µs | 6 µs | 33 µs |
| W3 version lookup | 9 µs | 12 µs | 9 µs | 43 µs |
| W3 history | 32 µs | 36 µs | 32 µs | 69 µs |
| W4 relationships F=10 | 128 µs | 156 µs | 128 µs | 193 µs |
| W4 relationships F=100 | 408 µs | 669 µs | 410 µs | 988 µs |
| W4 relationships F=1000 | 3,864 µs | 6,617 µs | 3,792 µs | 10,581 µs |
| W5 type scan | 5,541 µs | 7,766 µs | 5,459 µs | 27,158 µs |
| W7 context compilation | 56 µs | 112 µs | 57 µs | 223 µs |
| W8 mixed 70/20/10 | 6 µs · p99 42 µs | 13 µs · **p99 31,994 µs** | 9 µs · **p99 31,159 µs** | 45 µs · **p99 978 µs** |
| W6 ingestion | 21 µs (48,770 ops/s) | 2,023 µs (494 ops/s) | 724 µs (1,380 ops/s) | 728 µs (1,373 ops/s) |

**W8 — the write-mixed workload the application actually lives on:**
aikoql-v2's p99 tail is **978 µs** — a **33× better tail** than redb
(31,994 µs) and v1 (31,159 µs) — and **8,231 ops/s vs 1,373 / 1,476**
(**6.0× throughput**). Structural, not noise: group commit does one fsync per
batch where both durable alternatives pay a full commit per write.

## Resources at 100K Objects

| backend | seed wall | peak RSS | disk |
|---|---|---|---|
| aikoql-v2 | 221.6 s | **428.05 MiB** | **347.99 MiB** |
| aikoql v1 | 230.2 s | 611.22 MiB | 435.44 MiB |
| redb | 838.7 s | 513.94 MiB | 1.00 GiB |

v2 seeds 2.8× faster than redb with **30% less RAM than v1** and **65% less
disk than redb** — the bounded-RAM / disk-economy profile.

## Bounded Recovery (SE2-M40 checkpoint, 600K updates)

| open path | files | wall | bytes |
|---|---|---|---|
| checkpoint + deltas-after | 7 | **75.0 ms** | **1.73 MB** |
| full-history replay | 303 | 347.6 ms | 41.1 MB |

4.6× faster open, 23.7× less metadata, and the checkpoint arm's open stays
flat at live-state size while full replay grows with every published log.

## The Honest Deficit — warm point reads

W1 KO get: v2 34 µs vs v1 7 µs / redb 18 µs (**6.00× v1, 5.48× on W2** — inside
the amended ≤8× design gate with 1.2–1.5× headroom). Root cause is measured,
not guessed: one 64 KiB block fetch + soft-SHA-256 per cold get
(~18.7 µs of the 33.5 µs engine get-wall), against redb's 4 KiB pages and
v1's zero disk. It is the bounded-RAM trade, priced precisely — and it is
tunable: block target (64 KiB → 4–16 KiB, priced ~4–8×), cache sizing, and
the placement-direct hot-read path already cut O(run) scans to O(16)-entry
decode windows (2,800× on the hot key). Deployments that are pure read-hot
and RAM-affordant can stay on `storage.backend = aikoql`.

## Gate Status

Conformance and gates 1–3 PASS with committed evidence; gate 5 (bounded-RAM
design gate) **PASS at 6.00× / 5.48×** under the amended ≤8× bound — the
ratified 2026-09-07 adoption decision (`adoption-decision.md` §Ratification,
ADR `docs/STORAGE-ENGINE-ARCHITECTURE-DECISION.md`).
