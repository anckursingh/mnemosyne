# Identity & placement — final acceptance (SE2-M39, §52)

The identity/placement architecture (spec `logical-id-physical-id.md`) closes
with this milestone. The §52 acceptance matrix, the §41/42 randomized oracle
certification, the §43 performance certification and the §44 gates, with the
§45 memory rows from SE2-M38 (identity-placement.md).

## §52 final acceptance matrix

| Row | Verdict | Evidence |
|-----|---------|----------|
| Identity stable | PASS | or001 oracle: 20,000 ops + 3 crash windows, value/identity/version/existence zero divergence (SE2M39_NIGHTLY=1); identity directory suite id010-015 |
| Physical relocation | PASS | SE2-M35 CP-001..010 + SE2-M38 §40 stress (100k restarts): every relocated rid resolves to its surviving segment; pd001 pins the direct read after relocation |
| Flush correctness | PASS | SE2-M34 FL-001..004 + fl005 (SE2-M35); §24 pins state-C/D crash windows |
| Compaction correctness | PASS | SE2-M35 compaction relocation suite; the M39 oracle exercises FLUSH/COMPACT/RESTART interleavings end-to-end |
| Crash recovery | PASS | SE2-M36 CI-001..007 (7 windows in the one publication funnel) + or001's 3 child-kill crash windows |
| Restart recovery | PASS | SE2-M37 RC-001..005 + or001's 4 fixed restart boundaries (20k-op stream) |
| Existing storage certification | PASS | W1..W8 matrix re-run (release, 100K, 2026-09-06) — gate 5: W1 6.00× v1, W2 5.48× v1 (bound ≤ 8×, M22 amendment; baseline 6.54/5.61); gates 1-4 green |
| Performance gates | PASS | §44 regression vs the 2026-09-05 release baseline: hot (W2) −2.9%, warm (W1) −2.9%, cold (W5) −1.1% — bounds 10/10/15%; correctness 100% (246 tests + oracle) |
| Memory measurement | REPORTED | §45: 419 B/object on-disk at 100k (M38); M39 probe: resident 95.7 B/object, persisted 147 B/object — see identity-placement.md + placement-direct-read.md |
| Future replica extension point | PASS | the placement directory's generation/merge gate, per-replica anchors, and the resolver views are the extension surface (§53 invariant: identity never changes from storage ops; placement may move at any time) |
| Replication implementation | NOT NEEDED | single-node MVP per spec §53 — the model, not the distribution, was the deliverable |

## §41/42 oracle certification (or001)

- Stream: 20,000 random PUT/UPDATE/DELETE/FLUSH/COMPACT/RESTART ops vs an
  independent BTreeMap oracle, 4 restart boundaries, 3 child-kill crash
  windows (acked-marker kill harness). 315 s, zero divergence.
- The oracle caught two engine defects this milestone exposed (both fixed
  TDD, both pinned): (1) a post-flush put left the placement directory
  naming a stale segment — the newest memtable row was invisible to the
  direct read (fix: put/delete flips placement before its ack, at all three
  apply sites); (2) the flip's record was pending-only, so a compaction
  that removed the segment resurrected the dangling placement on reopen
  (fix: the compaction placement log drains the pending records with the
  relocations). pd002 pins both.

## §43 performance certification

- W1..W8 re-run: the §28 matrix above (release).
- New metrics (probe_m39, n=100k): identity lookup P50 700 ns, replica
  500 ns, placement 500 ns; total read latency P50 14.7 µs (hot-key shape);
  directory resident 9.57 MB; directory persisted 14.7 MB.
- §13 evidence: artifacts/storage-engine-v2/placement-direct-read.md —
  baseline (M38: 8852 s / 228,572 reads ≈ 38.7 ms/read, ~50,000 decodes),
  root cause (placement never consulted → O(run) scan), optimization
  (placement-direct O(16) via v4 dense cadence), alternative (per-block
  rid→offset index — rejected, the placement directory IS the index).

## Known limits

- The ≤2×-of-v1 KO-lookup bound remains the amended ≤8× (user decision,
  SE2-M22): v2 is a disk engine priced against the RAM v1; the achievable
  bar is redb/RocksDB-class parity (M19 adoption-decision.md).
- Replication is explicitly out of scope (§53): the directories are the
  future-proofed surface, not a distributed implementation.
