//! SE2-M1 — corrupted segments fail closed (TESTING-PLAN-V2 row V2-M1).
//!
//! Leg table over the pinned 249-byte golden fixture (§20 corruption model):
//! structural damage (header / block headers / index / bloom / footer) is
//! caught at open — the footer's skeleton checksum covers every block
//! header, so a flipped block-header byte fails at open even though block
//! payloads are validated lazily. Payload damage is caught on the read that
//! touches the block. Truncation at any boundary fails closed; trailing
//! bytes after the footer fail closed; an integrity-clean unknown version
//! classifies as Unsupported, never Corrupt.
//!
//! Offsets derive from the golden fixture (header 54, data block 89, index
//! 48, bloom 36, footer 22). The size assert below cross-checks them.

mod common;

use aikoql_kernel::knowledge::kom::sha256;
use aikoql_storage_v2::format::FormatError;
use aikoql_storage_v2::segment::{
    SegmentReader, SegmentWriter, FLAG_DELETE, FLAG_PUT, FLAG_VERSION,
};
use common::{entry, tmp};
use std::path::{Path, PathBuf};

const BLK_HDR: usize = 28;
const HDR: usize = 54;
const DB0: usize = HDR; // data block 0
const IDX: usize = 143; // index block
const BLM: usize = 191; // bloom block
const FTR: usize = 227; // footer
const TOT: usize = 249;

fn publish_fixture() -> (PathBuf, Vec<u8>) {
    let mut w = SegmentWriter::new(4096);
    w.push(entry("a3", "v3", 9, FLAG_DELETE));
    w.push(entry("a1", "v1", 5, FLAG_PUT));
    w.push(entry("a2", "v2", 7, FLAG_VERSION));
    let path = tmp("segment-corrupt");
    w.publish(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        bytes.len(),
        TOT,
        "golden fixture size changed — update leg offsets"
    );
    (path, bytes)
}

fn write_flipped(path: &Path, bytes: &[u8], offset: usize) {
    let mut bad = bytes.to_vec();
    bad[offset] ^= 0x80;
    std::fs::write(path, bad).unwrap();
}

#[test]
fn structural_corruption_fails_closed_at_open() {
    // (offset, what it is): magic bytes, checksum bytes, block headers.
    let legs: &[(usize, &str)] = &[
        (0, "header magic"),
        (HDR - 1, "header checksum"),
        (DB0, "data block magic"),
        (DB0 + BLK_HDR - 1, "data block header checksum"),
        (IDX, "index block magic"),
        (IDX + BLK_HDR - 1, "index block checksum"),
        (BLM, "bloom block magic"),
        (BLM + BLK_HDR - 1, "bloom block checksum"),
        (FTR, "footer magic"),
        (TOT - 1, "footer checksum"),
    ];
    let (path, bytes) = publish_fixture();
    for &(offset, what) in legs {
        write_flipped(&path, &bytes, offset);
        let err = SegmentReader::open(&path).unwrap_err();
        assert!(
            matches!(err, FormatError::Corrupt(_)),
            "{what} at {offset}: expected Corrupt, got {err:?}"
        );
    }
}

#[test]
fn payload_corruption_fails_closed_on_access() {
    let (path, bytes) = publish_fixture();
    write_flipped(&path, &bytes, DB0 + BLK_HDR + 5); // inside entry 0's payload
                                                     // Lazy validation: the payload is untouched at open…
    let r = SegmentReader::open(&path).unwrap();
    // …but any read that touches the block fails closed.
    assert!(matches!(r.get(b"a1"), Err(FormatError::Corrupt(_))));
    assert!(matches!(r.versions(b"a3"), Err(FormatError::Corrupt(_))));
}

#[test]
fn truncation_fails_closed_at_every_region() {
    let (path, bytes) = publish_fixture();
    // Mid-header, mid-block-header, mid-payload, mid-index, mid-bloom,
    // mid-footer, footer-minus-checksum.
    for cut in [10usize, 40, 60, 120, 150, 200, 240, 248] {
        std::fs::write(&path, &bytes[..cut]).unwrap();
        let err = SegmentReader::open(&path).unwrap_err();
        assert!(
            matches!(err, FormatError::Corrupt(_)),
            "truncation at {cut}: expected Corrupt, got {err:?}"
        );
    }
}

#[test]
fn trailing_bytes_fail_closed() {
    let (path, bytes) = publish_fixture();
    let mut overlong = bytes.clone();
    overlong.push(0);
    std::fs::write(&path, overlong).unwrap();
    assert!(matches!(
        SegmentReader::open(&path),
        Err(FormatError::Corrupt(_))
    ));
}

#[test]
fn unknown_version_fails_closed_as_unsupported() {
    // A version-2 segment with a VALID header checksum is a newer format,
    // not corruption — the error class must say so.
    let (path, bytes) = publish_fixture();
    let mut newer = bytes.clone();
    newer[4..6].copy_from_slice(&2u16.to_le_bytes());
    let checksum = sha256(&newer[..HDR - 8]);
    newer[HDR - 8..HDR].copy_from_slice(&checksum[..8]);
    std::fs::write(&path, newer).unwrap();
    let err = SegmentReader::open(&path).unwrap_err();
    assert!(matches!(err, FormatError::Unsupported(_)), "got {err:?}");
}
