//! SE2-M2 — WAL v2 frames (design §7: MAGIC, FORMAT_VERSION, FRAME_TYPE,
//! SEQUENCE, PAYLOAD_LENGTH, PAYLOAD, CRC — the checksum is the crate's
//! established sha256-8, not a CRC32).
//!
//! Frame layout (all little-endian):
//! `AKWF | format_version u16 | frame_type u8 | seq u64 | payload_len u32 |
//!  payload | sha256-8(everything before)`
//! payload: `entry_count u32 | per entry: op u8 | key_len u32 | key |
//!  (value_len u32 | value if Put)`
//! op: 1 = Put, 2 = Delete. One frame = one batch = one sequence number
//! (design refinement: sequence is per-batch, not per-op — the kernel
//! commits one atomic batch per transaction).
//!
//! Decode order is magic → version → type → checksum, so an
//! unknown-but-clean version or frame type is Unsupported (a newer-format
//! file is not damaged); damaged bytes are Corrupt. Replay walks frames
//! with the KSE-082B classifier shape: a failure with nothing valid after
//! it is a torn tail (kill mid-append — the caller truncates there);
//! damage followed by a valid frame fails closed.

use crate::format::{checksum8, Cursor, FormatError};
use crate::identity::{LogicalId, ObjectId, ReplicaId};

pub const WAL_FORMAT_VERSION: u16 = 1;
pub const FRAME_BATCH: u8 = 1;
pub const OP_PUT: u8 = 1;
pub const OP_DELETE: u8 = 2;
/// SE2-M30 — allocate a new object identity (spec §14): the frame carries
/// the (ObjectId, LogicalId, ReplicaId) triple; replay and the live apply
/// both rebuild the identity directories from it.
pub const OP_CREATE_OBJECT: u8 = 3;

const WAL_MAGIC: &[u8; 4] = b"AKWF";
const FRAME_HEADER_LEN: usize = 19; // magic 4 + version 2 + type 1 + seq 8 + payload_len 4

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
    /// `oid 16 | lid 8 | rid 8` after the op byte — fixed width, no
    /// length prefixes (a CreateObject has no key or value).
    CreateObject {
        oid: ObjectId,
        lid: LogicalId,
        rid: ReplicaId,
    },
}

impl Op {
    pub fn key(&self) -> &[u8] {
        match self {
            Op::Put(k, _) | Op::Delete(k) => k,
            Op::CreateObject { .. } => &[],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalFrame {
    pub seq: u64,
    pub ops: Vec<Op>,
}

pub fn encode_frame(seq: u64, ops: &[Op]) -> Result<Vec<u8>, FormatError> {
    if ops.is_empty() {
        return Err(FormatError::Invalid("WAL frame with no ops".into()));
    }
    let mut payload = Vec::new();
    payload.extend_from_slice(&(ops.len() as u32).to_le_bytes());
    for op in ops {
        match op {
            Op::Put(k, v) => {
                payload.push(OP_PUT);
                payload.extend_from_slice(&(k.len() as u32).to_le_bytes());
                payload.extend_from_slice(k);
                payload.extend_from_slice(&(v.len() as u32).to_le_bytes());
                payload.extend_from_slice(v);
            }
            Op::Delete(k) => {
                payload.push(OP_DELETE);
                payload.extend_from_slice(&(k.len() as u32).to_le_bytes());
                payload.extend_from_slice(k);
            }
            Op::CreateObject { oid, lid, rid } => {
                payload.push(OP_CREATE_OBJECT);
                payload.extend_from_slice(oid.as_bytes());
                payload.extend_from_slice(&lid.to_bytes());
                payload.extend_from_slice(&rid.to_bytes());
            }
        }
    }
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload.len() + 8);
    frame.extend_from_slice(WAL_MAGIC);
    frame.extend_from_slice(&WAL_FORMAT_VERSION.to_le_bytes());
    frame.push(FRAME_BATCH);
    frame.extend_from_slice(&seq.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&checksum8(&frame));
    Ok(frame)
}

/// Decode ONE frame from the front of `bytes`; returns the frame and its
/// encoded length. Bytes after the frame are not an error here —
/// `replay_frames` owns the stream-level judgement (torn tail vs damage).
pub fn decode_frame(bytes: &[u8]) -> Result<(WalFrame, usize), FormatError> {
    let mut cur = Cursor::new(bytes);
    if cur.take(4)? != WAL_MAGIC {
        return Err(FormatError::Corrupt("WAL frame bad magic".into()));
    }
    let version = cur.u16()?;
    if version != WAL_FORMAT_VERSION {
        return Err(FormatError::Unsupported(format!(
            "WAL frame format version {version} (this build: {WAL_FORMAT_VERSION})"
        )));
    }
    let frame_type = cur.u8()?;
    if frame_type != FRAME_BATCH {
        return Err(FormatError::Unsupported(format!(
            "WAL frame type {frame_type} (this build: {FRAME_BATCH})"
        )));
    }
    let seq = cur.u64()?;
    let payload_len = cur.u32()? as usize;
    let payload = cur.take(payload_len)?; // bounds-checked: an impossible
                                          // length fails here, before the
                                          // checksum (it IS truncation)
    let stored_ck = cur.take(8)?;
    let total = FRAME_HEADER_LEN + payload_len + 8;
    if checksum8(&bytes[..total - 8]) != stored_ck {
        return Err(FormatError::Corrupt("WAL frame checksum mismatch".into()));
    }

    let mut pcur = Cursor::new(payload);
    let count = pcur.u32()? as usize;
    if count == 0 {
        return Err(FormatError::Corrupt("WAL frame with zero entries".into()));
    }
    let mut ops = Vec::with_capacity(count);
    for _ in 0..count {
        let op = pcur.u8()?;
        match op {
            OP_PUT => {
                let key = pcur.vec()?;
                let value = pcur.vec()?;
                ops.push(Op::Put(key, value));
            }
            OP_DELETE => {
                let key = pcur.vec()?;
                ops.push(Op::Delete(key));
            }
            OP_CREATE_OBJECT => {
                let oid = ObjectId::from_bytes(pcur.take(16)?.try_into().expect("16-byte slice"));
                let lid = LogicalId::from_bytes(pcur.take(8)?.try_into().expect("8-byte slice"));
                let rid = ReplicaId::from_bytes(pcur.take(8)?.try_into().expect("8-byte slice"));
                ops.push(Op::CreateObject { oid, lid, rid });
            }
            other => {
                return Err(FormatError::Unsupported(format!("WAL op byte {other}")));
            }
        }
    }
    if !pcur.is_empty() {
        return Err(FormatError::Corrupt("WAL payload trailing bytes".into()));
    }
    Ok((WalFrame { seq, ops }, total))
}

/// Walk frames from the start. Returns the valid prefix and the byte count
/// it covers. A decode failure with nothing valid after it is a torn tail
/// (the caller truncates the WAL there); damage followed by a valid frame
/// is Corrupt — the WAL must not be trusted. Sequences must strictly
/// increase.
pub fn replay_frames(bytes: &[u8]) -> Result<(Vec<WalFrame>, usize), FormatError> {
    let mut frames = Vec::new();
    let mut pos = 0;
    let mut last_seq: Option<u64> = None;
    while pos < bytes.len() {
        match decode_frame(&bytes[pos..]) {
            Ok((frame, len)) => {
                if let Some(prev) = last_seq {
                    if frame.seq <= prev {
                        return Err(FormatError::Corrupt(format!(
                            "WAL sequence must increase: {frame:?} after {prev}"
                        )));
                    }
                }
                last_seq = Some(frame.seq);
                frames.push(frame);
                pos += len;
            }
            Err(_) => {
                // ponytail: O(n²) probe for a valid frame after the damage —
                // the active WAL is bounded, linear resync would be M3 polish.
                for probe in pos + 1..bytes.len() {
                    if decode_frame(&bytes[probe..]).is_ok() {
                        return Err(FormatError::Corrupt(format!(
                            "WAL damage at offset {pos} with valid frames after"
                        )));
                    }
                }
                return Ok((frames, pos));
            }
        }
    }
    Ok((frames, pos))
}
