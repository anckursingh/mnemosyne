//! KSE-10 — derived index rebuild (MRFC-KSE-001 §16).
//!
//! Canonical state must remain authoritative: the relo/reli/type rows are
//! derived indexes over ko/ heads. KSE-090 full rebuild reproduces the exact
//! derived key set AND equivalent logical query results; KSE-091 deleting
//! ~10% of derived rows and rebuilding restores complete correctness;
//! KSE-092 corrupt rows are detected (malformed keys fail queries closed;
//! stale rows are swept by the rebuild and reported) — never silent
//! incorrect knowledge after repair.
//!
//! The rebuild itself is a kernel op over `&dyn StorageEngine` (canonical
//! ko/ rows are the authority), so every backend gets the same gate.

mod common;

use aikoql_kernel::kernel::DerivedIndexRebuild;
use aikoql_kernel::storage::store::{MemoryEngine, StorageEngine, WriteBatch};
use aikoql_kernel::storage::store_redb::RedbEngine;
use aikoql_kernel::transaction::kernel::ManualClock;
use aikoql_kernel::{
    Direction, ForgetMode, Kernel, Metadata, RelationshipRef, RememberRequest, Subject,
};
use aikoql_storage::AikoqlStorageEngine;
use common::tmp;
use std::collections::BTreeSet;
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

fn derived_keys(engine: &dyn StorageEngine) -> BTreeSet<Vec<u8>> {
    [
        b"relo/".as_slice(),
        b"reli/".as_slice(),
        b"type/".as_slice(),
    ]
    .into_iter()
    .flat_map(|p| engine.scan(p).unwrap())
    .map(|(k, _v)| k)
    .collect()
}

/// Everything KSE-090..092 observed on one backend. PartialEq lets the
/// harness pin cross-backend parity against the MemoryEngine reference.
#[derive(Debug, PartialEq)]
struct Summary {
    golden_len: usize,                         // derived rows maintained incrementally
    full_restored: bool,                       // KSE-090: exact set + query equality
    full_report: (usize, usize, usize, usize), // heads, relo, reli, type
    partial_restored: bool,                    // KSE-091: ~10% loss repaired exactly
    malformed_fails_closed: bool,              // KSE-092: bad key errors the query
    ghost_visible: bool,                       // honest: stale row served pre-rebuild
    corrupt_report: (usize, usize),            // removed_stale, removed_invalid
    corrupt_restored: bool,                    // KSE-092: repair restores the exact set
}

fn scenarios(name: &'static str, engine: Arc<dyn StorageEngine>) -> Summary {
    let clock = Arc::new(ManualClock::new(10_000));
    let k = Kernel::open(engine.clone(), clock, SALT).unwrap();

    // Seed: A→B owns, B→C uses, C→B uses, D→A points; D tombstoned — its
    // head survives and keeps the edge (canonical content is authoritative).
    let a = k
        .remember(RememberRequest::create(alice(), meta("customer")))
        .unwrap()
        .koid;
    let b = k
        .remember(RememberRequest::create(alice(), meta("account")))
        .unwrap()
        .koid;
    let c = k
        .remember(RememberRequest::create(alice(), meta("account")))
        .unwrap()
        .koid;
    let d = k
        .remember(RememberRequest::create(alice(), meta("account")))
        .unwrap()
        .koid;
    for (src, rel_type, dst) in [
        (&a, "owns", &b),
        (&b, "uses", &c),
        (&c, "uses", &b),
        (&d, "points", &a),
    ] {
        // same type as created — a changed type_name would legitimately
        // leave a stale type row behind (readers filter; rebuild sweeps)
        let mut req = RememberRequest::update(
            alice(),
            *src,
            meta(if *src == a { "customer" } else { "account" }),
        );
        req.relationships.push(RelationshipRef {
            rel_type: rel_type.into(),
            target: *dst,
            direction: Direction::Outbound,
        });
        k.remember(req).unwrap();
    }
    k.forget(alice(), &d, ForgetMode::Tombstone, None, None)
        .unwrap();

    let golden = derived_keys(engine.as_ref());
    let golden_len = golden.len();
    assert_eq!(golden_len, 12, "{name}: 4 relo + 4 reli + 4 type");
    let golden_out_a = k.outbound_edges(&a, None).unwrap();
    let golden_in_b = k.inbound_edges(&b, None).unwrap();

    // KSE-090 — full rebuild: drop every derived row, rebuild, compare.
    let mut wipe = WriteBatch::new();
    for key in &golden {
        wipe.del(key.clone());
    }
    engine.write_batch(&wipe).unwrap();
    assert!(
        k.outbound_edges(&a, None).unwrap().is_empty(),
        "{name}: index down must return nothing, not stale rows"
    );
    let full_report: DerivedIndexRebuild = k.rebuild_derived_indexes().unwrap();
    let full_restored = derived_keys(engine.as_ref()) == golden
        && k.outbound_edges(&a, None).unwrap() == golden_out_a
        && k.inbound_edges(&b, None).unwrap() == golden_in_b;
    let full_report = (
        full_report.heads_scanned,
        full_report.relo_rows,
        full_report.reli_rows,
        full_report.type_rows,
    );

    // KSE-091 — partial loss: delete ~10% (1 of 12) of derived rows.
    let victim = golden.iter().nth(5).unwrap();
    let mut lose = WriteBatch::new();
    lose.del(victim.clone());
    engine.write_batch(&lose).unwrap();
    assert!(
        derived_keys(engine.as_ref()) != golden,
        "{name}: partial loss must actually lose a row"
    );
    k.rebuild_derived_indexes().unwrap();
    let partial_restored = derived_keys(engine.as_ref()) == golden;

    // KSE-092 — corrupt index: a malformed relo key under C's prefix (fails
    // decode → the query errors closed) and a stale ghost edge under A's
    // prefix (decodes, references nothing canonical).
    let mut plant = WriteBatch::new();
    let mut malformed = Vec::new();
    malformed.extend_from_slice(b"relo/");
    malformed.extend_from_slice(c.as_bytes());
    malformed.push(b'/');
    malformed.extend_from_slice(b"BAD"); // too short to decode
    let mut ghost = Vec::new();
    ghost.extend_from_slice(b"relo/");
    ghost.extend_from_slice(a.as_bytes());
    ghost.push(b'/');
    ghost.extend_from_slice(b"ghost/");
    ghost.extend_from_slice(b.as_bytes());
    plant.put(malformed, vec![]);
    plant.put(ghost, vec![]);
    engine.write_batch(&plant).unwrap();

    let malformed_fails_closed = k.outbound_edges(&c, None).is_err();
    let ghost_visible = k
        .outbound_edges(&a, None)
        .unwrap()
        .iter()
        .any(|(t, dst)| t == "ghost" && *dst == b);
    let report: DerivedIndexRebuild = k.rebuild_derived_indexes().unwrap();
    let corrupt_restored = derived_keys(engine.as_ref()) == golden
        && k.outbound_edges(&a, None).unwrap() == golden_out_a
        && k.outbound_edges(&c, None).is_ok();

    Summary {
        golden_len,
        full_restored,
        full_report,
        partial_restored,
        malformed_fails_closed,
        ghost_visible,
        corrupt_report: (report.removed_stale, report.removed_invalid),
        corrupt_restored,
    }
}

/// KSE-090..092 — derived index rebuild over the three backends.
#[test]
fn kse090_092_index_rebuild() {
    let memory = scenarios("memory", Arc::new(MemoryEngine::new()));
    let redb_p = tmp("kse10_redb");
    let redb = scenarios("redb", Arc::new(RedbEngine::open(&redb_p).unwrap()));
    let aikoql_p = tmp("kse10_aikoql");
    let aikoql = scenarios(
        "aikoql",
        Arc::new(AikoqlStorageEngine::open(&aikoql_p).unwrap()),
    );

    // Parity: every observable outcome identical to the reference.
    assert_eq!(redb, memory, "redb diverged from the memory reference");
    assert_eq!(aikoql, memory, "aikoql diverged from the memory reference");

    // Contract pins (KSE-090..092 gates, not just parity).
    assert_eq!(memory.golden_len, 12, "4 edges × 2 + 4 type rows");
    assert!(memory.full_restored, "KSE-090: full rebuild not exact");
    assert_eq!(
        memory.full_report,
        (4, 4, 4, 4),
        "heads scanned + 4 edges × 2 + 4 type rows"
    );
    assert!(
        memory.partial_restored,
        "KSE-091: partial loss not repaired"
    );
    assert!(
        memory.malformed_fails_closed,
        "KSE-092: malformed derived key must fail the query closed"
    );
    assert!(
        memory.ghost_visible,
        "honest pin: a stale row IS served until rebuilt"
    );
    assert_eq!(memory.corrupt_report, (1, 1), "detection counts");
    assert!(memory.corrupt_restored, "KSE-092: repair not exact");

    for p in [&redb_p, &aikoql_p] {
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_dir_all(p);
    }
}
