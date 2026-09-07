//! SE2-M0 — manifest golden bytes (docs/TESTING-PLAN-V2.md row V2-M0).
//!
//! The manifest is the authoritative topology: `AKMV | format_version u16 LE
//! | generation u64 LE | segment_count u32 LE | segment records | wal_count
//! u32 LE | wal ids | sha256-8 over everything before it`. Segment record:
//! `segment_id u64 | level u8 | key_min_len u32 | key_min | key_max_len u32
//! | key_max | seq_lo u64 | seq_hi u64 | record_count u64 | file_size u64 |
//! segment_checksum u64`.
//!
//! Corruption policy (design §20): checksum mismatch / truncation /
//! bad magic → Corrupt; unknown format version → Unsupported; a manifest
//! whose generation disagrees with CURRENT's pointer → Corrupt (the pair
//! check lives beside decode, since decode is self-contained).

mod common;

use aikoql_storage_v2::format::{FormatError, Manifest, SegmentRecord};
use common::tmp;

const FORMAT_VERSION: u16 = 1;

fn fixture_manifest() -> Manifest {
    Manifest {
        format_version: FORMAT_VERSION,
        generation: 1,
        segments: vec![SegmentRecord {
            segment_id: 1,
            level: 0,
            key_min: b"a1".to_vec(),
            key_max: b"z9".to_vec(),
            seq_lo: 5,
            seq_hi: 9,
            record_count: 100,
            file_size: 4096,
            checksum: 0x1122334455667788,
        }],
        wal_ids: vec![2],
    }
}

#[test]
fn manifest_golden_bytes() {
    let bytes = fixture_manifest().encode();
    assert_eq!(
        hex(&bytes),
        "414b4d560100010000000000000001000000010000000000000000020000006131\
         020000007a39050000000000000009000000000000006400000000000000001000\
         00000000008877665544332211010000000200000000000000b31e9a0604b61761",
        "manifest golden bytes changed — format break"
    );
}

#[test]
fn manifest_round_trip() {
    let m = fixture_manifest();
    let decoded = Manifest::decode(&m.encode()).unwrap();
    assert_eq!(decoded.format_version, FORMAT_VERSION);
    assert_eq!(decoded.generation, 1);
    assert_eq!(decoded.segments.len(), 1);
    assert_eq!(decoded.segments[0].segment_id, 1);
    assert_eq!(decoded.segments[0].level, 0);
    assert_eq!(decoded.segments[0].key_min, b"a1");
    assert_eq!(decoded.segments[0].key_max, b"z9");
    assert_eq!(decoded.segments[0].seq_lo, 5);
    assert_eq!(decoded.segments[0].seq_hi, 9);
    assert_eq!(decoded.segments[0].record_count, 100);
    assert_eq!(decoded.segments[0].file_size, 4096);
    assert_eq!(decoded.segments[0].checksum, 0x1122334455667788);
    assert_eq!(decoded.wal_ids, vec![2]);
}

#[test]
fn manifest_empty_round_trip() {
    let m = Manifest {
        format_version: FORMAT_VERSION,
        generation: 9,
        segments: vec![],
        wal_ids: vec![],
    };
    let decoded = Manifest::decode(&m.encode()).unwrap();
    assert_eq!(decoded.generation, 9);
    assert!(decoded.segments.is_empty());
    assert!(decoded.wal_ids.is_empty());
}

#[test]
fn manifest_checksum_mismatch_fails_closed() {
    let mut bytes = fixture_manifest().encode();
    bytes[10] ^= 0x40; // inside the generation field
    assert!(matches!(
        Manifest::decode(&bytes),
        Err(FormatError::Corrupt(_))
    ));
    let mut bytes = fixture_manifest().encode();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    assert!(matches!(
        Manifest::decode(&bytes),
        Err(FormatError::Corrupt(_))
    ));
}

#[test]
fn manifest_truncated_fails_closed() {
    // Cut mid-record (segment key_max bytes) and mid-checksum.
    for cut in [30usize, 40, 98] {
        let bytes = fixture_manifest().encode();
        assert!(
            matches!(
                Manifest::decode(&bytes[..cut]),
                Err(FormatError::Corrupt(_))
            ),
            "cut at {cut} must fail closed"
        );
    }
}

#[test]
fn manifest_unknown_version_fails_closed() {
    let mut m = fixture_manifest();
    m.format_version = FORMAT_VERSION + 1;
    let err = Manifest::decode(&m.encode()).unwrap_err();
    assert!(matches!(err, FormatError::Unsupported(_)));
}

#[test]
fn manifest_generation_mismatch_fails_closed() {
    // CURRENT points at generation 1; the manifest on disk says 2 — either
    // publication half-failed or the file is wrong. Fail closed, never pick.
    let current = aikoql_storage_v2::format::Current::new(FORMAT_VERSION, 1);
    let mut manifest = fixture_manifest();
    manifest.generation = 2;
    let err = aikoql_storage_v2::format::verify_pair(&current, &manifest).unwrap_err();
    assert!(matches!(err, FormatError::Corrupt(_)));
}

#[test]
fn manifest_file_round_trip() {
    let path = tmp("manifest-roundtrip");
    let m = fixture_manifest();
    Manifest::publish(&path, &m).unwrap();
    let read = Manifest::read(&path).unwrap();
    assert_eq!(read.generation, 1);
    assert_eq!(read.segments.len(), 1);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
