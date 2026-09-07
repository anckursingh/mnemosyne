//! KSE-3 — record envelope at the file level (MRFC-KSE-001 §9).
//!
//! Envelope unit tests (KSE-020..023) live in `src/envelope.rs`; these tests
//! prove the WAL itself behaves correctly when the FILE is damaged: a
//! flipped byte must fail closed on reopen, a torn tail must truncate
//! silently, and a non-AIKOQL file must refuse to open.

use aikoql_kernel::knowledge::kom::KError;
use aikoql_kernel::storage::store::{StorageEngine, WriteBatch};
use aikoql_storage::AikoqlStorageEngine;
use std::path::PathBuf;

mod common;
use common::tmp;

fn seed(p: &PathBuf) {
    let e = AikoqlStorageEngine::open(p).unwrap();
    let mut b = WriteBatch::new();
    b.put(b"k1".to_vec(), vec![1, 2, 3]);
    e.write_batch(&b).unwrap();
    drop(e);
}

/// KSE-021 at the file level: one flipped payload bit is a deterministic
/// corruption error on reopen — corrupted data is never served as valid.
#[test]
fn kse3_reopen_detects_flipped_byte() {
    let p = tmp("flip");
    seed(&p);
    let mut bytes = std::fs::read(&p).unwrap();
    bytes[12] ^= 0x01; // inside the first record's payload (header = 11 bytes)
    std::fs::write(&p, &bytes).unwrap();

    let err = match AikoqlStorageEngine::open(&p) {
        Err(e) => e,
        Ok(_) => panic!("open must fail on a corrupted file"),
    };
    assert!(
        matches!(err, KError::Store(ref m) if m.contains("checksum mismatch")),
        "unexpected error: {err}"
    );
    let _ = std::fs::remove_file(&p);
}

/// KSE-022 at the file level: a torn tail (crash mid-append) truncates back
/// to the last good record — the acknowledged state survives.
#[test]
fn kse3_reopen_truncates_torn_tail() {
    let p = tmp("torn");
    let e = AikoqlStorageEngine::open(&p).unwrap();
    let mut b1 = WriteBatch::new();
    b1.put(b"a".to_vec(), vec![1]);
    e.write_batch(&b1).unwrap();
    let mut b2 = WriteBatch::new();
    b2.put(b"b".to_vec(), vec![2]);
    e.write_batch(&b2).unwrap();
    drop(e);

    let good_len = std::fs::metadata(&p).unwrap().len();
    // Simulate a crash mid-append: a partial header reaches the disk.
    let mut bytes = std::fs::read(&p).unwrap();
    bytes.extend_from_slice(b"AKQL\x01\x00");
    std::fs::write(&p, &bytes).unwrap();

    let e2 = AikoqlStorageEngine::open(&p).unwrap();
    assert_eq!(e2.get(b"a").unwrap(), Some(vec![1]));
    assert_eq!(e2.get(b"b").unwrap(), Some(vec![2]));
    drop(e2);
    // The torn bytes are gone — the file is back to the last good offset.
    assert_eq!(std::fs::metadata(&p).unwrap().len(), good_len);
    let _ = std::fs::remove_file(&p);
}

/// A file that is not an AIKOQL log fails closed on open (bad magic).
#[test]
fn kse3_reopen_rejects_foreign_file() {
    let p = tmp("foreign");
    std::fs::write(&p, b"not an aikoql log, just words").unwrap();
    let err = match AikoqlStorageEngine::open(&p) {
        Err(e) => e,
        Ok(_) => panic!("open must fail on a non-AIKOQL file"),
    };
    assert!(
        matches!(err, KError::Store(ref m) if m.contains("bad magic")),
        "unexpected error: {err}"
    );
    let _ = std::fs::remove_file(&p);
}
