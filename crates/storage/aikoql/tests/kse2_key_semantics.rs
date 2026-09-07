//! KSE-2 — preserve AIKOQL key semantics (MRFC-KSE-001 §8).
//!
//! The repository layer owns the logical key schema; the engine is a raw KV
//! store. These tests run the real kernel over `AikoqlStorageEngine` and
//! assert the semantics at BOTH levels — kernel results AND the raw key
//! layout (`ko/`, `head/`, `tomb/`, `idem/`, `relo/`, `reli/`, `type/`) —
//! so a semantic divergence from the repository contract is caught, not
//! silently papered over by a kernel-level workaround.

use aikoql_kernel::storage::store::StorageEngine;
use aikoql_kernel::transaction::kernel::ManualClock;
use aikoql_kernel::{
    Direction, ForgetMode, Kernel, LifecycleState, RelationshipRef, RememberRequest, Subject,
};
use aikoql_storage::AikoqlStorageEngine;
use std::path::PathBuf;
use std::sync::Arc;

mod common;
use common::tmp;

fn alice() -> Subject {
    Subject::new("alice")
}

fn meta(t: &str) -> aikoql_kernel::Metadata {
    aikoql_kernel::Metadata {
        type_name: t.into(),
        tenant: None,
        schema_version: 1,
        tags: vec![],
    }
}

fn mk(tag: &str) -> (Kernel, Arc<ManualClock>, Arc<AikoqlStorageEngine>, PathBuf) {
    let p = tmp(tag);
    let clock = Arc::new(ManualClock::new(10_000));
    let store = Arc::new(AikoqlStorageEngine::open(&p).unwrap());
    let k = Kernel::open(store.clone(), clock.clone(), 0xC0FFEE).unwrap();
    (k, clock, store, p)
}

fn create(k: &Kernel, t: &str) -> aikoql_kernel::KOID {
    k.remember(RememberRequest::create(alice(), meta(t)))
        .unwrap()
        .koid
}

/// Last 8 bytes of a `ko/<koid>/<ts>` key: the commit timestamp (BE).
fn ts_of(key: &[u8]) -> u64 {
    u64::from_be_bytes(key[key.len() - 8..].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// KSE-010 — version ordering: versions scan ascending by commit_ts
// ---------------------------------------------------------------------------

#[test]
fn kse010_version_ordering() {
    let (k, clock, store, _p) = mk("kse010");
    let id = create(&k, "fact");
    clock.set(2_000);
    k.remember(RememberRequest::update(alice(), id, meta("fact")))
        .unwrap();
    clock.set(3_000);
    k.remember(RememberRequest::update(alice(), id, meta("fact")))
        .unwrap();

    // Kernel view: history is ascending.
    let hist = k.history(alice(), &id).unwrap();
    let versions: Vec<u64> = hist.iter().map(|(_, ko)| ko.version).collect();
    assert_eq!(versions, vec![1, 2, 3]);
    let tss: Vec<u64> = hist.iter().map(|(ts, _)| *ts).collect();
    assert!(tss.windows(2).all(|w| w[0] < w[1]), "ascending ts: {tss:?}");

    // Raw view: three ko/ versions, ordered by key = ordered by ts.
    let mut prefix = b"ko/".to_vec();
    prefix.extend_from_slice(id.as_bytes());
    let rows: Vec<(u64, Vec<u8>)> = store
        .scan(&prefix)
        .unwrap()
        .into_iter()
        .map(|(k, _)| (ts_of(&k), k))
        .collect();
    assert_eq!(rows.len(), 3);
    assert!(rows.windows(2).all(|w| w[0].0 < w[1].0));
}

// ---------------------------------------------------------------------------
// KSE-011 — current head: head/<koid> resolves to the latest valid version
// ---------------------------------------------------------------------------

#[test]
fn kse011_current_head() {
    let (k, clock, store, _p) = mk("kse011");
    let id = create(&k, "fact");
    clock.set(2_000);
    k.remember(RememberRequest::update(alice(), id, meta("fact")))
        .unwrap();

    // Raw view: exactly one head row for this KOID.
    let mut head = b"head/".to_vec();
    head.extend_from_slice(id.as_bytes());
    assert!(store.get(&head).unwrap().is_some());
    assert_eq!(store.scan(b"head/").unwrap().len(), 1);

    // Kernel view: get returns the latest version.
    let ko = k.get(alice(), &id).unwrap();
    assert_eq!(ko.version, 2);
}

// ---------------------------------------------------------------------------
// KSE-012 — historical read: given a timestamp, the correct version returns
// ---------------------------------------------------------------------------

#[test]
fn kse012_historical_read() {
    let (k, clock, _store, _p) = mk("kse012");
    let id = create(&k, "fact"); // v1 @ 10_000 (mk default)
    clock.set(20_000);
    k.remember(RememberRequest::update(alice(), id, meta("fact")))
        .unwrap(); // v2 @ 20_000
    clock.set(30_000);
    k.remember(RememberRequest::update(alice(), id, meta("fact")))
        .unwrap(); // v3 @ 30_000

    assert_eq!(
        k.get_as_of(alice(), &id, 15_000).unwrap().unwrap().version,
        1
    );
    assert_eq!(
        k.get_as_of(alice(), &id, 20_000).unwrap().unwrap().version,
        2
    );
    assert_eq!(
        k.get_as_of(alice(), &id, 25_000).unwrap().unwrap().version,
        2
    );
    assert_eq!(
        k.get_as_of(alice(), &id, 30_000).unwrap().unwrap().version,
        3
    );
    assert_eq!(k.get_as_of(alice(), &id, 9_000).unwrap(), None);
}

// ---------------------------------------------------------------------------
// KSE-013 — tombstone: deleted objects never appear as current active objects
// ---------------------------------------------------------------------------

#[test]
fn kse013_tombstone() {
    let (k, clock, store, _p) = mk("kse013");
    // Tombstone mode: a new Deleted version is committed; versions are
    // retained and the head still points at the tombstoned state.
    let id = create(&k, "fact");
    clock.set(2_000);
    k.remember(RememberRequest::update(alice(), id, meta("fact")))
        .unwrap();
    k.forget(alice(), &id, ForgetMode::Tombstone, None, None)
        .unwrap();

    let mut prefix = b"ko/".to_vec();
    prefix.extend_from_slice(id.as_bytes());
    assert_eq!(store.scan(&prefix).unwrap().len(), 3); // v1, v2, v3-Deleted
    let mut head = b"head/".to_vec();
    head.extend_from_slice(id.as_bytes());
    assert!(store.get(&head).unwrap().is_some());
    assert_eq!(
        store.scan(b"tomb/").unwrap().len(),
        0,
        "tomb/ is Erase-only"
    );

    // Kernel view: current state is Deleted, and history skips it.
    assert_eq!(
        k.get(alice(), &id).unwrap().lifecycle.state,
        LifecycleState::Deleted
    );
    let hist = k.history(alice(), &id).unwrap();
    assert!(hist
        .iter()
        .all(|(_, ko)| ko.lifecycle.state != LifecycleState::Deleted));
    assert_eq!(hist.last().unwrap().1.version, 2);

    // Erase mode: versions + head removed; the hash-only tomb/ stub keeps the
    // audit chain verifiable (prove) without the payload.
    let victim = create(&k, "secret");
    k.forget(alice(), &victim, ForgetMode::Erase, None, None)
        .unwrap();
    let mut tomb = b"tomb/".to_vec();
    tomb.extend_from_slice(victim.as_bytes());
    assert!(store.get(&tomb).unwrap().is_some());
    assert!(matches!(
        k.get(alice(), &victim),
        Err(aikoql_kernel::KError::NotFound(_))
    ));
}

// ---------------------------------------------------------------------------
// KSE-014 — idempotency: one idempotency key commits one logical operation
// ---------------------------------------------------------------------------

#[test]
fn kse014_idempotency() {
    let (k, _clock, store, _p) = mk("kse014");
    let mut req = RememberRequest::create(alice(), meta("fact"));
    req.idempotency_key = Some("req-kse2-1".into());
    let r1 = k.remember(req.clone()).unwrap();
    let r2 = k.remember(req).unwrap();
    assert_eq!(r1, r2, "retry must return the original commit");
    assert_eq!(k.journal().unwrap().len(), 1, "one logical op, one event");

    // Raw view: the idempotency row exists.
    assert!(store.get(b"idem/req-kse2-1").unwrap().is_some());
}

// ---------------------------------------------------------------------------
// KSE-015/016 — outbound + inbound relationship indexes
// ---------------------------------------------------------------------------

#[test]
fn kse015_016_relationship_indexes() {
    let (k, _clock, store, _p) = mk("kse015");
    let b = create(&k, "dst");
    let mut req = RememberRequest::create(alice(), meta("src"));
    req.relationships.push(RelationshipRef {
        rel_type: "cites".into(),
        target: b,
        direction: Direction::Outbound,
    });
    let a = k.remember(req).unwrap().koid;

    // Raw view: outbound row under relo/<A>/cites/<B>, inbound under reli/<B>/cites/<A>.
    let mut relo = b"relo/".to_vec();
    relo.extend_from_slice(a.as_bytes());
    relo.extend_from_slice(b"/cites/");
    relo.extend_from_slice(b.as_bytes());
    assert!(store.get(&relo).unwrap().is_some());
    let mut reli = b"reli/".to_vec();
    reli.extend_from_slice(b.as_bytes());
    reli.extend_from_slice(b"/cites/");
    reli.extend_from_slice(a.as_bytes());
    assert!(store.get(&reli).unwrap().is_some());

    // Kernel view: discoverable from A outbound and from B inbound.
    assert_eq!(
        k.outbound_edges(&a, None).unwrap(),
        vec![("cites".into(), b)]
    );
    assert_eq!(
        k.inbound_edges(&b, None).unwrap(),
        vec![("cites".into(), a)]
    );
}

// ---------------------------------------------------------------------------
// KSE-017 — type index: type scans return the correct live candidates
// ---------------------------------------------------------------------------

#[test]
fn kse017_type_index() {
    let (k, _clock, store, _p) = mk("kse017");
    let f1 = create(&k, "fact");
    let f2 = create(&k, "fact");
    let n1 = create(&k, "note");

    let facts = k.scan_by_type(&alice(), "fact").unwrap();
    assert_eq!(facts.len(), 2);
    let koids: Vec<aikoql_kernel::KOID> = facts.into_iter().map(|ko| ko.koid).collect();
    assert!(koids.contains(&f1) && koids.contains(&f2) && !koids.contains(&n1));

    // Raw view: type/<type>/<koid> rows for the indexed candidates only.
    let mut f1_key = b"type/fact/".to_vec();
    f1_key.extend_from_slice(f1.as_bytes());
    let mut n1_key = b"type/note/".to_vec();
    n1_key.extend_from_slice(n1.as_bytes());
    assert!(store.get(&f1_key).unwrap().is_some());
    assert_eq!(store.scan(b"type/fact/").unwrap().len(), 2);
    assert!(store.get(&n1_key).unwrap().is_some());
}

// ---------------------------------------------------------------------------
// KSE-2 durability: every semantic above survives a reopen (the WAL replays)
// ---------------------------------------------------------------------------

#[test]
fn kse2_semantics_survive_reopen() {
    let (k, clock, store, p) = mk("reopen");
    let id = create(&k, "fact");
    clock.set(2_000);
    k.remember(RememberRequest::update(alice(), id, meta("fact")))
        .unwrap();
    let b = create(&k, "dst");
    let mut req = RememberRequest::create(alice(), meta("src"));
    req.relationships.push(RelationshipRef {
        rel_type: "cites".into(),
        target: b,
        direction: Direction::Outbound,
    });
    let a = k.remember(req).unwrap().koid;
    let mut req2 = RememberRequest::create(alice(), meta("fact"));
    req2.idempotency_key = Some("req-kse2-reopen".into());
    k.remember(req2).unwrap();
    k.forget(alice(), &id, ForgetMode::Tombstone, None, None)
        .unwrap();
    drop((k, store, clock));

    // Reopen from the same WAL file: versions, head, indexes, idempotency all
    // intact.
    let clock2 = Arc::new(ManualClock::new(10_000));
    let store2 = Arc::new(AikoqlStorageEngine::open(&p).unwrap());
    let k2 = Kernel::open(store2.clone(), clock2.clone(), 0xC0FFEE).unwrap();
    assert_eq!(
        k2.get(alice(), &id).unwrap().lifecycle.state,
        LifecycleState::Deleted
    );
    assert_eq!(k2.history(alice(), &id).unwrap().len(), 2); // v1, v2 (v3 tombstone skipped)
    assert_eq!(
        k2.outbound_edges(&a, None).unwrap(),
        vec![("cites".into(), b)]
    );
    assert_eq!(
        k2.inbound_edges(&b, None).unwrap(),
        vec![("cites".into(), a)]
    );
    // Live facts only: id is tombstoned, the idem-req fact survives.
    assert_eq!(k2.scan_by_type(&alice(), "fact").unwrap().len(), 1);
    // v1 committed at exactly 10_000 (HLC counter 0); v2 landed at +1.
    assert_eq!(
        k2.get_as_of(alice(), &id, 10_000).unwrap().unwrap().version,
        1
    );
    let _ = std::fs::remove_file(&p);
}
