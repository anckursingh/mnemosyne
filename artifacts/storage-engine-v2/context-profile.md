# Context Compilation Profile (W7) — SE2-M27

Generated only when `SE2M27_NIGHTLY=1` (strict opt-in). Perf numbers are report cells, never asserts.

- Test: `v2_m27_context_profile`
- Build mode: release
- Machine: windows/x86_64; 8 logical cores; AMD64 Family 23 Model 24 Stepping 1, AuthenticAMD
- Date: 2026-09-05
- Dataset: one v2 database, 100000 KOs / 10000 deep × 10 versions (SEED 0x270000); one W7 op = `k.get(id)` (2 engine point gets — head + ~1.4 KiB version row — + decode + authz) + `outbound_edges(id)` (one `relo/` prefix scan, no gets, no authz) + 10 × `k.get(target)` + `history(id)` (one `ko/` prefix scan + decode + authz per version, no gets)
- Sample (capture-pinned): the harness's exact W7 draw sequence — 5000 draws of `Xs(SEED ^ 0x27).below(100000)`, uniform with replacement (a stride sample would ride the ring's block locality and understate the miss rate); hubs included when drawn (100/1000 edges, 11 versions); 468 deep draws × 10 versions + 4532 shallow × 2 (create + ring update)
- Matrix reference (09-05 workloads.md, warm): W7 v2 222 µs vs v1 57 µs (3.9× — inside the amended gate-5 bound ≤8×); L1/L6 below run the same op on this machine in two cache regimes (fresh vs post-W1-W5-thrash)
- Decision-tree thresholds (fixed before the run): scan share < 15% → no scan-shape work (the scans are already single prefix scans); kernel residual > 30% → kernel-side profiling follow-up; batch ratio ≥ 0.90 → parity (M25's falsification holds at W7's mix → no new batch primitives); < 0.80 → reopen the batch question; 0.80–0.90 → re-run before deciding (built in — two batch-vs-loop pairs per run); history share > 30% → the versions path gets its own follow-up

| leg | p50 | p95 | p99 | max | throughput |
|---|---|---|---|---|---|
| W7 kernel op — matrix draw replay | 219 µs | 451 µs | 730 µs | 4078 µs | 3895 ops/s (mean 257 µs) |
| engine scans — relo/<id>/ + ko/<id> | 54 µs | 108 µs | 150 µs | 549 µs | 16520 ops/s (mean 61 µs) |
| kernel gets — id + per-target loop | 150 µs | 238 µs | 344 µs | 993 µs | 6237 ops/s (mean 160 µs) |
| kernel get_many — [id] + targets in one batch | 131 µs | 268 µs | 380 µs | 1675 µs | 6714 ops/s (mean 149 µs) |
| kernel history — ko/ scan + per-version decode | 32 µs | 90 µs | 139 µs | 447 µs | 24334 ops/s (mean 41 µs) |
| kernel gets — id + per-target loop (rerun) | 149 µs | 240 µs | 332 µs | 2329 µs | 6259 ops/s (mean 160 µs) |
| kernel get_many — [id] + targets in one batch (rerun) | 126 µs | 251 µs | 532 µs | 42703 µs | 5991 ops/s (mean 167 µs) |
| W7 kernel op — matrix regime (post-thrash) | 207 µs | 342 µs | 461 µs | 2208 µs | 4418 ops/s (mean 226 µs) |

| leg | lookups/op | cache hits/op | cache misses/op | blocks read/op | bytes read/op | entries decoded/op | get_wall/op | segs/op |
|---|---|---|---|---|---|---|---|---|
| W7 kernel op — matrix draw replay | 22 | 25.8 | 3.7 | 3.7 | 60751 | 262 | 141 µs | 115.9 |
| engine scans — relo/<id>/ + ko/<id> | 0 | 5.8 | 2.2 | 2.2 | 36608 | 87 | 0 µs | 0.0 |
| kernel gets — id + per-target loop | 22 | 19.2 | 2.2 | 2.2 | 35440 | 175 | 126 µs | 115.9 |
| kernel get_many — [id] + targets in one batch | 22 | 0.7 | 2.2 | 2.2 | 35233 | 175 | 112 µs | 14.0 |
| kernel history — ko/ scan + per-version decode | 0 | 2.8 | 1.2 | 1.2 | 19637 | 45 | 0 µs | 0.0 |
| kernel gets — id + per-target loop (rerun) | 22 | 19.3 | 2.2 | 2.2 | 35424 | 175 | 125 µs | 115.9 |
| kernel get_many — [id] + targets in one batch (rerun) | 22 | 0.7 | 2.2 | 2.2 | 35233 | 175 | 128 µs | 14.0 |
| W7 kernel op — matrix regime (post-thrash) | 22 | 25.8 | 3.7 | 3.7 | 60751 | 262 | 124 µs | 115.9 |

## Decomposition (sums over the legs)

- engine scans: 61 µs of the 257 µs mean W7 op (23.6%) — leg 2 runs both prefix scans on the same rotation (relo → 10 edge rows, ko → 2–10 version rows); the kernel's scan-side decode is NOT in leg 2, it lands in the residual
- engine point gets: 141 µs/op (54.8%) — get_wall accumulated by the 11 head_objects (22 engine gets per op)
- kernel residual: 56 µs/op (21.6%) = W7 wall − scans − engine gets (decode + authz + assembly)
- per-get kernel check: 5.1 µs per get in the W7 op vs 3.1 µs per plain get in leg 3 (+63.5% per get beyond a plain get)
- history share: 41 µs/op (16.0% of the W7 op) — one ko/ prefix scan + decode + authz per version, zero lookups (its engine floor is part of leg 2)
- batch shape: get_many p50 131 µs vs per-target loop p50 150 µs (0.87×) — rerun 126 µs vs 149 µs (0.85×); engine get_wall inside the batch 112 µs/op vs the loop 126 µs/op; segs/op loop 116 vs batch 14 (the batch resolves the segment list once, the loop re-walks it per get — M25's warm repeated-target shape hid this cost)
- matrix reproduction: L1 p50 219 µs vs the 09-05 cell 222 µs (-1.4%) — the harness's exact draw sequence; L6 (post-thrash) 207 µs
- regime: L6 re-runs the op after W1/W3/W5-shaped thrash (20k gets + 10k histories + 100 type scans ≈ 420 MiB through the 8 MiB block cache) and moves nothing — with the matrix's random draws each op's rows are scattered, so the block cache barely matters either way; any remaining gap vs the cell is sampling-independent (the matrix's 15+ min of sustained-load CPU state, OS page-cache differences)


## Verdict

- scan share 23.6%: block-summary investigation opens — the scans' own share is material.
- kernel residual 21.6%: no kernel instrumentation.
- batch ratios 0.87× / 0.85×: inconclusive band (0.80–0.90) — the runs straddle or sit inside it; re-run the probe before deciding.
- history share 16.0%: no versions-path work — history's share is small.
