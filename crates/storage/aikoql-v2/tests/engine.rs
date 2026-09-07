//! V2-Adopt — `AikoqlStorageEngineV2`: the kernel `StorageEngine` adapter
//! over the v2 Db. The six KSE-1 asserts (the shared definition) run here
//! per-backend as granular tests; the KSE-20 matrix
//! (`kse20_backend_conformance.rs`) runs the same definition across all
//! backends. Persistence across reopen is the one divergence surface the
//! six asserts cannot see — pinned per engine below.

mod common;

use aikoql_kernel::storage::store::{StorageEngine, WriteBatch};
use aikoql_storage_v2::db::{Config, DurabilityMode};
use aikoql_storage_v2::AikoqlStorageEngineV2;
use common::{kse, tmp};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn kse001_get() {
    kse::kse001_get(&AikoqlStorageEngineV2::open(tmp("engine-kse1")).unwrap());
}

#[test]
fn kse002_missing_key() {
    kse::kse002_missing_key(&AikoqlStorageEngineV2::open(tmp("engine-kse2")).unwrap());
}

#[test]
fn kse003_prefix_scan() {
    kse::kse003_prefix_scan(&AikoqlStorageEngineV2::open(tmp("engine-kse3")).unwrap());
}

#[test]
fn kse004_atomic_batch() {
    kse::kse004_atomic_batch(&AikoqlStorageEngineV2::open(tmp("engine-kse4")).unwrap());
}

#[test]
fn kse005_empty_batch() {
    kse::kse005_empty_batch(&AikoqlStorageEngineV2::open(tmp("engine-kse5")).unwrap());
}

#[test]
fn kse006_conflicting_put_delete() {
    kse::kse006_conflicting_put_delete(&AikoqlStorageEngineV2::open(tmp("engine-kse6")).unwrap());
}

#[test]
fn reopen_serves_durable_state() {
    let path = tmp("engine-reopen");
    {
        let e = AikoqlStorageEngineV2::open(&path).unwrap();
        let mut b = WriteBatch::new();
        b.put(b"keep".to_vec(), b"v".to_vec());
        b.put(b"gone".to_vec(), b"v".to_vec());
        e.write_batch(&b).unwrap();
        let mut d = WriteBatch::new();
        d.del(b"gone".to_vec());
        e.write_batch(&d).unwrap();
    } // drop the handle — reopen must serve the committed state
    let e = AikoqlStorageEngineV2::open(&path).unwrap();
    assert_eq!(e.get(b"keep").unwrap(), Some(b"v".to_vec()));
    assert_eq!(e.get(b"gone").unwrap(), None);
    let rows: Vec<Vec<u8>> = e.scan(b"").unwrap().into_iter().map(|(k, _)| k).collect();
    assert_eq!(rows, vec![b"keep".to_vec()]);
}

#[test]
fn multi_put_batch_last_wins_and_is_atomic() {
    let path = tmp("engine-multiput");
    let e = AikoqlStorageEngineV2::open(&path).unwrap();
    let mut b = WriteBatch::new();
    b.put(b"d".to_vec(), vec![1]);
    b.put(b"d".to_vec(), vec![2]); // same key twice — last put wins
    b.put(b"e".to_vec(), vec![3]);
    e.write_batch(&b).unwrap();
    assert_eq!(e.get(b"d").unwrap(), Some(vec![2]));
    assert_eq!(e.get(b"e").unwrap(), Some(vec![3]));
    // all-or-nothing survives reopen: both keys came from one batch
    drop(e);
    let e = AikoqlStorageEngineV2::open(&path).unwrap();
    assert_eq!(e.get(b"d").unwrap(), Some(vec![2]));
    assert_eq!(e.get(b"e").unwrap(), Some(vec![3]));
}

/// PR#2 review SE-05: 32 concurrent StorageEngine writers through the
/// ADAPTER, GroupCommit enabled. The old adapter held `RwLock<Db>` and
/// serialized write_batch before the commit queue ever saw a second batch
/// — group size ≈ 1, one fsync per batch (32 here). The refactored adapter
/// holds the Db directly: the barrier releases all 32 within microseconds
/// of each other and the 200 ms drain window must coalesce them into ONE
/// group (default caps 4096 ops / 16 MiB — no cap split). The fsync count
/// IS the group-size evidence; recovery correctness closes the review's
/// checklist.
#[test]
fn se05_adapter_does_not_defeat_group_commit() {
    let path = tmp("engine-gc32");
    let mut cfg = Config::new(PathBuf::from(&path));
    cfg.durability = DurabilityMode::GroupCommit; // explicit opt-in, never silent
    cfg.max_wait_duration = Duration::from_millis(200); // long window
    let e = Arc::new(AikoqlStorageEngineV2::open_with_config(cfg).unwrap());
    let barrier = Arc::new(std::sync::Barrier::new(32));
    let threads: Vec<_> = (0..32u64)
        .map(|i| {
            let e = Arc::clone(&e);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut b = WriteBatch::new();
                b.put(format!("k{i}").into_bytes(), format!("v{i}").into_bytes());
                barrier.wait(); // all writers hit the queue within µs
                e.write_batch(&b).unwrap();
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(
        e.fsync_count(),
        1,
        "one fsync for all 32 batches — the adapter must not serialize \
         writers before the commit queue (32 would mean a group of one)"
    );
    // Ack implies apply: every batch's key is visible through the adapter.
    for i in 0..32u64 {
        assert_eq!(
            e.get(&format!("k{i}").into_bytes()).unwrap(),
            Some(format!("v{i}").into_bytes())
        );
    }
    drop(e); // drops the Db — the committer commits pending and is joined
    let e = AikoqlStorageEngineV2::open(&path).unwrap();
    for i in 0..32u64 {
        assert_eq!(
            e.get(&format!("k{i}").into_bytes()).unwrap(),
            Some(format!("v{i}").into_bytes()),
            "recovery serves all 32 keys after reopen"
        );
    }
}
