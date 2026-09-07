# KSE-19 — Resource Usage (MRFC-KSE-001 §25)

Date: 2026-09-01 · seed 0x190000 · engine: AikoqlStorageEngine · build profile: release · sizes run in this suite run: 100000/1000000

| KOs | build (wall≈CPU) | peak RSS | heap (live store) | index memory | disk (WAL) | bytes/KO (store) | bytes/KO (disk) |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 100000 | 110383 ms | 423882752 B | 64388970 B | 6800000 B | 76688941 B | 644 | 767 |
| 1000000 | 1242211 ms | 4127371264 B | 644888971 B | 68000000 B | 767888942 B | 645 | 768 |

10M projection (linear in N, from the 1000000 row): heap ≈ 6.45 GB, disk ≈ 7.68 GB — the engine holds the ENTIRE graph in a BTreeMap and replays 100% of the WAL at open (KSE-15), so memory is linear by design. §25 verdict evidence: it DOES require the whole graph in memory — ~645 B/KO.

## Honest metric mapping

- RSS: sampled every 500 ms on the loader child (WorkingSet64) — the peak is a LOWER BOUND (spikes between samples are missed); includes the loader process itself
- heap: the live store bytes (Σ k+v of every row) — the BTreeMap mirror IS the heap; node overhead adds a constant factor not counted here
- cache memory: 0 by construction — there is no cache layer; the store itself is in RAM
- index memory: the derived-index rows (head/type/relo/reli) — a subset of the store, not a separate structure
- peak allocation: NOT_MEASURED (no allocator tracing wired)
- disk: WAL bytes at load end (fsync per commit — KSE-15)
- CPU: build wall time, single-threaded (wall ≈ CPU); debug build inflates instruction cost, not memory

## Honest limits

- single-writer child; RSS of concurrent access (multi-reader) not measured here (KSE-13 covers behavior, not memory)
- the 10M row is a projection, not a run — running 10M is a machine choice, not a gate
- peak RSS spans the WHOLE load including the end-of-load pins (full-store materialization scans + per-head lineage decode) — transient allocations that exist only at the tail of the run, so the peak overstates steady-state ingest RSS
- RSS is Windows-only (PowerShell WorkingSet64 poll); CI (ubuntu) rows carry heap/disk without RSS
- KOID identity at 1M relies on the HLC counter (clock ticks 1 ms per commit here); same-instant commits are pinned by KSE-8
