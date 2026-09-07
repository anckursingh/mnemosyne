//! SE2-M1 — segment golden bytes + round-trip (TESTING-PLAN-V2 row V2-M1).
//!
//! Byte layout (design §11, pinned by the fixture): header `AKSE` (version,
//! block count, entry count, key range, seq range, sha256-8) → data blocks →
//! index block → bloom block → footer `AKFT` (skeleton sha256-8). Each
//! block: 20-byte header `AKBL` (version, type, compression, entry count,
//! sizes, sha256-8 over header+payload) + payload. Entries are
//! prefix-compressed and sorted (key asc, seq desc) — head = first version.
//!
//! The fixture below was computed independently in python (hashlib +
//! struct) before any Rust existed; a format change is a visible diff.

mod common;

use aikoql_storage_v2::format::{checksum8, FormatError};
use aikoql_storage_v2::segment::{
    SegmentReader, SegmentWriter, FLAG_DELETE, FLAG_PUT, FLAG_VERSION,
};
use common::{entry, hex, tmp};

#[test]
fn segment_golden_bytes() {
    // Writer input deliberately unsorted — publish must sort by
    // (key asc, seq desc) so the output is deterministic.
    let mut w = SegmentWriter::new(4096);
    w.push(entry("a3", "v3", 9, FLAG_DELETE));
    w.push(entry("a1", "v1", 5, FLAG_PUT));
    w.push(entry("a2", "v2", 7, FLAG_VERSION));
    let path = tmp("segment-golden");
    w.publish(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        hex(&bytes),
        concat!(
            "414b53450100010000000300000000000000020000006131020000006133050000",
            "00000000000900000000000000c4db5c102ef23785414b424c0100000003000000",
            "3d0000003d000000e028ebd981084a840000020061310200000076310500000000",
            "000000010100010032020000007632070000000000000004010001003302000000",
            "7633090000000000000002414b424c01000100010000001400000014000000742f",
            "cc49dbdc87320200613102006133360000000000000003000000414b424c010002",
            "00030000000800000008000000f6d1b0a32f1ff4ae1e0000000ca50139414b4654",
            "0100030000000000000004503fe3eec20615",
        ),
        "segment golden bytes changed — format break"
    );
}

#[test]
fn segment_round_trip() {
    let mut w = SegmentWriter::new(4096);
    w.push(entry("a3", "v3", 9, FLAG_DELETE));
    w.push(entry("a1", "v1", 5, FLAG_PUT));
    w.push(entry("a2", "v2", 7, FLAG_VERSION));
    let path = tmp("segment-roundtrip");
    w.publish(&path).unwrap();

    let r = SegmentReader::open(&path).unwrap();
    assert_eq!(r.entry_count(), 3);
    assert_eq!(r.block_count(), 1);
    assert_eq!(r.key_min(), b"a1");
    assert_eq!(r.key_max(), b"a3");
    assert_eq!(r.seq_lo(), 5);
    assert_eq!(r.seq_hi(), 9);

    assert_eq!(r.get(b"a1").unwrap(), Some(entry("a1", "v1", 5, FLAG_PUT)));
    assert_eq!(
        r.get(b"a2").unwrap(),
        Some(entry("a2", "v2", 7, FLAG_VERSION))
    );
    assert_eq!(
        r.get(b"a3").unwrap(),
        Some(entry("a3", "v3", 9, FLAG_DELETE))
    );
    assert_eq!(r.get(b"zz").unwrap(), None);
    assert_eq!(
        r.versions(b"a1").unwrap(),
        vec![entry("a1", "v1", 5, FLAG_PUT)]
    );
    assert!(r.bloom_may_contain(b"a1"));
}

#[test]
fn segment_reader_never_mutates() {
    let mut w = SegmentWriter::new(4096);
    w.push(entry("a1", "v1", 5, FLAG_PUT));
    w.push(entry("a2", "v2", 7, FLAG_VERSION));
    let path = tmp("segment-immutable");
    w.publish(&path).unwrap();

    let before = std::fs::read(&path).unwrap();
    let r = SegmentReader::open(&path).unwrap();
    let _ = r.get(b"a1");
    let _ = r.get(b"a2");
    let _ = r.versions(b"a1");
    let _ = r.scan(b"a", b"b");
    let _ = r.bloom_may_contain(b"a1");
    drop(r);
    let after = std::fs::read(&path).unwrap();
    assert_eq!(before, after, "reads must never mutate a published segment");
}

#[test]
fn v2_publish_bytes_pinned() {
    // SE2-M15 — the streamed publish rewrite must stay byte-identical to
    // the buffered writer. The fixture spans the branches the rewrite has
    // to reproduce exactly: unsorted input, a 17-version equal-key run
    // (the SE2-M14 restart skip), and a second block forced by a 300-byte
    // value (target 512).
    let mut w = SegmentWriter::new_v2(512);
    w.push(entry("gamma", &"g".repeat(300), 300, FLAG_PUT));
    for (i, seq) in (100..=116).enumerate() {
        w.push(entry("alpha", &format!("v{i}"), seq, FLAG_PUT));
    }
    w.push(entry("beta", "vb", 200, FLAG_VERSION));
    let path = tmp("v2-publish-pin");
    let (file_size, checksum) = w.publish(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        file_size as usize,
        bytes.len(),
        "publish file_size mismatch"
    );
    assert_eq!(
        checksum,
        u64::from_le_bytes(checksum8(&bytes)),
        "publish checksum is not the whole-file checksum8"
    );
    assert_eq!(
        hex(&bytes),
        // Pinned bytes captured from the buffered writer BEFORE the
        // SE2-M15 streamed rewrite (the rewrite must be byte-identical).
        // The shared header/block/footer paths are independently pinned
        // by the python-computed v1 golden above.
        include_str!("v2_publish_pin.hex"),
        "v2 segment bytes changed — format break"
    );
    let r = SegmentReader::open(&path).unwrap();
    assert_eq!(r.entry_count(), 19);
}

#[test]
fn v1_multiblock_bytes_pinned() {
    // SE2-M15 — the v1 dry pass must reproduce the buffered writer's block
    // boundaries. Target 64 splits on every entry, and each boundary entry
    // prefix-shares with the previous block's last key — the case where the
    // split estimate (old-block shared prefix) differs from the encoding
    // (shared = 0 in the new block). Pinned bytes captured from the
    // buffered writer at 00e2270 (worktree) and verified byte-identical to
    // the streamed rewrite.
    let mut w = SegmentWriter::new(64);
    w.push(entry("aa1", &"x".repeat(24), 1, FLAG_PUT));
    w.push(entry("aa2", &"x".repeat(24), 2, FLAG_PUT));
    w.push(entry("aa3", &"x".repeat(24), 3, FLAG_PUT));
    w.push(entry("aa4", &"x".repeat(24), 4, FLAG_PUT));
    w.push(entry("aa4", &"y".repeat(24), 5, FLAG_VERSION));
    w.push(entry("zz", &"z".repeat(24), 6, FLAG_DELETE));
    let path = tmp("v1-multiblock-pin");
    let (file_size, checksum) = w.publish(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        file_size as usize,
        bytes.len(),
        "publish file_size mismatch"
    );
    assert_eq!(
        checksum,
        u64::from_le_bytes(checksum8(&bytes)),
        "publish checksum is not the whole-file checksum8"
    );
    assert_eq!(
        hex(&bytes),
        include_str!("v1_multiblock_pin.hex"),
        "v1 multiblock segment bytes changed — format break"
    );
    let r = SegmentReader::open(&path).unwrap();
    assert_eq!(r.entry_count(), 6);
    assert_eq!(r.block_count(), 6);
}

#[test]
fn segment_publish_validation() {
    // Caller misuse is rejected at publish, not written to disk.
    let mut empty = SegmentWriter::new(4096);
    assert!(matches!(
        empty.publish(&tmp("segment-empty")),
        Err(FormatError::Invalid(_))
    ));

    let mut dup = SegmentWriter::new(4096);
    dup.push(entry("a1", "v1", 5, FLAG_PUT));
    dup.push(entry("a1", "v2", 5, FLAG_PUT)); // same (key, seq)
    assert!(matches!(
        dup.publish(&tmp("segment-dup")),
        Err(FormatError::Invalid(_))
    ));

    let mut zero = SegmentWriter::new(0);
    zero.push(entry("a1", "v1", 5, FLAG_PUT));
    assert!(matches!(
        zero.publish(&tmp("segment-zero")),
        Err(FormatError::Invalid(_))
    ));
}
