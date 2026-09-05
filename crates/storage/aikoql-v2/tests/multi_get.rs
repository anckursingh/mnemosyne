//! SE2-M25 — batch read path REDs (TESTING-PLAN-V2 row SE2-M25):
//! `get_many` answers byte-identical to the per-key loop, dedup accounting
//! (one lookup per unique target), and the kernel batch matches per-target
//! `get` incl. NotFound fail-fast.

mod common;

use aikoql_kernel::transaction::kernel::ManualClock;
use aikoql_kernel::{Kernel, Metadata, RememberRequest, Value, KOID, KOID_LEN};
use aikoql_storage_v2::db::{Config, Db};
use aikoql_storage_v2::AikoqlStorageEngineV2;
use common::{ctx, dir, stats_delta};
use std::sync::Arc;

fn meta(t: &str) -> Metadata {
    Metadata {
        type_name: t.into(),
        tenant: None,
        schema_version: 1,
        tags: vec![],
    }
}

#[test]
fn multi_get_answers_match_loop() {
    let db = Db::open(Config::new(dir("m25-answers"))).unwrap();
    // three generations across two flushes: segment-only keys, memtable
    // overwrites, tombstones in both layers, absent keys
    for i in 0..300u32 {
        db.put(
            format!("k{i:03}").as_bytes(),
            format!("v{i:03}-g0").as_bytes(),
        )
        .unwrap();
    }
    db.flush().unwrap();
    for i in 0..50u32 {
        db.put(
            format!("k{i:03}").as_bytes(),
            format!("v{i:03}-g1").as_bytes(),
        )
        .unwrap();
    }
    for i in 100..120u32 {
        db.delete(format!("k{i:03}").as_bytes()).unwrap();
    }
    db.flush().unwrap();
    for i in 200..210u32 {
        db.put(
            format!("k{i:03}").as_bytes(),
            format!("v{i:03}-g2").as_bytes(),
        )
        .unwrap();
    }
    for i in 50..60u32 {
        db.delete(format!("k{i:03}").as_bytes()).unwrap();
    }

    // query set: everything, plus absent keys, plus duplicates
    let mut keys: Vec<Vec<u8>> = (0..350u32)
        .map(|i| format!("k{i:03}").into_bytes())
        .collect();
    for i in [5u32, 7, 299] {
        keys.push(format!("k{i:03}").into_bytes());
    }
    let key_refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
    let expected: Vec<Option<Vec<u8>>> = key_refs.iter().map(|k| db.get(k).unwrap()).collect();
    let batch = db.get_many(&key_refs).unwrap();
    assert_eq!(batch, expected);
}

#[test]
fn multi_get_one_lookup_per_unique_target() {
    let db = Db::open(Config::new(dir("m25-dedup"))).unwrap();
    for i in 0..10u32 {
        db.put(format!("k{i:03}").as_bytes(), format!("v{i:03}").as_bytes())
            .unwrap();
    }
    db.flush().unwrap();

    let before = db.read_path_stats();
    let keys: Vec<&[u8]> = vec![
        b"k000", b"k001", b"k002", b"k000", b"k001", b"k002", b"k999", b"k000",
    ];
    let batch = db.get_many(&keys).unwrap();
    let delta = stats_delta(db.read_path_stats(), before);

    // 8 positions, 4 unique keys (k000, k001, k002, k999) — dedup counts once
    assert_eq!(delta.lookups, 4);
    assert_eq!(batch[0], Some(b"v000".to_vec()));
    assert_eq!(batch[0], batch[3]);
    assert_eq!(batch[3], batch[7]);
    assert_eq!(batch[1], Some(b"v001".to_vec()));
    assert_eq!(batch[1], batch[4]);
    assert_eq!(batch[2], Some(b"v002".to_vec()));
    assert_eq!(batch[2], batch[5]);
    assert_eq!(batch[6], None);
}

#[test]
fn multi_get_kernel_matches_loop() {
    let engine = AikoqlStorageEngineV2::open(dir("m25-kernel")).unwrap();
    let clock = Arc::new(ManualClock::new(10_000));
    let k = Kernel::open(Arc::new(engine), clock, 0x25).unwrap();

    let mut koids = Vec::with_capacity(30);
    for i in 0..30i64 {
        let mut req = RememberRequest::create(ctx(), meta("m25"));
        req.properties.insert("seq".into(), Value::Int(i));
        req.properties
            .insert("body".into(), Value::Text(format!("payload {i:03}")));
        koids.push(k.remember(req).unwrap().koid);
    }

    // query set: all created + duplicates + a minted-never-created koid
    let missing = KOID([0xEE; KOID_LEN]);
    let mut targets: Vec<KOID> = koids.clone();
    targets.push(koids[2]);
    targets.push(koids[29]);
    targets.push(missing);

    // valid-only batch == per-target loop, byte-exact
    let valid = &targets[..targets.len() - 1];
    let expected: Vec<aikoql_kernel::KnowledgeObject> =
        valid.iter().map(|t| k.get(ctx(), t).unwrap()).collect();
    let batch = k.get_many(ctx(), valid).unwrap();
    assert_eq!(batch, expected);

    // fail-fast parity: a missing target errors in both shapes
    assert!(k.get(ctx(), &missing).is_err());
    assert!(k.get_many(ctx(), &[koids[0], missing]).is_err());
}
