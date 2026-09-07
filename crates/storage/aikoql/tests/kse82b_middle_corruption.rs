//! KSE-082B — middle-record corruption with a valid tail (certification
//! doc §4: `docs/AIKOQL_Storage_Engine_MVP_Certification_TDD.md`).
//!
//! The doc's missing scenario: record 101 corrupted while records 102-200
//! remain valid. The engine must fail closed — never skip record 101 and
//! never truncate acknowledged records after it — and a failed open must
//! not mutate the WAL. The genuine torn tail (crash mid-append of the LAST
//! record) must still truncate and recover.
//!
//! Policy (documented PoV): A — fail closed. The tail-vs-middle
//! distinction is made by construction: a torn-looking record followed by
//! a complete, checksum-verified record is middle corruption, not a crash
//! tail.
//!
//! RED history: TEST-KSE-082B-01/-02 and the magic/version/type legs of
//! -03 failed closed on day one (envelope parse_at propagates Err before
//! open() truncates); the payload_len-OVERRUN leg of -03 was the RED —
//! parse_at classified the overrun as TornTail, replay broke, and open()
//! truncated the file at record 101's offset, silently destroying
//! acknowledged records 102-200.

mod common;

use aikoql_kernel::knowledge::kom::sha256;
use aikoql_kernel::storage::store::{StorageEngine, WriteBatch};
use aikoql_storage::envelope;
use aikoql_storage::AikoqlStorageEngine;
use common::tmp;
use std::path::PathBuf;

const RECORDS: usize = 200;
/// Envelope layout: magic(4) version(1) flags(1) type(1) payload_len(4),
/// payload, checksum(8) — see src/envelope.rs.
const HEADER_LEN: usize = 11;
const CHECKSUM_LEN: usize = 8;

/// Seed a WAL with `RECORDS` one-put records and return the raw bytes.
fn seed(tag: &str) -> (PathBuf, Vec<u8>) {
    let p = tmp(tag);
    {
        let e = AikoqlStorageEngine::open(&p).unwrap();
        for i in 0..RECORDS {
            let mut b = WriteBatch::new();
            b.put(format!("k{i:03}").into_bytes(), vec![(i % 251) as u8; 256]);
            e.write_batch(&b).unwrap();
        }
    }
    let bytes = std::fs::read(&p).unwrap();
    (p, bytes)
}

/// Offsets of every record boundary: bounds[i] = start of record i.
fn record_bounds(bytes: &[u8]) -> Vec<usize> {
    let mut v = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        v.push(pos);
        match envelope::parse_at(bytes, pos).unwrap() {
            envelope::ParseOutcome::Complete { end, .. } => pos = end,
            envelope::ParseOutcome::TornTail => break,
        }
    }
    v
}

fn hash(bytes: &[u8]) -> Vec<u8> {
    sha256(bytes).to_vec()
}

/// Assert the failed-open contract: the error is a store-level
/// classification (not a torn tail), classified per leg, and the WAL is
/// byte-unchanged. The shared property across all fail-closed legs is the
/// `aikoql-storage:` store prefix; the per-leg needle names the cause.
fn assert_fail_closed_and_untouched(p: &PathBuf, before: &[u8], needle: &str) {
    let err = match AikoqlStorageEngine::open(p) {
        Err(e) => e,
        Ok(_) => panic!("middle corruption must fail closed — open succeeded"),
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("aikoql-storage:"),
        "expected a store-level classification, got: {msg}"
    );
    assert!(msg.contains(needle), "expected {needle:?}, got: {msg}");
    let after = std::fs::read(p).unwrap();
    assert_eq!(
        after.len(),
        before.len(),
        "WAL size changed by a failed open"
    );
    assert_eq!(
        hash(&after),
        hash(before),
        "WAL bytes changed by a failed open"
    );
}

/// TEST-KSE-082B-01 — middle payload corruption: one payload byte of
/// record 101 flipped, records 102-200 valid. Fail closed, WAL untouched,
/// nothing after the corruption applied.
#[test]
fn test_kse_082b_01_middle_payload_corruption() {
    let (p, bytes) = seed("kse82b_payload");
    let b = record_bounds(&bytes);
    let mut damaged = bytes.clone();
    let payload_start = b[101] + HEADER_LEN;
    let payload_end = b[102] - CHECKSUM_LEN;
    damaged[payload_start + (payload_end - payload_start) / 2] ^= 0x01;
    std::fs::write(&p, &damaged).unwrap();
    assert_fail_closed_and_untouched(&p, &damaged, "checksum mismatch");
}

/// TEST-KSE-082B-02 — middle checksum corruption: one checksum byte of
/// record 101 modified. Fail closed, WAL unchanged.
#[test]
fn test_kse_082b_02_middle_checksum_corruption() {
    let (p, bytes) = seed("kse82b_checksum");
    let b = record_bounds(&bytes);
    let mut damaged = bytes.clone();
    damaged[b[102] - 1] ^= 0x01; // last checksum byte of record 101
    std::fs::write(&p, &damaged).unwrap();
    assert_fail_closed_and_untouched(&p, &damaged, "checksum mismatch");
}

/// TEST-KSE-082B-03 — middle header corruption: magic, version, record
/// type, and payload length mutated independently. Every leg must fail
/// closed with no automatic truncation and no partial recovery.
#[test]
fn test_kse_082b_03_middle_header_corruption() {
    // (description, mutation applied to record 101, expected classification)
    type Leg = (
        &'static str,
        Box<dyn Fn(&mut Vec<u8>, &[usize])>,
        &'static str,
    );
    let legs: Vec<Leg> = vec![
        ("magic", Box::new(|d, b| d[b[101]] ^= 0x01), "bad magic"),
        (
            "version",
            Box::new(|d, b| d[b[101] + 4] = 2),
            "unsupported format version 2",
        ),
        (
            "record type",
            Box::new(|d, b| d[b[101] + 6] = 99),
            "unknown record type 99",
        ),
        // payload_len shrunk: header no longer matches the stored
        // checksum -> checksum mismatch, fail closed.
        (
            "payload_len shrink",
            Box::new(|d, b| d[b[101] + 7..b[101] + 11].copy_from_slice(&1u32.to_be_bytes())),
            "checksum mismatch",
        ),
        // payload_len overrun: declared length runs past EOF. Must be
        // classified as middle corruption, NOT a torn tail (the RED leg).
        (
            "payload_len overrun",
            Box::new(|d, b| d[b[101] + 7..b[101] + 11].copy_from_slice(&u32::MAX.to_be_bytes())),
            "truncated record followed by valid data",
        ),
    ];
    for (name, mutate, needle) in legs {
        let (p, bytes) = seed(&format!("kse82b_hdr_{}", name.replace(' ', "_")));
        let b = record_bounds(&bytes);
        let mut damaged = bytes.clone();
        mutate(&mut damaged, &b);
        std::fs::write(&p, &damaged).unwrap();
        assert_fail_closed_and_untouched(&p, &damaged, needle);
    }
}

/// Control: a genuine torn tail (crash mid-append of the LAST record)
/// still truncates and recovers — the fix must not break the KSE-022/
/// KSE-083 recovery contract.
#[test]
fn test_kse_082b_torn_tail_control() {
    let (p, bytes) = seed("kse82b_tail");
    let b = record_bounds(&bytes);
    assert_eq!(b.len(), RECORDS);
    // Cut inside the final record: records 0..198 complete, record 199 torn.
    let cut = b[199] + 15;
    std::fs::write(&p, &bytes[..cut]).unwrap();
    let e = AikoqlStorageEngine::open(&p).unwrap();
    let scan = e.scan(b"k").unwrap();
    assert_eq!(
        scan.len(),
        199,
        "torn tail must truncate to the last complete record"
    );
    assert_eq!(
        std::fs::metadata(&p).unwrap().len(),
        b[199] as u64,
        "file must be truncated at the record boundary"
    );
}
