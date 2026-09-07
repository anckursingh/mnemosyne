# KSE-15 — Startup and Recovery (KSE-140..141)

Date: 2026-08-31 · seed 0x150000 · engine: AikoqlStorageEngine · debug build, measurements from this suite run

## KSE-140 — cold startup (2,000 KOs + 200 updates + 1 schema; 2200 version rows, 2200 journal events, 1500587 B WAL)

| stage | time |
|---|---:|
| open (WAL replay) | 92.24 ms (41.9 µs/row) |
| metadata initialization (kernel open: type-index marker check + schema reload) | 23.06 ms |
| index initialization | 0 — derived indexes (relo/reli/type) are WAL rows, replayed with open; nothing is built at startup |
| ready (first query) | 22.1 µs |

## KSE-141 — crash recovery (real kill after ≥300 durable commits; recovered 302, 205298 B WAL at recovery)

| stage | time |
|---|---:|
| crash → restart | process kill, instant |
| recovery (open: WAL replay + torn-tail truncation) | 15.69 ms |
| kernel open | 3.58 ms |
| first successful query | 3752.6 µs |

## Pins

- KSE-140: cold open serves the byte-exact pre-close store; type scan == 2,000; the updated KO kept its 2-version lineage; structural sweep + rebuild (0,0)
- KSE-141: recovered seqs exactly 1..=n (no lost or phantom middle commits — append-only replay can only drop a torn tail); KOIDs unique; seq-1 KO whole (version 1, one event); structural sweep + rebuild (0,0)

## Honest limits

- "index initialization" has no cost for this engine by construction — derived indexes are store rows; the only startup index work is the one-time R9 backfill for pre-R9 databases, a no-op on a fresh marker and not part of this measurement
- the kill lands at an arbitrary instant — the torn-tail truncation path is pinned deterministically by KSE-9 (fault injection); here the replay handled a real kill
- the two first-query numbers are not directly comparable: KSE-141's is the full type scan (all recovered KOs decoded), KSE-140's is a point get
- child is single-writer (the kernel pipeline is single-writer by design)
