# ADR — AikoQL Storage V2 as the Forward Storage Engine

- **Status:** accepted — ratified by the user 2026-09-07 (record: `artifacts/storage-engine-v2/adoption-decision.md` §Ratification; the default flip is shipped: `engine.rs` missing-path auto-create → v2, Python SDK `backend = "aikoql-v2"`)
- **Date:** 2026-09-07 · **Branch:** `feature/sorage-engine`
- **Candidates:** `aikoql-v2` (segmented LSM), `aikoql` v1 (WAL + RAM mirror), `redb` (COW B-tree), `rocksdb` (C++ LSM, reference point)
- **Evidence basis:** the certified cross-engine matrix `artifacts/storage-engine-v2/workloads.md` (2026-09-06, release, 100K KOs / 20K ops), `adoption-decision.md` (09-01/09-04/09-05), `directory-checkpoint.md` (09-07), M7 v1-adoption matrix, KSE-142/143 recovery-memory suites. All numbers below are measured in this repo; nothing is estimated.

## 1. Context

One application — AIKOQL agent knowledge — four possible engines behind one `StorageEngine` trait. The application's actual load is **write-mixed**: ingestion, knowledge-object creation, context compilation, relationship traversal; point reads are frequent but only one of eight certified shapes. The system targets bounded-RAM deployments (agent hosts), bounded restart time, and a future where replication/sharding must not force a format rewrite (`logical-id-physical-id.md` §53: separate WHAT an object is from WHERE a copy lives).

The certified W1–W8 matrix (page-cache-warm regime, p50 unless noted):

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
| W8 mixed 70/20/10 | 6 µs · p99 42 µs | 13 µs · **p99 31,994 µs** | 9 µs · **p99 31,159 µs** | 45 µs · **p99 978 µs** (8,231 ops/s) |
| W6 ingestion | 21 µs (48,770 ops/s) | 2,023 µs (494 ops/s) | 724 µs (1,380 ops/s) | 728 µs (1,373 ops/s) |

Resources at 100K (09-04, post-M15/M16): seed wall v2 221.6 s / v1 230.2 s / redb 838.7 s; peak RSS v2 **428.05 MiB** / v1 611.22 / redb 513.94; disk v2 **347.99 MiB** / v1 435.44 / redb **1.00 GiB**.

## 2. Engine-by-engine analysis

### `aikoql` v1 — WAL + RAM mirror

Architecture: every write appends to one WAL and applies into an in-memory BTreeMap; the RAM mirror **is** the store. KSE-142/143 measured the price: open replays the whole WAL at 3.8 ms/MB (open(100 MB) = 376.7 ms; SLO 565 ms) with a **8.77× peak-RSS multiplier** (112.8 MB peak for a 100 MB WAL).

Verdict: unbeatable warm reads (zero disk by design) — and a dead end. No dataset-larger-than-RAM capability at all, unbounded WAL growth, linear replay, no compaction. Its read edge is a design gift that caps its ceiling; the W8 p99 (31.2 ms) shows the ungrouped per-commit fsync.

### `redb` — copy-on-write B-tree

Architecture: single-file MVCC B-tree, 4 KiB pages, transactional commits. Recovery is open-the-file; warm point reads are its home turf (18 µs W1 — 4 KiB pages vs v2's 64 KiB blocks is the whole story, `adoption-decision.md` §Diagnosis).

Verdict: excellent at what it optimizes, and structurally capped at everything else: **2.0× the disk** of v2 (1.00 GiB at 100K), **2.8× slower ingestion** (494 vs 1,373 ops/s), **33× worse mixed-write p99** (31,994 µs vs 978 µs — every commit pays the full COW + fsync), no compaction (file grows with history), no identity or placement concept — the application would carry ObjectId→location indirection itself, and its single-file format has no sharding path. Its 32 ms write tail is not a bug; it is the COW design's price, exactly as v1's replay is the mirror's.

### `rocksdb` — C++ LSM (reference point, not a candidate backend)

Not measured in this repo — no backend exists, and no benchmark row is claimed. Architecture-only: RocksDB is the industry LSM, and v2 deliberately mirrors its shape (WAL + memtable, immutable sorted runs, tiered compaction, bloom filters, block cache). What it would cost this application: a C++/MSVC cross-compile dependency in a pure-Rust codebase (the M22 survey already showed native-asm shims failing on this toolchain); the two-cache memory duplication (memtable + block cache + pinning) that v2 avoids with one bounded block cache; level-compaction write stalls surfacing as p99 spikes; and an order of magnitude more tuning surface. It is also a KV store: the ObjectId → LogicalId → ReplicaId → PhysicalLocation hierarchy this application needs (opened spec `logical-id-physical-id.md`) would sit on top of it as application code, not engine guarantees. The right reading: **v2 is this application's Rust LSM, specialized to its actual data model** — the best-practices LSM pieces, none of the generality tax.

### `aikoql-v2` — segmented LSM with an identity/placement layer

The full architecture: group-commit WAL → memtable → immutable segments (bloom + block index + v4 dense cadence) → size-tiered compaction with relocation → identity/replica/placement directories with checkpoint-bounded recovery. Every cost axis is a **tunable knob**, not a design wall — `db.rs` Config carries all of them:

```rust
pub memtable_bytes: usize,   // flush trigger (SE2-M2)
pub durability: DurabilityMode, // Sync | GroupCommit | Async (SE2-M6)
pub max_wait_duration: Duration, // committer group window
pub cache_bytes: usize,      // bounded LRU block cache (SE2-M7)
pub l0_compact_trigger: usize,  // count gate (SE2-M10/M16)
pub merge_chunk_bytes: usize,   // bounded merge emission (SE2-M20)
pub checkpoint_bytes: usize,    // directory checkpoint budget (SE2-M40)
```

## 3. The honest scorecard

v2's wins are decisive on the axes this application actually lives on; its deficit is one axis, precisely measured, with named levers.

**v2 wins vs redb:** ingestion 2.8×; mixed-workload throughput 6.0× with a 33× better p99 tail; peak RSS −17%; disk −65%; bounded open via the M40 checkpoint (75.0 ms vs 347.6 ms at 600K updates — `directory-checkpoint.md`); dataset-larger-than-RAM queryability (gate 2); a compile-time identity/placement model redb does not have.

**v2 wins vs v1:** peak RSS −30%; disk −20%; mixed p99 32× better (978 µs vs 31,159 µs); ingestion at parity; bounded recovery (v1 replays the entire WAL — 8.77× peak RSS multiplier, 3.8 ms/MB linear); compaction exists (v1 has none — its disk grows with every write); larger-than-RAM datasets possible (impossible in v1); placement-direct hot reads (M39 certification probe: 8852 s debug baseline → 3.15 s release — the same fix, both regimes measured in the evidence pack).

**v2's one deficit — warm point reads:** 6.00× v1 (W1) / 5.48× (W2) — inside the amended ≤8× design gate with 1.2–1.5× headroom — and ~1.9× redb on W1. Root cause is measured, not guessed: one 64 KiB block fetch + soft-sha256 checksum per cold get (~18.7 µs of the 33.5 µs M22 get_wall, `attribution.md`), against redb's 4 KiB pages and v1's zero disk. This is the bounded-RAM trade, priced precisely.

**Why the deficit is tunable, not architectural:** the read path already walks memtable → placement-direct anchor → bloom → block index → bounded decode, and each stage has a shipped knob or a shipped lever behind it: the M39 placement-direct dispatch cut the hot-key path 2,800× (O(run) → O(16) decode window); the M17 tiered path bounds segment walks; block target, cache sizing, and the M25/M27 batch API are named, measured levers — the adoption-decision.md remediation section priced them (blocks ~4–8×, compaction 2–4×). v1's replay and redb's write tail have no equivalent lever — they are the designs.

## 4. Code evidence — the four properties that make the decision

### 4.1 The hot read is placement-direct, not a scan

`db.rs:935` — the M39 §13 fix, the read that answers in O(RESTART_INTERVAL):

```rust
// SE2-M39 §13 — a Segment-placed replica answers placement-direct:
// its anchor decodes O(RESTART_INTERVAL) entries instead of
// scanning the key's whole equal-key run. A put flips the placement
// before its ack, so a Segment placement means no newer row can
// hide in a memtable. An anchor that doesn't answer (pre-v4
// segment, stale, or another key of a multi-key object) falls back
// to the rid-filtered scan, which answers every case.
let (mem_hit, segments, direct) = {
    let state = self.state.read().unwrap();
    let direct = match state.placements.get(&rid) {
        Some(Placement::Segment(loc)) => state
            .segment_records
            .iter()
            .position(|r| r.segment_id == loc.segment_id.0)
            .and_then(|i| state.segments.get(i).cloned())
            .map(|seg| (seg, *loc)),
        _ => None,
    };
    let mem_hit = if direct.is_some() { /* no memtable shadowing possible */ }
    ...
};
```

This is the §12 hot-path rule honored with evidence: resolver indirection is 500–700 ns P50 per hop, the read remains block-I/O-bound.

### 4.2 Recovery is bounded by a checkpoint, never by history

`checkpoint.rs` — the M40 protocol the review demanded, in production:

```rust
/// Atomic publish. SE2-M40 — staged: the crash-window harness parks
/// inside the temp's write/fsync (`AIKOQL_V2_PLACE_PARK` naming
/// `FAIL_AFTER_CHECKPOINT_WRITE` / `_FSYNC`, the M36 plumbing).
pub fn publish_staged(path: &Path, checkpoint: &Self, stage: Option<&str>)
    -> Result<(), FormatError> { ... } // write-temp → fsync → rename

// load_newest: the newest valid checkpoint ≤ CURRENT; a file whose
// internal generation disagrees with its name is a publication anomaly
// — fail closed, never pick. Damage fails closed ALWAYS: no fallback
// to the deltas (unsound after a partial prune).

// prune_deltas_before: delete every delta log at or below `generation`
// and every OLDER checkpoint; a leftover is harmless — re-applying old
// logs is idempotent under the merge gates.
```

Measured: at 600K updates, open = checkpoint + deltas-after = **75.0 ms / 1.73 MB**, versus full-history replay at 347.6 ms / 41.1 MB — and the checkpoint arm's open is flat at live-state size while the off arm grows with every published log.

### 4.3 The write path groups commits — the p99 nobody else matches

`db.rs:774` + `db.rs:1954`:

```rust
pub fn writer(&self) -> Result<CommitWriter, FormatError> {
    match &self.queue_tx {
        Some(tx) => Ok(CommitWriter { tx: tx.clone() }),
        None => Err(FormatError::Invalid(
            "writer handles require DurabilityMode::GroupCommit".into(),
        )),
    }
}

fn committer_loop(
    rx: mpsc::Receiver<Batch>,
    wal: Arc<Mutex<File>>, ...
) {
    let wait = config.max_wait_duration;
    let mut carry: Option<Batch> = None;
    loop {
        let first = match carry.take().or_else(|| rx.recv().ok()) { ... }
        // one fsync per group, apply-before-ack, exact-fit caps
```

The consequence is the W8 cell: p99 978 µs while both other durable engines sit at ~31–32 ms — a 32× tail-latency win that is structural, not noise.

### 4.4 Identity is compile-time — no `u64` substitution can slip through

`identity/logical_id.rs:9` and friends, with ID-004 a `compile_fail` doc-test:

```rust
pub struct LogicalId(pub u64);
pub struct ReplicaId(pub u64);
pub struct NodeId(pub u64);
pub struct ObjectId(pub [u8; 16]);

// the §6.3 example, verbatim, as a compile_fail test:
// fn accepts_replica(_: ReplicaId) {}
// let logical = LogicalId(42);
// accepts_replica(logical);   // MUST NOT COMPILE
```

This is the M28 spec's §28.1 requirement realized: `LogicalId(42) != ReplicaId(42)` at the type level even though both are 8 bytes. It is the property that makes future replication (LogicalId + NodeId → ReplicaId per node) a data change, not a format migration.

## 5. Decision

**ADOPT AikoQL Storage V2 as the forward storage engine.**

1. **v2 is the strategic target and the default** — ratified 2026-09-07 and shipped: a fresh (missing) `db_path` auto-creates `aikoql-v2` (`engine.rs`), the Python SDK defaults to `backend = "aikoql-v2"`, and `STORAGE-BACKENDS.md` now records v2 as the production default. (The `adoption-decision.md` amendment recorded that a certified matrix passing all gates under the amended bound re-opens the question — M39's matrix did exactly that, and M40 closed the review's last open P0.)
2. **v1 remains available** as the read-hot/RAM-affordant profile via the existing `storage.backend = aikoql` knob — auto-detection at the path makes any direction of the switch safe (`STORAGE-BACKENDS.md`). No migration is required to keep it.
3. **redb stays the opt-out compatibility backend**, not a target.
4. **rocksdb is not adopted** — the reference LSM; v2 is the application-specialized Rust realization of the same architecture.

The decision rests on the axes the application lives on — write-mixed throughput, tail latency, bounded memory, bounded recovery, disk economy, and a data model (identity/placement) that survives distribution — where v2 is at parity or decisively ahead, and on the one deficit axis being a measured, tunable trade inside the certified design gate rather than a design wall.

## 6. Roadmap — closing the read cells (each lever measured, each with a named milestone)

| order | lever | expected | evidence so far |
|---|---|---|---|
| 1 | Adopt `get_many` in the harness W4/W7 legs | ~13% p50 on the get sub-shape; segs/op 14 vs 116 | M27: 0.81/0.87/0.85× stable across runs |
| 2 | Block target 64 KiB → 16/4 KiB | ~4–8× on cold/warm point gets | M22 attribution: 18.7 µs of 33.5 µs is one 64 KiB fetch; redb's 4 KiB pages explain its entire W1 edge |
| 3 | Runtime L0→L1 merge already shipped; tune `merge_ratio` for scan cells | W5/history/versions 2–4× | M16: −76% merge I/O, −21% peak, gate held |
| 4 | Cache sizing for hot datasets | converges toward v1 by configuration, not redesign | gate 3 pins the knob works; M19 warm regime 37 µs |
| 5 | Paged placement directory (M28 spec §47) | metadata beyond RAM at 10M–100M objects | M38: 419 B/object measured today |

## 7. Risks and honest limits

- The warm-read gap stays ~6× v1 until levers 2–4 land; deployments that are pure read-hot and RAM-affordant should stay on `aikoql` until then. This is stated, not hidden: the amended gate is a design gate, and the 09-01 NOT ADOPT record stands until this ADR is ratified.
- RocksDB has no measured row here; the comparison against it is architectural. A rocksdb backend harness would be the honest way to convert that row from reasoning to evidence if it is ever wanted.
- `STORAGE-BACKENDS.md` has been updated with this ratification: v2 is the production default, redb is the opt-out compatibility fallback, and the auto-detection table records the missing-path rule.
