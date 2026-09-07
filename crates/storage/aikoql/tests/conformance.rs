//! KSE-1 — storage contract conformance (MRFC-KSE-001 §7).
//!
//! The six KSE asserts run identically against every backend so the
//! custom engine passes exactly what MemoryEngine, RedbEngine and
//! RocksDbEngine pass. The asserts live in `common::kse` — one definition,
//! shared verbatim with the KSE-20 matrix suite.

mod common;

use aikoql_kernel::storage::store::{MemoryEngine, StorageEngine, WriteBatch};
use aikoql_kernel::storage::store_redb::RedbEngine;
use aikoql_storage::AikoqlStorageEngine;
use common::{kse, tmp};

/// Runs the six KSE asserts against one backend instance.
macro_rules! backend_tests {
    ($modname:ident, $open:expr) => {
        mod $modname {
            use super::*;

            // Backend-prefixed temp names: parallel tests must not share files.
            #[test]
            fn kse001_get() {
                kse::kse001_get(&*($open)(concat!(stringify!($modname), "_kse001")));
            }
            #[test]
            fn kse002_missing_key() {
                kse::kse002_missing_key(&*($open)(concat!(stringify!($modname), "_kse002")));
            }
            #[test]
            fn kse003_prefix_scan() {
                kse::kse003_prefix_scan(&*($open)(concat!(stringify!($modname), "_kse003")));
            }
            #[test]
            fn kse004_atomic_batch() {
                kse::kse004_atomic_batch(&*($open)(concat!(stringify!($modname), "_kse004")));
            }
            #[test]
            fn kse005_empty_batch() {
                kse::kse005_empty_batch(&*($open)(concat!(stringify!($modname), "_kse005")));
            }
            #[test]
            fn kse006_conflicting_put_delete() {
                kse::kse006_conflicting_put_delete(&*($open)(concat!(
                    stringify!($modname),
                    "_kse006"
                )));
            }
        }
    };
}

backend_tests!(aikoql, |name: &str| -> Box<dyn StorageEngine> {
    Box::new(AikoqlStorageEngine::open(tmp(name)).unwrap())
});
backend_tests!(memory, |_: &str| -> Box<dyn StorageEngine> {
    Box::new(MemoryEngine::new())
});
backend_tests!(redb, |name: &str| -> Box<dyn StorageEngine> {
    Box::new(RedbEngine::open(tmp(name)).unwrap())
});
#[cfg(feature = "kse5-rocksdb")]
backend_tests!(rocks, |name: &str| -> Box<dyn StorageEngine> {
    Box::new(aikoql_rocksdb::RocksDbEngine::open(tmp(name)).unwrap())
});

/// Engine sanity (mirrors the rocksdb crate's): state survives a reopen.
#[test]
fn persists_across_reopen() {
    let p = tmp("reopen");
    {
        let e = AikoqlStorageEngine::open(&p).unwrap();
        let mut b = WriteBatch::new();
        b.put(b"k1".to_vec(), vec![1, 2, 3]);
        e.write_batch(&b).unwrap();
    }
    let e2 = AikoqlStorageEngine::open(&p).unwrap();
    assert_eq!(e2.get(b"k1").unwrap(), Some(vec![1, 2, 3]));
    let _ = std::fs::remove_file(&p);
}
