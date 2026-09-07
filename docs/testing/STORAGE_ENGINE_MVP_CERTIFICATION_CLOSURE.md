# Storage Engine MVP Certification Closure

Per `docs/AIKOQL_Storage_Engine_MVP_Certification_TDD.md` §10-13. Date: 2026-09-01 · branch `feature/sorage-engine` · engine: `AikoqlStorageEngine` (`crates/storage/aikoql`) · all four suites run in **release** mode with printed seeds on Windows 11 (the environment each report records; RSS via PowerShell WorkingSet64, Windows-only).

## Executive Verdict

```text
PASS WITH ACCEPTED LIMITATIONS
```

All four findings closed with release-mode evidence. The accepted limitations are the design's by-construction boundaries — full WAL replay at open, single-writer serialization, no compaction — each now measured and published, not merely documented (Q1-Q4 below). Nothing measured violates an MVP SLO; the only asserted gates (fail-closed corruption, 100% acknowledged-write recovery, 100% semantic recovery) passed at every size.

## Results

| Test | Status | Evidence |
|---|---|---|
| KSE-082B | PASS | `kse82b_middle_corruption.rs` 4/4 — every header/payload/checksum leg (TEST-KSE-082B-01/02/03) fails closed with the WAL byte-unchanged (size AND hash asserted); genuine torn tail still truncates at the record boundary; the only production change of the whole document: a torn tail is legitimate only when NO complete, checksum-verified record parses after the torn offset (commit 31971a2). Regression: lib 11/11, kse2 8/8, kse3 3/3, kse9 1/1, kse11 1/1, kse12 1/1, kse14 3/3, kse15 2/2 |
| KSE-120C | PASS | `kse120c_writer_contention.rs` 2/2 — the doc's full matrix, writers 1/2/4/8/16/32 × readers 0/32, 20,000 unique-key writes per scenario (release). Hard gate asserted at every cell: drop → reopen → recovered == acknowledged, byte-exact. Throughput 1,544 → 1,491 writes/sec (97% — plateau, not collapse); write P50 flat 0.59 → 0.65 ms; tail P95 0.92 → 99.4 ms / P99 1.09 → 180.4 ms at 32 writers (queueing on the serialized fsync — durability cost, not lock cost); 32 readers hammer at ~1.2 K reads/sec without disturbing writes. Report: `artifacts/storage-engine/kse120c-writer-contention.md` |
| KSE-142 | PASS | `kse142_recovery_scaling.rs` 3/3 — release matrix 1/10/100 MB WAL: cold open 6.3 / 49.9 / 376.7 ms (linear, 3.8 ms/MB); first query 4-5 µs; 100% semantic recovery asserted at every size (full model equality, 4 prefix scans, delete-absent, overwrite-final-value pins). Recovery SLO proposed from the measured slope × 1.5 headroom: open(100 MB) ≤ 565 ms, open(1 GB) ≤ 5,650 ms. Report: `artifacts/storage-engine/kse142-recovery-scaling.md` |
| KSE-143 | PASS | `kse143_replay_memory.rs` 3/3 — release matrix 1/10/100 MB WAL: peak replay memory multiplier 1.00x → 8.77x (peak 112.8 MB / final 12.9 MB at 100 MB WAL — the raw WAL buffer coexists with the live store during replay, exactly the PoV's predicted shape). Marginal slope 1.04 B per WAL byte → deployment memory requirement: ~9 MB baseline + 1.04 B/WAL-byte × WAL cap × 1.2 headroom = **~134 MB RAM reserved for open() at a 100 MB operational WAL cap**. Report: `artifacts/storage-engine/kse143-replay-memory.md` |

## Coder Point of View

### KSE-082B — middle-record corruption with a valid tail

- **QA concern:** a corrupted middle record followed by valid records must not be treated as a crash tail and silently truncated (acknowledged-data loss).
- **Coder interpretation:** `parse_at` already failed closed on magic/version/type/checksum; the one gap was a corrupted `payload_len` overrunning EOF classifying as `TornTail`, which replay then truncated.
- **Agree:** fully — the RED leg reproduced silent truncation of acknowledged records exactly as inspection predicted.
- **Evidence:** TEST-KSE-082B-03's payload_len-overrun leg RED → minimal fix (resync scan in replay's TornTail arm; ~2^-64 false-positive odds, fails in the safe direction) → all legs GREEN, WAL size AND hash unchanged on every fail-closed assert.
- **Final decision:** P0, fixed in production (the only production change this document caused). Policy A (fail closed) with tail-vs-middle distinguished by construction.

### KSE-120C — writer contention scaling

- **QA concern:** no measured evidence of how throughput/latency behave as writers scale; the risk that the single log Mutex collapses under contention.
- **Coder interpretation:** partially confirm — the evidence gap is real, but the implied risk is misplaced: single-writer serialization is intentional (log Mutex across append+fsync+apply so log order == commit order, the KSE-13 120a fix) and matches the kernel's single-writer pipeline; fsync dominates the serialized section.
- **Partially agree** → confirmed by measurement.
- **Evidence:** release matrix above — 97% throughput retention at 32 writers, write P50 flat, tails growing as fsync queueing. Zero production changes: the RED was the missing evidence itself, and no measured number violated an SLO.
- **Final decision:** P1, closed by evidence. Single-writer serialization certified for AIKOQL's mutation patterns; the 32-writer row is deliberately beyond any real workload (§13 excludes very-high-concurrent-writes anyway).

### KSE-142 — recovery scaling

- **QA concern:** recovery semantics pinned at 2,200 records; the scaling curve and the MVP dataset limit unmeasured.
- **Coder interpretation:** confirm as a certification gap, not a suspected defect — replay is O(bytes) parse + O(records) amortized-linear inserts (memory-first by design, KSE-19 §25 verdict).
- **Agree.**
- **Evidence:** release matrix 1/10/100 MB — open 6.3 → 376.7 ms, linear 3.8 ms/MB, first query µs-scale, 100% semantic recovery asserted at every size (full model equality via the re-derived deterministic model, not spot checks).
- **Final decision:** P1, closed. MVP dataset boundary: 100K KOs recommended (M7's scale), 1M KOs the measured ceiling (KSE-19). Recovery SLO proposed from the slope (reported, not asserted — §9).

### KSE-143 — large replay resource stability

- **QA concern:** steady-state RSS measured (KSE-19), startup PEAK not — deployment risk is peak, not final.
- **Coder interpretation:** confirm; expected shape peak ≈ final + WAL bytes (the raw WAL buffer, decode allocations, and the live BTreeMap coexist during replay), so the multiplier grows with the WAL-to-live ratio.
- **Agree.**
- **Evidence:** multiplier 1.00x at 1/10 MB → 8.77x at 100 MB — the predicted shape, now measured. Deployment memory requirement published (134 MB at a 100 MB WAL cap).
- **Final decision:** P1, closed. The multiplier IS the full-replay design's startup cost; the mitigation is bounding the WAL (operational cap + the §13 exclusions), not a code change.

## Measured Operational Boundary

```text
recommended maximum MVP dataset   100K Knowledge Objects (M7's adoption scale; agent memory /
                                  repository knowledge / document knowledge)
measured ceiling                  1M Knowledge Objects (KSE-19: 1,242 s ingest, 4.13 GB RSS,
                                  645 MB heap, 645 B/KO linear)
recommended memory                ~134 MB RAM reserved for open() at a 100 MB operational WAL cap
                                  (baseline ~9 MB + 1.04 B per WAL byte × 1.2 headroom, KSE-143);
                                  steady-state linear in live keys (KSE-19)
expected startup behavior         cold open linear at 3.8 ms per WAL MB (release): 100 MB → 377 ms,
                                  first query 4-5 µs; recovery byte-exact, torn tails truncated at
                                  the record boundary, corruption fails closed with the WAL untouched
expected write concurrency        single-writer pipeline by design (kernel commits batches through
                                  one pipeline); engine-level ~1.5K fsynced batches/sec; a 32-writer
                                  storm retains 97% of the 1-writer rate with byte-exact recovery
                                  (KSE-120C); 32 concurrent readers are undisturbed
known unsupported scale           unbounded historical datasets (full replay at open — WAL grows
                                  forever, no compaction), multi-terabyte storage, very high
                                  concurrent writes, general-purpose OLTP (§13's not-certified list)
```

## §11 Challenge Questions

**Q1 — Is fail-closed middle corruption always correct?** Yes — for THIS engine. The WAL carries no redundant framing, so the two alternatives both destroy acknowledged commits: skipping a corrupted record loses that record, truncating at the corruption loses everything after it. Fail-closed preserves the evidence for an operator decision (KSE-082B pins the WAL byte-unchanged). The tail case is not collateral damage: a torn tail is legitimate only when nothing complete parses after the torn offset — the resync classifier distinguishes the two by construction. A safer alternative exists only with more framing (per-segment CRCs, file checksums) — that belongs to the compaction/segmented-WAL era, not MVP; Q4 covers the boundary.

**Q2 — Is single-writer architecture actually a bottleneck for AIKOQL?** No. Measured, not assumed: Knowledge Object mutations are one kernel committing batches through one pipeline (single-writer by design — there is no second writer to contend); write P50 is FLAT from 1 to 32 engine-level writers (0.59 → 0.65 ms, release) — the serialized section is the fsync, so latency is durability cost, not lock cost; throughput retains 97% under a 32-writer storm (1,544 → 1,491 batches/sec). Against the actual workloads: interactive updates are ms-scale point ops through that pipeline (KSE-13 measured ~4.5-6 ms end-to-end through the kernel); repository/document ingestion is batch-bound at ~1.5K fsynced batches/sec, and M7's W6 ingestion measured 2.90× redb WITH replay-at-open priced in; agent workloads are read-dominated, and 32 concurrent readers hammer without disturbing writes (~1.2 K reads/sec). The engine does not support multi-process sharing — within-process threads are its real contention surface, and the kernel's own pipeline is single-writer. The number that would change this answer is a parallel-writer ingestion workload — excluded from MVP (§13).

**Q3 — Is full replay acceptable for MVP?** Yes. Target dataset: 100K KOs (recommended) / 1M KOs (measured ceiling). Measured recovery time: linear 3.8 ms per WAL MB, 100 MB → 377 ms cold open, first query 4-5 µs (KSE-142, release). Memory requirement: ~134 MB reserved for open() at a 100 MB WAL cap (KSE-143). Deployment profile: local/embedded single-node — the open happens at process start against a bounded WAL, so sub-second replay is inside the operator's tolerance. Unacceptable only for unbounded history — which §13 already excludes from certification. The SLO (open ≤ slope × 1.5) is the tripwire that reopens this question if the WAL cap is raised.

**Q4 — Should compaction block MVP release?** **NO.** Justified by measurement: (1) replay cost is already priced — M7's ADOPT verdict (ingest 2.90× redb) ran against an engine that replays the whole WAL at open, and 100 MB replays in 377 ms against a 565 ms SLO; (2) disk growth is bounded and measured — storage amplification 1.94× logical (KSE-16), and the MVP datasets (100K KOs) produce ~100 MB-scale WALs, not TB; (3) no phase mandates it, and the WAL-growth limitation is published in the operational boundary above. Compaction becomes P0 when a measured MVP workload exceeds a published boundary (replay time or disk), not before.

## §12 Final Certification Gate

### Correctness

- [x] KSE-082B passes (4/4 legs, fail-closed with WAL byte-unchanged).
- [x] Middle corruption behavior is explicitly defined and tested (every header/payload/checksum field; resync classifier distinguishes tail vs middle).
- [x] Torn final writes recover correctly (control test: truncation at the record boundary; KSE-9/KSE-15 real-kill evidence).
- [x] Failed corruption open does not silently mutate WAL (size AND hash asserted unchanged on every fail-closed leg).

### Concurrency

- [x] KSE-120C completed (2/2; release matrix, 20,000 writes/scenario).
- [x] All tested writer counts preserve acknowledged-write correctness (drop → reopen → recovered == acknowledged, byte-exact, at all 7 cells).
- [x] Contention behavior measured (throughput plateau 97%, write P50 flat, tail growth quantified).

### Recovery

- [x] KSE-142 completed (3/3; release 1/10/100 MB).
- [x] Recovery scaling documented (linear 3.8 ms/MB; SLO open(100 MB) ≤ 565 ms proposed from the slope).
- [x] Target MVP dataset defined (100K KOs recommended, 1M measured ceiling).

### Resources

- [x] KSE-143 completed (3/3; release 1/10/100 MB).
- [x] Peak replay RSS measured (112.8 MB at 100 MB WAL — the 8.77x multiplier).
- [x] Final RSS measured (12.9 MB at 100 MB WAL).
- [x] Deployment memory requirement documented (~134 MB RAM at a 100 MB operational WAL cap).

### Evidence

- [x] Release-mode results (all three measurement suites ran release; 082B is mode-independent).
- [x] Environment documented (Windows 11, build profile, per-report).
- [x] Seeds reproducible (0x120c0000 / 0x14200000 / 0x14300000, deterministic walgen LCG; model crosses the process boundary by re-derivation, determinism pinned by parent re-run compare).
- [x] No unsupported performance claims (honest limits + NOT_MEASURED rows in every report; SLOs reported-not-asserted per §9).

## §13 Certification Scope

The engine is certified as:

```text
AIKOQL MVP Native Storage Backend
```

It is NOT marketed as:

```text
Universal General-Purpose Database Storage Engine
```

Certified strengths: agent memory, Knowledge Objects, repository knowledge, code intelligence, document knowledge, ontology-driven applications, medium-scale knowledge graphs, local/embedded deployments.

Not yet certified: unbounded historical datasets, multi-terabyte storage, very high concurrent writes, general-purpose OLTP workloads.

## V2 Hardening Review — SE2-M40 (2026-09-07)

Review: `AIKOQL_Storage_V2_Immediate_Hardening_Physical_Resolution_TDD.md` (in `E:\downloads`). Its P0-1/P0-2 (bounded recovery + crash-safe checkpoint publication) are exactly the v1 closure's published limitation — full history replay at open — reappearing in the v2 directory metadata; P0-3 (explicit generation allocation) extends INV-05. The review's M1–M4 and M8–M11 were already shipped by SE2-M29–M39 (identity/placement directories, crash injection, recovery, relocation stress, randomized oracle); this pass closed M5–M7 + M12 as SE2-M40 and wrote the §16 challenge dispositions with evidence.

### §16 Challenge dispositions (written reasoning as the review mandates)

- **Challenge A — Is `PhysicalHandle` justified? REJECTED on evidence.** The proposed §5 `PhysicalHandle` is a rename of state the shipped `Placement` enum already resolves (`Memtable` / `Segment(PhysicalLocation)` / `Retired`, SE2-M32). M39's direct-read certification measured the resolver at 500–700 ns P50 — no hotspot a repr-transparent wrapper would fix — and M21's attribution attributed the point-read cost to block I/O (21.4 µs of 39.5 µs), not metadata resolution. The correctness risk the handle names (physical location validity) is already enforced by `validate_segment_location` + the fail-closed decode paths.
- **Challenge B — Checkpoint topology: coordinated, one file.** The identity, replica, and placement directories are ONE consistency domain — a flush publishes all three at the same manifest generation, so per-directory checkpoints would still need a joint marker to be atomic. One `CHECKPOINT-{gen:06}.log` snapshot at one generation wins on publish count (one atomic rename), atomicity (no cross-file crash state), and prune simplicity (one `generation ≤ G` cut).
- **Challenge C — Generation authority: real defect, fixed.** The pgen orphan-burn window: a state-C orphan PLACEMENT log (SE2-M35's after_location crash) can carry a pgen that the allocator re-issues for a later relocation — INV-05's "generations never silently reused" violated. Fix: `orphan_placement_max_generation(dir, current)` folds orphan placement pgens into the replay seed at open, so the next published generation strictly exceeds every orphaned one. ckp006 pins it, RED-verified (the test fails against the pre-fix allocator).
- **Challenge D — Compaction ordering: already shipped, no change.** The §7 required ordering (segment write → relocation PLACEMENT log → manifest → CURRENT → retire) is exactly the shipped publication order — certified by SE2-M35's cp009/cp010 state-C/state-D pins and SE2-M36's seven-window crash matrix.

### Milestone dispositions

| Milestone | Disposition |
|---|---|
| M1 generation allocator | shipped SE2-M30 (allocators resume at max+1 across restart) |
| M2 PhysicalHandle type safety | REJECTED (Challenge A — the `Placement` enum already resolves) |
| M3 physical directory | shipped SE2-M32 (placement directory) |
| M4 placement → handle → location | shipped SE2-M32/M33 (enum resolution + memtable integration) |
| M5 checkpoint writer | shipped SE2-M40 ckp001/ckp002 (golden + replay equivalence) |
| M6 delta replay | shipped SE2-M40 ckp002/ckp005 (checkpoint ≡ full-replay byte equality) |
| M7 crash-safe checkpoint rotation | shipped SE2-M40 ckp004 (five crash windows) |
| M8 compaction relocation | shipped SE2-M35 |
| M9 post-flush write regression | shipped SE2-M39 (oracle caught 2 defects, pd002 pins) |
| M10 compaction restart regression | shipped SE2-M37/SE2-M38 |
| M11 randomized oracle | shipped SE2-M39 (20k ops + 3 crash windows, zero divergence) |
| M12 directory growth certification | shipped SE2-M40 ckp008 |

### M12 gate — measured (release, 2026-09-07)

The review's gate: recovery must not grow linearly with total historical metadata mutations once checkpointing is active. At 600K updates over 10K objects (`ckp008_growth_probe`, `SE2M40_NIGHTLY=1`; artifact `artifacts/storage-engine-v2/directory-checkpoint.md`):

| arm | metadata bytes | log files | warm open |
|---|---|---|---|
| checkpointing off | 41,077,878 | 303 | 347.6 ms |
| checkpoint budget 2 MiB | 1,734,216 | 7 | 75.0 ms |

The off arm's open climbs with the update count (every published log is decoded); the checkpoint arm's stays flat at the live-state size (~10K identity/replica records + the trigger window of placement records). Open is now proportional to checkpoint + deltas-after — 4.6× faster warm open and 23.7× less metadata at the measured ceiling. Residual open-time growth is the segment set (identical across arms), not the directory.

### Review Definition of Done

- Correctness: generation allocation restart-safe (SE2-M30, ckp006); never silently reused (ckp006, RED-verified); checkpoint + delta recovery (ckp002/ckp005); all checkpoint crash points (ckp004); invalid references fail closed (ckp001 damage matrix); post-flush and compaction/restart regressions (SE2-M39, SE2-M37/M38); randomized oracle (SE2-M39).
- Scalability: checkpointing implemented, historical replay bounded, obsolete deltas pruned (ckp005: 0 surviving logs at the trigger), recovery benchmark produced, recovery not proportional to metadata history (M12 table).
- Performance: direct reads bounded (SE2-M39: read P50 14.7 µs at 8.5 decodes); resolver overhead measured (500–700 ns P50 — the Challenge A evidence); hot-path regression within the amended gate (SE2-M39 §44: −2.9/−2.9/−1.1% vs baseline, bounds 10/10/15%); no segment scan introduced; bounded entry decode preserved (SE2-M34 mt007/§11 pin).
- Architecture: ObjectId/LogicalId/ReplicaId stability re-pinned by the M38 100k stress and the M39 oracle; physical relocation never rewrites logical identity (SE2-M35); the physical layer supports future remote locations by construction (the `Placement` enum's variant space, no remote variant implemented — per the review's §18).
