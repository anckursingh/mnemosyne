//! SE2-M0 — CURRENT golden bytes (docs/TESTING-PLAN-V2.md row V2-M0).
//!
//! CURRENT is the root pointer of the database directory: fixed 22-byte
//! layout `AKCV | format_version u16 LE | manifest_generation u64 LE |
//! sha256-8 over the first 14 bytes`. Any deviation from the golden bytes is
//! a format change — visible as a fixture diff, never silent.
//!
//! Corruption policy (design §20, manifest-corruption class): fail closed —
//! bad magic / unknown version / checksum mismatch / truncation all return
//! an error class, never a guessed state. Unsupported versions must be
//! distinguished from corruption (a newer-format file is not damaged).

mod common;

use aikoql_storage_v2::format::{Current, FormatError, CURRENT_LEN};
use common::tmp;

const FORMAT_VERSION: u16 = 1;

#[test]
fn current_golden_bytes() {
    let c = Current::new(FORMAT_VERSION, 7);
    let bytes = c.encode();
    assert_eq!(bytes.len(), CURRENT_LEN);
    assert_eq!(
        hex(&bytes),
        "414b435601000700000000000000d34680137cb752ed",
        "CURRENT golden bytes changed — format break"
    );
}

#[test]
fn current_round_trip() {
    let c = Current::new(FORMAT_VERSION, 7);
    let decoded = Current::decode(&c.encode()).unwrap();
    assert_eq!(decoded.format_version, FORMAT_VERSION);
    assert_eq!(decoded.manifest_generation, 7);
}

#[test]
fn current_unknown_version_fails_closed() {
    // A version-2 CURRENT is a newer format, not corruption — the error
    // class must say so (an operator's tooling may distinguish them).
    let bytes = Current::new(FORMAT_VERSION + 1, 7).encode();
    let err = Current::decode(&bytes).unwrap_err();
    assert!(
        matches!(err, FormatError::Unsupported(_)),
        "unknown version must classify Unsupported, got {err:?}"
    );
}

#[test]
fn current_checksum_mismatch_fails_closed() {
    // Flip one field byte: the trailing checksum no longer matches.
    let mut bytes = Current::new(FORMAT_VERSION, 7).encode();
    bytes[7] ^= 0xFF;
    assert!(matches!(
        Current::decode(&bytes),
        Err(FormatError::Corrupt(_))
    ));
    // Flip one checksum byte: same class.
    let mut bytes = Current::new(FORMAT_VERSION, 7).encode();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    assert!(matches!(
        Current::decode(&bytes),
        Err(FormatError::Corrupt(_))
    ));
}

#[test]
fn current_truncated_and_overlong_fail_closed() {
    let bytes = Current::new(FORMAT_VERSION, 7).encode();
    assert!(matches!(
        Current::decode(&bytes[..21]),
        Err(FormatError::Corrupt(_))
    ));
    let mut overlong = bytes.clone();
    overlong.push(0);
    assert!(matches!(
        Current::decode(&overlong),
        Err(FormatError::Corrupt(_))
    ));
}

#[test]
fn current_bad_magic_fails_closed() {
    let mut bytes = Current::new(FORMAT_VERSION, 7).encode();
    bytes[0] = b'X';
    assert!(matches!(
        Current::decode(&bytes),
        Err(FormatError::Corrupt(_))
    ));
}

#[test]
fn current_file_round_trip() {
    let path = tmp("current-roundtrip");
    let c = Current::new(FORMAT_VERSION, 42);
    Current::publish(&path, &c).unwrap();
    let read = Current::read(&path).unwrap();
    assert_eq!(read.manifest_generation, 42);
}

#[test]
fn current_missing_file_fails_closed() {
    // A missing CURRENT is an Io condition (fresh database), NOT corruption
    // — open() will later treat it as initialization; read() must not guess.
    let path = tmp("current-missing");
    let _ = std::fs::remove_file(&path);
    assert!(matches!(Current::read(&path), Err(FormatError::Io(_))));
}

/// Pinned-by-construction helper: the fixture above is hex, so this is the
/// only format drift surface left to eyeballs.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
