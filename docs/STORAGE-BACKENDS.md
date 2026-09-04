# AIKOQL Storage Backends — Public Contract

One `db_path`, three engines. Selection flows through the single
`RuntimeConfig` pipeline (defaults → TOML → env → CLI) and is resolved at
open (`crates/services/api/mcp/src/engine.rs`). An explicit backend opens
exactly that engine; the default auto-detects the format already at the
path, so an upgrade never reinterprets an existing database.

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
| directory containing `CURRENT` | `aikoql-v2` |
| file starting with the `AKQL` magic | `aikoql` |
| anything else, missing path included | `redb` (redb validates its own format and fails closed) |
| directory without `CURRENT` | explicit error — name a backend |

Detection is what makes the default switch safe in both directions: a redb
database from before the switch and a native WAL written while `aikoql` was
the production default both keep working at the same path.

## Profiles — not interchangeable

| backend | intended profile | memory model | startup | recovery |
|---|---|---|---|---|
| `redb` | stable compatibility / general embedded | file-backed B-tree, bounded | O(index) | transactional |
| `aikoql` | query-heavy, RAM-affordant, bounded dataset | full dataset in memory | O(WAL size) — full replay | torn-tail resync; replay stops at last good record |
| `aikoql-v2` | bounded-memory / bounded-recovery, experimental | memtable + immutable segments + tiered compaction | O(manifest) + tail replay | segment-scoped; WAL seq strictly monotonic |

Do not treat the three as interchangeable "AIKOQL native storage":

- `aikoql`'s WAL grows unbounded and replay is full — deployment boundary
  documented in `crates/storage/aikoql/src/lib.rs` (PR#2 review SE-03).
- `aikoql-v2` is experimental and is not the default (adoption evidence:
  NOT ADOPT, `artifacts/storage-engine/`).
- `redb` is the stable default; the other two are explicit opt-ins.
