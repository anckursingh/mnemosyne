//! Versioned physical record envelope (KSE-3, MRFC-KSE-001 §9).
//!
//! Layout (all big-endian):
//!
//! ```text
//! magic(4) version(1) flags(1) record_type(1) payload_len(4) payload checksum(8)
//! ```
//!
//! checksum = first 8 bytes of sha256 over everything before it (the same
//! hash primitive the kernel's audit chain uses). A torn tail (fewer bytes
//! than one full record — crash mid-append) is distinguished from corruption:
//! replay truncates the former, fails closed on the latter. KSE-11 reserves
//! flags bit 0 for encrypted payloads.
//!
//! Threat model (PR#2 review SE-07): the 8-byte checksum is an INTEGRITY
//! FINGERPRINT — accidental corruption detection YES (false-positive
//! ~2^-64 per candidate), cryptographic authenticity NO (no keyed input),
//! attacker modification resistance NO (an attacker who can rewrite bytes
//! can recompute the fingerprint). Adversarial integrity belongs to the
//! encrypted envelope (MRFC-0020, KSE-11), whose keyed cipher authenticates
//! payloads; this checksum only catches bit rot, truncation and torn tails.

use aikoql_kernel::knowledge::kom::{sha256, KError, KResult};

pub const MAGIC: &[u8; 4] = b"AKQL";
pub const FORMAT_VERSION: u8 = 1;
pub const TYPE_BATCH: u8 = 1;
const HEADER_LEN: usize = 11;
const CHECKSUM_LEN: usize = 8;

fn corrupt(what: &str) -> KError {
    KError::Store(format!("aikoql-storage: corrupt log: {}", what))
}

/// Encode one record: header + payload + checksum.
pub fn encode_record(record_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len() + CHECKSUM_LEN);
    out.extend_from_slice(MAGIC);
    out.push(FORMAT_VERSION);
    out.push(0); // flags — bit 0 reserved for encryption (KSE-11)
    out.push(record_type);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    let ck = sha256(&out);
    out.extend_from_slice(&ck[..CHECKSUM_LEN]);
    out
}

#[derive(Debug)]
pub enum ParseOutcome {
    /// One complete, checksum-verified record (the type was validated;
    /// unknown types are rejected in `parse_at`).
    Complete { payload: Vec<u8>, end: usize },
    /// Fewer bytes than a full record — a crash mid-append. Not corruption;
    /// replay truncates back to the last good offset.
    TornTail,
}

/// Parse one record at `offset`. `Err` = corruption / incompatibility
/// (deterministic, fail closed); `TornTail` = safely ignorable tail.
pub fn parse_at(bytes: &[u8], offset: usize) -> KResult<ParseOutcome> {
    if bytes.len() - offset < HEADER_LEN + CHECKSUM_LEN {
        return Ok(ParseOutcome::TornTail);
    }
    if &bytes[offset..offset + 4] != MAGIC {
        return Err(corrupt("bad magic"));
    }
    let version = bytes[offset + 4];
    if version != FORMAT_VERSION {
        return Err(KError::Store(format!(
            "aikoql-storage: unsupported format version {} (this build supports {})",
            version, FORMAT_VERSION
        )));
    }
    let record_type = bytes[offset + 6];
    if record_type != TYPE_BATCH {
        return Err(corrupt(&format!("unknown record type {}", record_type)));
    }
    let plen = u32::from_be_bytes(bytes[offset + 7..offset + 11].try_into().unwrap()) as usize;
    let end = offset + HEADER_LEN + plen + CHECKSUM_LEN;
    if end > bytes.len() {
        return Ok(ParseOutcome::TornTail);
    }
    let stored = &bytes[end - CHECKSUM_LEN..end];
    let computed = sha256(&bytes[offset..end - CHECKSUM_LEN]);
    if stored != &computed[..CHECKSUM_LEN] {
        return Err(corrupt("checksum mismatch"));
    }
    Ok(ParseOutcome::Complete {
        payload: bytes[offset + HEADER_LEN..offset + HEADER_LEN + plen].to_vec(),
        end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        encode_record(TYPE_BATCH, b"hello-kse3")
    }

    /// KSE-020 — decode(encode(record)) == record for every supported type.
    #[test]
    fn kse020_round_trip_batch() {
        let payload = b"batch payload \x00\x01\xff".to_vec();
        let rec = encode_record(TYPE_BATCH, &payload);
        match parse_at(&rec, 0).unwrap() {
            ParseOutcome::Complete {
                payload: got, end, ..
            } => {
                assert_eq!(got, payload);
                assert_eq!(end, rec.len());
            }
            ParseOutcome::TornTail => panic!("complete record parsed as torn tail"),
        }
    }

    /// KSE-021 — flipping one payload bit yields a deterministic corruption
    /// error; no corrupted data may ever be returned as valid.
    #[test]
    fn kse021_bit_flip_payload() {
        let rec = sample();
        for i in (HEADER_LEN..rec.len() - CHECKSUM_LEN).step_by(7) {
            let mut flipped = rec.clone();
            flipped[i] ^= 0x01;
            let err = parse_at(&flipped, 0).unwrap_err();
            assert!(
                format!("{err}").contains("checksum mismatch"),
                "unexpected error: {err}"
            );
        }
    }

    #[test]
    fn kse021_bit_flip_header() {
        let mut rec = sample();
        rec[1] ^= 0x01; // magic
        let err = parse_at(&rec, 0).unwrap_err();
        assert!(format!("{err}").contains("bad magic"), "got: {err}");
    }

    /// KSE-022 — a truncated record at ANY cut point is a safe outcome
    /// (TornTail), never a panic and never corrupt data returned as valid.
    #[test]
    fn kse022_truncated_never_panics() {
        let rec = sample();
        for cut in 0..rec.len() {
            let out = parse_at(&rec[..cut], 0).unwrap();
            assert!(
                matches!(out, ParseOutcome::TornTail),
                "cut at {cut} parsed as complete"
            );
        }
    }

    /// KSE-023 — an unsupported format version is an explicit
    /// incompatibility error, not a corrupt-read or a panic.
    #[test]
    fn kse023_unsupported_version() {
        let mut rec = sample();
        rec[4] = 2; // version byte
        let err = parse_at(&rec, 0).unwrap_err();
        assert!(
            format!("{err}").contains("unsupported format version 2"),
            "got: {err}"
        );
    }

    #[test]
    fn unknown_record_type_rejected() {
        let mut rec = sample();
        rec[6] = 99; // record_type byte
        let err = parse_at(&rec, 0).unwrap_err();
        assert!(
            format!("{err}").contains("unknown record type 99"),
            "got: {err}"
        );
    }

    /// Replay symmetry: two records parse back-to-back with exact offsets.
    #[test]
    fn two_records_parse_sequentially() {
        let mut buf = encode_record(TYPE_BATCH, b"first");
        buf.extend_from_slice(&encode_record(TYPE_BATCH, b"second"));
        match parse_at(&buf, 0).unwrap() {
            ParseOutcome::Complete { payload, end, .. } => {
                assert_eq!(payload, b"first".to_vec());
                assert!(end < buf.len());
                match parse_at(&buf, end).unwrap() {
                    ParseOutcome::Complete {
                        payload: p2,
                        end: e2,
                        ..
                    } => {
                        assert_eq!(p2, b"second".to_vec());
                        assert_eq!(e2, buf.len());
                    }
                    ParseOutcome::TornTail => panic!("second record torn"),
                }
            }
            ParseOutcome::TornTail => panic!("first record torn"),
        }
    }
}
