//! SE2-M2 — WAL v2 frame format (TESTING-PLAN-V2 row V2-M2, design §7).
//!
//! Frame layout (all little-endian):
//! `AKWF | format_version u16 | frame_type u8 | seq u64 | payload_len u32 |
//!  payload | sha256-8(everything before)`
//! payload: `entry_count u32 | per entry: op u8 (1=Put 2=Delete) | key_len
//!  u32 | key | (value_len u32 | value if Put)`.
//! One frame = one batch = one sequence number. Decode order is magic →
//! version → type → checksum: unknown-but-clean version/type = Unsupported,
//! damaged bytes = Corrupt. The fixture was computed in python (hashlib +
//! struct) before any Rust existed — a format change is a visible diff.

mod common;

use aikoql_storage_v2::format::FormatError;
use aikoql_storage_v2::wal::{decode_frame, encode_frame, replay_frames, Op, WalFrame};
use common::hex;

fn put(key: &str, value: &str) -> Op {
    Op::Put(key.as_bytes().to_vec(), value.as_bytes().to_vec())
}

fn del(key: &str) -> Op {
    Op::Delete(key.as_bytes().to_vec())
}

#[test]
fn wal_frame_golden_bytes() {
    let bytes = encode_frame(7, &[put("a1", "v1"), del("b2")]).unwrap();
    assert_eq!(bytes.len(), 51);
    assert_eq!(
        hex(&bytes),
        concat!(
            "414b57460100010700000000000000180000000200000001020000006131020000",
            "00763102020000006232209810f7f6c140b7",
        ),
        "WAL frame golden bytes changed — format break"
    );
}

#[test]
fn wal_frame_round_trip() {
    let ops = vec![put("key1", "value1"), del("key2")];
    let bytes = encode_frame(42, &ops).unwrap();
    let (frame, len) = decode_frame(&bytes).unwrap();
    assert_eq!(len, bytes.len());
    assert_eq!(frame, WalFrame { seq: 42, ops });
}

#[test]
fn wal_frame_rejects_empty_batch() {
    assert!(matches!(encode_frame(1, &[]), Err(FormatError::Invalid(_))));
}

#[test]
fn wal_frame_corrupt() {
    let good = encode_frame(7, &[put("a", "b")]).unwrap();
    let mut b = good.clone();
    b[0] ^= 0xff;
    assert!(
        matches!(decode_frame(&b), Err(FormatError::Corrupt(_))),
        "bad magic"
    );
    let mut b = good.clone();
    *b.last_mut().unwrap() ^= 0xff;
    assert!(
        matches!(decode_frame(&b), Err(FormatError::Corrupt(_))),
        "bad checksum"
    );
    let mut b = good.clone();
    b.truncate(good.len() - 3);
    assert!(
        matches!(decode_frame(&b), Err(FormatError::Corrupt(_))),
        "truncation"
    );
    let mut b = good.clone();
    b[15..19].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(
        matches!(decode_frame(&b), Err(FormatError::Corrupt(_))),
        "impossible payload_len"
    );
    let mut b = good.clone();
    b[4..6].copy_from_slice(&2u16.to_le_bytes());
    assert!(
        matches!(decode_frame(&b), Err(FormatError::Unsupported(_))),
        "unknown version"
    );
    let mut b = good.clone();
    b[6] = 9;
    assert!(
        matches!(decode_frame(&b), Err(FormatError::Unsupported(_))),
        "unknown frame type"
    );
    // an op byte is payload — flipping it fails the checksum, not a parse
    let mut b = good.clone();
    b[23] = 9;
    assert!(
        matches!(decode_frame(&b), Err(FormatError::Corrupt(_))),
        "op byte flip"
    );
}

#[test]
fn wal_replay_torn_tail() {
    let f1 = encode_frame(1, &[put("a", "1")]).unwrap();
    let f2 = encode_frame(2, &[put("b", "2")]).unwrap();
    // kill mid-append: a complete frame followed by a partial one
    let mut w = f1.clone();
    w.extend_from_slice(&f2);
    w.truncate(f1.len() + 10);
    let (frames, consumed) = replay_frames(&w).unwrap();
    assert_eq!(consumed, f1.len());
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].seq, 1);
    // trailing garbage after a complete frame is the same class: truncate
    let mut w2 = f1.clone();
    w2.extend_from_slice(&[0xde, 0xad]);
    let (frames, consumed) = replay_frames(&w2).unwrap();
    assert_eq!(consumed, f1.len());
    assert_eq!(frames.len(), 1);
}

#[test]
fn wal_replay_middle_damage_fails_closed() {
    // damage followed by a VALID frame: the WAL must not be trusted
    // (KSE-082B classifier shape) — never silently skip.
    let f1 = encode_frame(1, &[put("a", "1")]).unwrap();
    let f3 = encode_frame(3, &[put("c", "3")]).unwrap();
    let mut w = f1.clone();
    w.extend_from_slice(&[0u8; 51]);
    w.extend_from_slice(&f3);
    assert!(matches!(replay_frames(&w), Err(FormatError::Corrupt(_))));
}

#[test]
fn wal_replay_seq_must_increase() {
    let f5 = encode_frame(5, &[put("a", "1")]).unwrap();
    let f3 = encode_frame(3, &[put("b", "2")]).unwrap();
    let mut w = f5;
    w.extend_from_slice(&f3);
    assert!(matches!(replay_frames(&w), Err(FormatError::Corrupt(_))));
}

#[test]
fn wal_replay_walks_frames() {
    assert!(replay_frames(&[]).unwrap().0.is_empty());
    let f1 = encode_frame(1, &[put("a", "1")]).unwrap();
    let f2 = encode_frame(2, &[put("b", "2"), del("c")]).unwrap();
    let mut w = f1.clone();
    w.extend_from_slice(&f2);
    let (frames, consumed) = replay_frames(&w).unwrap();
    assert_eq!(consumed, w.len());
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].seq, 1);
    assert_eq!(frames[1].seq, 2);
    assert_eq!(frames[1].ops, vec![put("b", "2"), del("c")]);
}
