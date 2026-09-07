//! KSE-9 — crash consistency (MRFC-KSE-001 §15).
//!
//! Storage-level fault injection against the AikoqlStorageEngine WAL. The
//! doc's eight fault points map onto the WAL's real crash surface:
//!
//! - before append          → record absent from the file (truncate at the
//!   pre-batch boundary)
//! - after append           → complete record present (crash = kill+reopen)
//! - before flush           → inside write_batch (the engine fsyncs before
//!   returning) — no external boundary; covered by the same two shapes
//! - after flush            → same as after append
//! - before commit marker   → torn tail (record cut short)
//! - after commit marker    → complete + checksummed record
//! - before/after index     → no separate index-publication step: relo/reli
//!   publication            rows are atoms of the same batch as ko/ rows
//!
//! Gates: KSE-080 crash before commit = old state, no partial rows; KSE-081
//! crash after commit = committed state survives restart; KSE-082 crash
//! during index update = canonical knowledge valid + index divergence is
//! unreachable by construction (whole-record granularity; byte damage fails
//! closed); KSE-083 recovery = truncation heals, corruption errors, never
//! silent wrong data.
//!
//! redb/RocksDB crash recovery is vendor-owned — NOT_MEASURED here.

mod common;

use aikoql_kernel::transaction::kernel::ManualClock;
use aikoql_kernel::{
    Direction, Kernel, Metadata, ReferentialPolicy, RelationshipRef, RememberRequest, Subject,
    TransactionOp,
};
use aikoql_storage::envelope::{parse_at, ParseOutcome};
use aikoql_storage::AikoqlStorageEngine;
use common::tmp;
use std::path::Path;
use std::sync::Arc;

const SALT: u64 = 0xC0FFEE;

fn alice() -> Subject {
    Subject::new("alice")
}

fn meta(t: &str) -> Metadata {
    Metadata {
        type_name: t.into(),
        tenant: None,
        schema_version: 1,
        tags: vec![],
    }
}

fn reopen(p: &Path) -> Kernel {
    let engine = AikoqlStorageEngine::open(p).unwrap();
    Kernel::open(Arc::new(engine), Arc::new(ManualClock::new(10_000)), SALT).unwrap()
}

/// (start, end) of every complete record in the WAL.
fn record_offsets(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut offs = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        match parse_at(bytes, at).unwrap() {
            ParseOutcome::Complete { end, .. } => {
                offs.push((at, end));
                at = end;
            }
            ParseOutcome::TornTail => break,
        }
    }
    offs
}

/// Each field: (description-relevant state tuple) per fault class.
struct Summary {
    committed: (u64, usize, usize), // (A version, outbound(A), inbound(B)) — before crash
    pre_b2: (u64, bool, usize),     // KSE-080: (A version, B exists, outbound(A))
    after_commit: (u64, bool, usize, bool), // KSE-081: + inbound(B) has A
    torn: (u64, bool, bool),        // F3: (A version, B exists, tail truncated)
    corrupt: bool,                  // F4: reopen failed closed
    untouched: bool,                // F4: failed open left the file alone
    healed: (u64, bool),            // KSE-083: truncate the bad record, reopen
}

fn run(p: &Path) -> Summary {
    // Seed: batch 1 = create A; batch 2 = transact(create B + update A with
    // edge A→B) — ko/, relo/, reli/ rows are atoms of the same record. B's
    // KOID is pre-assigned so the edge can point at it intra-batch (strict
    // referential policy resolves batch-internal targets).
    let k = reopen(p);
    let a = k
        .remember(RememberRequest::create(alice(), meta("customer")))
        .unwrap()
        .koid;
    let b = aikoql_kernel::KOID([0xbb; aikoql_kernel::KOID_LEN]);
    let mut b_req = RememberRequest::create(alice(), meta("account"));
    b_req.koid = Some(b);
    b_req.expected_version = Some(0);
    b_req.referential_policy = ReferentialPolicy::Strict;
    let mut upd = RememberRequest::update(alice(), a, meta("customer"));
    upd.relationships.push(RelationshipRef {
        rel_type: "owns".into(),
        target: b,
        direction: Direction::Outbound,
    });
    let res = k
        .transact(vec![
            TransactionOp::new(alice(), b_req),
            TransactionOp::new(alice(), upd),
        ])
        .unwrap();
    assert_eq!(res[0].koid, b, "pre-assigned KOID must be honored");

    let committed = (
        k.get(alice(), &a).unwrap().version,
        k.outbound_edges(&a, None).unwrap().len(),
        k.inbound_edges(&b, None).unwrap().len(),
    );
    drop(k);

    let bytes = std::fs::read(p).unwrap();
    let offs = record_offsets(&bytes);
    // bootstrap record + create A + transact(A→B with B created intra-batch)
    assert_eq!(offs.len(), 3, "expected bootstrap + create + transact");

    // F1 — KSE-080 crash before commit: the last record never made it.
    std::fs::write(p, &bytes[..offs[2].0]).unwrap();
    let k = reopen(p);
    let pre_b2 = (
        k.get(alice(), &a).unwrap().version,
        k.get(alice(), &b).is_ok(),
        k.outbound_edges(&a, None).unwrap().len(),
    );
    drop(k);

    // F2 — KSE-081 crash after commit: full file, kill, reopen.
    std::fs::write(p, &bytes).unwrap();
    let k = reopen(p);
    let after_commit = (
        k.get(alice(), &a).unwrap().version,
        k.get(alice(), &b).is_ok(),
        k.outbound_edges(&a, None).unwrap().len(),
        k.inbound_edges(&b, None)
            .unwrap()
            .iter()
            .any(|(t, src)| t == "owns" && *src == a),
    );
    drop(k);

    // F3 — torn tail (crash mid-append): last record cut short.
    std::fs::write(p, &bytes[..bytes.len() - 10]).unwrap();
    let k = reopen(p); // engine truncates the tail at open
    let torn = (
        k.get(alice(), &a).unwrap().version,
        k.get(alice(), &b).is_ok(),
        std::fs::metadata(p).unwrap().len() == offs[2].0 as u64,
    );
    drop(k);

    // F4 — corruption inside the last record's payload: fail closed.
    let mut bad = bytes.clone();
    bad[offs[2].0 + 20] ^= 0xFF;
    std::fs::write(p, &bad).unwrap();
    let corrupt = AikoqlStorageEngine::open(p).is_err();
    let untouched = std::fs::read(p).unwrap() == bad;

    // F5 — KSE-083 recovery: truncate the bad record, clean reopen.
    std::fs::write(p, &bytes[..offs[2].0]).unwrap();
    let k = reopen(p);
    let healed = (
        k.get(alice(), &a).unwrap().version,
        k.get(alice(), &b).is_ok(),
    );
    drop(k);

    Summary {
        committed,
        pre_b2,
        after_commit,
        torn,
        corrupt,
        untouched,
        healed,
    }
}

/// KSE-080..083 — WAL fault injection and recovery.
#[test]
fn kse080_083_crash_consistency() {
    let p = tmp("kse9");
    let s = run(&p);

    // KSE-081: the committed state is exactly what survives a clean restart
    // — the pre-crash in-memory view must equal the reopened view.
    assert_eq!(s.after_commit, (2, true, 1, true), "committed state lost");
    assert_eq!(s.committed.0, s.after_commit.0, "version diverged");
    assert_eq!(s.committed.1, s.after_commit.2, "outbound diverged");
    assert_eq!(s.committed.2, 1, "inbound diverged");
    // KSE-080: losing the last record returns to the pre-batch state — no
    // partial rows, no phantom edges.
    assert_eq!(
        s.pre_b2,
        (1, false, 0),
        "pre-commit crash left partial state"
    );
    // Torn tail: same pre-batch state, and the tail was actually truncated.
    assert_eq!(s.torn, (1, false, true), "torn tail mishandled");
    // KSE-082: byte damage fails closed and never mutates the file — the
    // record format has no partial-index failure mode (ko/relo/reli are one
    // checksummed record).
    assert!(s.corrupt, "corruption did not fail closed");
    assert!(s.untouched, "failed open modified the WAL");
    // KSE-083: recovery by truncation restores the last good state.
    assert_eq!(s.healed, (1, false), "recovery state wrong");

    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_dir_all(&p);
}
