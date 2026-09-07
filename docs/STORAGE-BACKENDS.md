# AIKOQL Storage Backends — Public Contract

One `db_path`, three engines. Selection flows through the single
`RuntimeConfig` pipeline (defaults → TOML → env → CLI) and is resolved at
open (`crates/services/api/mcp/src/engine.rs`). An explicit backend opens
exactly that engine; the default auto-detects the format already at the
path, so an upgrade never reinterprets an existing database — and a fresh
(missing) path creates `aikoql-v2`, the 2026-09-07 ratified default (ADR
`docs/STORAGE-ENGINE-ARCHITECTURE-DECISION.md`).

## Selection

| layer | knob | values |
|---|---|---|
| default (auto) | — | detect format at `db_path` |
| TOML | `storage.backend` | `redb` \| `aikoql` \| `aikoql-v2` |
| env | `AIKOQL_BACKEND` | same |
| CLI (`serve`) | `--backend` | same |

Precedence is the standard pipeline order: TOML < env < CLI. An unknown
value fails closed at config load — never a silent fresh create.

## Auto-detection

| `db_path` | opens as |
|---|---|
| missing path (fresh create) | `aikoql-v2` — the ratified production default (2026-09-07 ADR) |
| directory containing `CURRENT` | `aikoql-v2` |
| file starting with the `AKQL` magic | `aikoql` |
| any other existing file | `redb` (redb validates its own format and fails closed — snapshots and pre-flip databases keep working) |
| directory without `CURRENT` | explicit error — name a backend |

Detection is what makes the default switch safe in both directions: a redb
database from before the switch and a native WAL written while `aikoql` was
the production default both keep working at the same path.

## Profiles — not interchangeable

| backend | intended profile | memory model | startup | recovery |
|---|---|---|---|---|
| `aikoql-v2` | the production default — bounded-memory, bounded-recovery, write-mixed | memtable + immutable segments + tiered compaction | O(manifest) + tail replay (checkpoint-bounded, SE2-M40) | segment-scoped; WAL seq strictly monotonic |
| `redb` | opt-out compatibility / general embedded | file-backed B-tree, bounded | O(index) | transactional |
| `aikoql` | read-hot, RAM-affordant, bounded dataset | full dataset in memory | O(WAL size) — full replay | torn-tail resync; replay stops at last good record |

Do not treat the three as interchangeable "AIKOQL native storage":

- `aikoql`'s WAL grows unbounded and replay is full — deployment boundary
  documented in `crates/storage/aikoql/src/lib.rs` (PR#2 review SE-03).
- `aikoql-v2` is the default since 2026-09-07 (certified matrix + M40
  checkpoint, `artifacts/storage-engine-v2/adoption-decision.md`); the
  09-01 NOT ADOPT record was superseded by that decision.
- `redb` remains the compatibility fallback — snapshots, pre-flip
  databases, and explicit `storage.backend = redb` keep working; it and
  `aikoql` are explicit opt-ins.
