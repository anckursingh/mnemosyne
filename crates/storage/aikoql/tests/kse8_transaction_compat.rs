//! KSE-8 — transaction compatibility (MRFC-KSE-001 §14).
//!
//! KSE-070..074: atomic multi-KO commit, rollback, OCC conflict,
//! independent transactions, and snapshot read — the kernel's transaction
//! semantics exercised over MemoryEngine (reference), redb, and
//! AikoqlStorageEngine. The engine's job is to make the kernel's guarantees
//! hold; the gate is that every scenario produces the SAME observable
//! outcome on every backend, and that the outcome matches the documented
//! kernel contract (pinned explicitly, not just by parity).
//!
//! The engine-level atomicity instrument is the shared CountingEngine:
//! an aborted batch must commit ZERO write_batches and ZERO new rows.

mod common;

use aikoql_kernel::storage::store::{MemoryEngine, StorageEngine};
use aikoql_kernel::storage::store_redb::RedbEngine;
use aikoql_kernel::transaction::kernel::ManualClock;
use aikoql_kernel::{
    Clock, Direction, KError, Kernel, Metadata, RelationshipRef, RememberRequest, Subject,
    TransactionOp,
};
use aikoql_storage::AikoqlStorageEngine;
use common::{tmp, CountingEngine, LogicalCounts};
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

/// Everything the five scenarios observed on one backend. PartialEq lets the
/// harness pin cross-backend parity against the MemoryEngine reference.
#[derive(Debug, PartialEq)]
struct Summary {
    // KSE-070: atomic multi-KO commit (customer + account + relationship).
    atomic_versions: Vec<u64>,
    atomic_journal: usize,
    atomic_batches: u64, // write_batches committed by the one transact
    // KSE-071: rollback — no partial logical state.
    rollback_err: String,
    rollback_batches: u64, // zero committed batches during the abort
    rollback_rows: u64,    // zero new ko/ rows during the abort
    // KSE-072: OCC conflict — winner/conflict per kernel contract.
    occ_winner_version: u64,
    occ_conflict_found: u64,
    // KSE-073: independent transactions — conflict on A must not leak to B.
    indep_b_version: u64,
    // KSE-074: snapshot read — pinned snapshot stays stable across commits.
    snapshot_stable: bool,
    snapshot_head_version: u64,
}

fn scenarios(name: &'static str, engine: Arc<dyn StorageEngine>) -> Summary {
    let counting = CountingEngine::new(engine);
    let clock = Arc::new(ManualClock::new(10_000));
    let k = Kernel::open(counting.clone(), clock.clone(), SALT).unwrap();

    // KSE-070 — customer + account in ONE logical txn, then a relationship
    // KO pointing at one of them.
    let batches0 = LogicalCounts::writes(&counting).0;
    let customer = k
        .transact(vec![
            TransactionOp::new(alice(), RememberRequest::create(alice(), meta("customer"))),
            TransactionOp::new(alice(), RememberRequest::create(alice(), meta("account"))),
        ])
        .unwrap();
    let atomic_batches = LogicalCounts::writes(&counting).0 - batches0;
    assert_eq!(customer.len(), 2, "{name}: atomic txn size");
    let atomic_versions: Vec<u64> = customer.iter().map(|r| r.version).collect();
    let rel_req = {
        let mut req = RememberRequest::create(alice(), meta("relationship"));
        req.relationships.push(RelationshipRef {
            rel_type: "owns".into(),
            target: customer[1].koid,
            direction: Direction::Outbound,
        });
        req
    };
    k.remember(rel_req).unwrap();
    let atomic_journal = k.journal().unwrap().len();
    assert!(k.get(alice(), &customer[0].koid).is_ok());
    assert!(k.get(alice(), &customer[1].koid).is_ok());

    // KSE-071 — rollback: stale op first (fails phase-1 OCC), valid create
    // second — the whole batch must abort with nothing durable.
    let mut stale = RememberRequest::update(alice(), customer[0].koid, meta("customer"));
    stale.expected_version = Some(0);
    let fresh = RememberRequest::create(alice(), meta("orphan"));
    let rows_before = counting.inner.scan(b"ko/").unwrap().len() as u64;
    let batches_before = LogicalCounts::writes(&counting).0;
    let rollback_err = match k.transact(vec![
        TransactionOp::new(alice(), stale),
        TransactionOp::new(alice(), fresh),
    ]) {
        Ok(_) => "unexpected-ok".into(),
        Err(KError::VersionConflict { found, .. }) => format!("conflict:{found}"),
        Err(e) => format!("other:{e:?}"),
    };
    let rollback_batches = LogicalCounts::writes(&counting).0 - batches_before;
    let rollback_rows = counting.inner.scan(b"ko/").unwrap().len() as u64 - rows_before;

    // KSE-072 — OCC: winner commits, stale second update conflicts.
    let mut w = RememberRequest::update(alice(), customer[0].koid, meta("customer"));
    w.expected_version = Some(1);
    k.transact(vec![TransactionOp::new(alice(), w)]).unwrap();
    let mut c = RememberRequest::update(alice(), customer[0].koid, meta("customer"));
    c.expected_version = Some(1); // stale
    let occ_conflict_found = match k.transact(vec![TransactionOp::new(alice(), c)]) {
        Err(KError::VersionConflict { found, .. }) => found,
        other => panic!("{name}: expected VersionConflict, got {other:?}"),
    };
    let occ_winner_version = k.get(alice(), &customer[0].koid).unwrap().version;

    // KSE-073 — a conflict on A must not block an update to unrelated B
    // (same wall instant: the HLC counter, not the wall clock, separates).
    let mut b = RememberRequest::update(alice(), customer[1].koid, meta("account"));
    b.expected_version = Some(1);
    let indep = k.transact(vec![TransactionOp::new(alice(), b)]).unwrap();
    let indep_b_version = indep[0].version;

    // KSE-074 — snapshot read: pin as-of this wall instant, commit three
    // more versions; the pinned view must not move while the head advances.
    let snap_millis = clock.millis() + 1;
    let asof_before = k
        .get_as_of(alice(), &customer[1].koid, snap_millis)
        .unwrap()
        .unwrap();
    for _ in 0..3 {
        clock.tick(10_000);
        k.remember(RememberRequest::update(
            alice(),
            customer[1].koid,
            meta("account"),
        ))
        .unwrap();
    }
    let asof_after = k
        .get_as_of(alice(), &customer[1].koid, snap_millis)
        .unwrap()
        .unwrap();
    let snapshot_stable = asof_before.version == asof_after.version
        && asof_before.commit_ts == asof_after.commit_ts
        && asof_before.properties == asof_after.properties;
    let snapshot_head_version = k.get(alice(), &customer[1].koid).unwrap().version;

    Summary {
        atomic_versions,
        atomic_journal,
        atomic_batches,
        rollback_err,
        rollback_batches,
        rollback_rows,
        occ_winner_version,
        occ_conflict_found,
        indep_b_version,
        snapshot_stable,
        snapshot_head_version,
    }
}

/// KSE-070..074 — kernel transaction semantics over the three backends.
#[test]
fn kse070_074_transaction_compat() {
    let memory = scenarios("memory", Arc::new(MemoryEngine::new()));
    let redb_p = tmp("kse8_redb");
    let redb = scenarios("redb", Arc::new(RedbEngine::open(&redb_p).unwrap()));
    let aikoql_p = tmp("kse8_aikoql");
    let aikoql = scenarios(
        "aikoql",
        Arc::new(AikoqlStorageEngine::open(&aikoql_p).unwrap()),
    );

    // Parity pins: every observable outcome identical to the reference.
    assert_eq!(redb, memory, "redb diverged from the memory reference");
    assert_eq!(aikoql, memory, "aikoql diverged from the memory reference");

    // Contract pins (documented kernel semantics, not just parity).
    assert_eq!(memory.atomic_versions, vec![1, 1]);
    assert_eq!(memory.atomic_journal, 3);
    assert_eq!(memory.atomic_batches, 1, "transact must commit one batch");
    assert_eq!(memory.rollback_err, "conflict:1");
    assert_eq!(memory.rollback_batches, 0, "aborted txn committed a batch");
    assert_eq!(memory.rollback_rows, 0, "aborted txn wrote rows");
    assert_eq!(memory.occ_winner_version, 2);
    assert_eq!(memory.occ_conflict_found, 2);
    assert_eq!(memory.indep_b_version, 2, "conflict leaked across KOs");
    assert!(memory.snapshot_stable, "pinned snapshot moved");
    assert_eq!(
        memory.snapshot_head_version, 5,
        "head must advance past snapshot"
    );

    for p in [&redb_p, &aikoql_p] {
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_dir_all(p);
    }
}
