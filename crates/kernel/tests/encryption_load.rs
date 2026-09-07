//! Encryption load test — MRFC-0020 Phase 1 performance measurement.
//!
//! Measures write throughput overhead of EncryptedStore vs plain redb.
//! Report-only cell (the M8 rule — a perf number never gates a test): the
//! <100% floor formerly asserted here flapped on a dev box (186.6% with no
//! code change — AV/disk-cache noise on microsecond samples). Runs weekly
//! via --ignored release.

use aikoql_kernel::security::crypto::{Aes256Gcm, Crypto};
use aikoql_kernel::storage::encrypted::EncryptedStore;
use aikoql_kernel::storage::store::{StorageEngine, WriteBatch};
use aikoql_kernel::storage::store_redb::RedbEngine;
use std::sync::Arc;
use std::time::Instant;

const BATCH_SIZE: usize = 50;
const VALUE_SIZE: usize = 256;

#[test]
#[cfg_attr(debug_assertions, ignore)]
fn load_encryption_overhead_with_redb() {
    let path_base = format!(
        "{}/aikoql-load-{}",
        std::env::temp_dir().display(),
        std::process::id()
    );

    let plain_path = format!("{}-plain.redb", path_base);
    let enc_path = format!("{}-enc.redb", path_base);
    let _ = std::fs::remove_file(&plain_path);
    let _ = std::fs::remove_file(&enc_path);

    let plain = Arc::new(RedbEngine::open(&plain_path).expect("open plain"));
    let crypto = Arc::new(Crypto::new(Box::new(Aes256Gcm::new())));
    let key = crypto.generate_key();
    let enc_redb = Arc::new(RedbEngine::open(&enc_path).expect("open enc"));
    let enc = EncryptedStore::new(enc_redb, crypto, key);

    let value = vec![0xABu8; VALUE_SIZE];
    let mut keys = Vec::with_capacity(BATCH_SIZE);
    for i in 0..BATCH_SIZE {
        keys.push(format!("k{:04x}", i).into_bytes());
    }

    // Warm-up.
    for _ in 0..3 {
        let mut b = WriteBatch::new();
        for k in &keys {
            b.put(k.clone(), value.clone());
        }
        plain.write_batch(&b).unwrap();
        enc.write_batch(&b).unwrap();
    }

    // Benchmark: plain redb.
    let mut plain_times = Vec::with_capacity(10);
    for _ in 0..10 {
        let mut b = WriteBatch::new();
        for k in &keys {
            b.put(k.clone(), value.clone());
        }
        let start = Instant::now();
        plain.write_batch(&b).unwrap();
        plain_times.push(start.elapsed().as_micros());
    }
    let plain_avg = plain_times.iter().sum::<u128>() as f64 / plain_times.len() as f64;

    // Benchmark: encrypted redb.
    let mut enc_times = Vec::with_capacity(10);
    for _ in 0..10 {
        let mut b = WriteBatch::new();
        for k in &keys {
            b.put(k.clone(), value.clone());
        }
        let start = Instant::now();
        enc.write_batch(&b).unwrap();
        enc_times.push(start.elapsed().as_micros());
    }
    let enc_avg = enc_times.iter().sum::<u128>() as f64 / enc_times.len() as f64;

    let overhead_pct = ((enc_avg - plain_avg) / plain_avg) * 100.0;
    println!(
        "redb plain: {:.0}µs, encrypted: {:.0}µs, overhead: {:.1}% ({} × {}-byte values)",
        plain_avg, enc_avg, overhead_pct, BATCH_SIZE, VALUE_SIZE
    );

    let _ = std::fs::remove_file(&plain_path);
    let _ = std::fs::remove_file(&enc_path);
}
