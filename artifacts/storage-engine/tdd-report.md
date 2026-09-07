# TDD Report — AIKOQL Storage Engine (MRFC-KSE-001)

Date: 2026-09-01 · branch feature/sorage-engine

Umbrella index for the §31 canonical report set. One row per phase of
MRFC-KSE-001, pointing at the test that gates it and the report that
carries its numbers. Test names are in `crates/storage/aikoql/tests/`;
all phase suites run the real `Kernel` over `&dyn StorageEngine` (§32).

## §31 report set

```text
artifacts/storage-engine/
├── tdd-report.md            ← this file
├── conformance.md           ✓
├── crash-recovery.md        ✓
├── concurrency.md           ✓
├── corruption.md            ✓
├── encryption.md            ✓
├── benchmark.md             ✓
├── amplification.md         ✓
├── resource-usage.md        ✓
└── adoption-decision.md     ✓
```

Plus non-§31 extras the phases produced: `kse5-locality.md`,
`kse6-relationship-locality.md`, `kse7-temporal-locality.md`,
`kse14-snapshot-restore.md`, and the three certification suites'
`kse120c-writer-contention.md` / `kse142-recovery-scaling.md` /
`kse143-replay-memory.md`.

## Phase table

| phase | gates | test | report |
|---|---|---|---|
| KSE-1 | KSE-001..006 | `conformance.rs` | conformance.md |
| KSE-2 | KSE-010..017 | `kse2_key_semantics.rs` | — (pins in TESTING-PLAN §13.2) |
| KSE-3 | KSE-020..023 | `kse3_envelope.rs` + `src/envelope.rs` | corruption.md |
| KSE-4 | KSE-030..033 | `src/block.rs` tests | — |
| KSE-5 | KSE-040 | `kse5_locality.rs` | kse5-locality.md |
| KSE-6 | KSE-050..052 | `kse6_relationship_locality.rs` | kse6-relationship-locality.md |
| KSE-7 | KSE-060..063 | `kse7_temporal_locality.rs` | kse7-temporal-locality.md |
| KSE-8 | KSE-070..074 | `kse8_transaction_compat.rs` | — |
| KSE-9 | KSE-080..083 | `kse9_crash_consistency.rs` | corruption.md |
| KSE-10 | KSE-090..092 | `kse10_index_rebuild.rs` | corruption.md |
| KSE-11 | KSE-100..104 | `kse11_encryption_boundary.rs` | encryption.md |
| KSE-12 | §18 invariants | `kse12_property.rs` | — |
| KSE-13 | KSE-120 | `kse13_concurrency.rs` | concurrency.md |
| KSE-14 | KSE-130..132 | `kse14_snapshot_restore.rs` | kse14-snapshot-restore.md |
| KSE-15 | KSE-140..141 | `kse15_startup_recovery.rs` | crash-recovery.md |
| KSE-16..18 | amplification | `kse16_17_18_amplification.rs` | amplification.md |
| KSE-19 | resource usage | `kse19_resource.rs` | resource-usage.md |
| KSE-20 | backend conformance | `kse20_backend_conformance.rs` | conformance.md |
| M7 | W1..W8 + §28 matrix + §29 gate | `kse_m7_workloads.rs` | benchmark.md + adoption-decision.md |
| Cert-082B | TEST-KSE-082B-01..03 | `kse82b_middle_corruption.rs` | — (pins in TESTING-PLAN row 617) |
| Cert-120C | §5 matrix 1/2/4/8/16/32 × 0/32 | `kse120c_writer_contention.rs` | kse120c-writer-contention.md |
| Cert-142 | §6 matrix 1/10/100 MB (1 GB opt) | `kse142_recovery_scaling.rs` | kse142-recovery-scaling.md |
| Cert-143 | §7 peak-replay multiplier | `kse143_replay_memory.rs` | kse143-replay-memory.md |
| Cert closure | §10-12 gate | cross-suite (rows above) | `docs/testing/STORAGE_ENGINE_MVP_CERTIFICATION_CLOSURE.md` |

Phases with "—" have no standalone report: their evidence is the pinned
test itself (TESTING-PLAN §13.2 rows carry the pin descriptions). The
generated reports (concurrency, crash-recovery, amplification,
resource-usage, benchmark, adoption-decision, and the three locality
extras) are written by the suites' `*_report` tests, so they always carry
numbers from the same run.

## Verdict

`adoption-decision.md` ends in exactly one verdict:

```text
ADOPT AIKOQL STORAGE ENGINE
```

10 of 11 workloads faster than redb at 100K (best 2.90×, one loss inside
the §29 bound), resource bounds held (disk 0.42×, CPU 0.34×, RSS 1.19×
redb). The verdict is live: `aikoql-mcp` and the Python SDK default to
`AikoqlStorageEngine` (post-gate §6 wiring, commit e605617), with redb
reachable via `AIKOQL_BACKEND=redb` / `backend="redb"` and the REC-002
backup/restore flow as the migration path.
