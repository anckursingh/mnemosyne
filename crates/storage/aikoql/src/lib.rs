//! AIKOQL-native storage engine (MRFC-KSE-001).
//!
//! Experimental backend behind the kernel's `StorageEngine` trait. The
//! adoption gate passed (TDD doc §29; artifacts/storage-engine/) and aikoql
//! served as the production default for one cycle; PR#2 external review
//! (SE-03) reverted that default: open replays the FULL WAL (startup time
//! and memory are O(WAL size)) and the WAL grows unbounded — there is no
//! checkpointing. Deployment boundary: aikoql suits query-heavy,
//! RAM-affordant, bounded datasets; opt in via `--backend aikoql` /
//! `AIKOQL_BACKEND=aikoql` / `storage.backend = "aikoql"`
//! (docs/STORAGE-BACKENDS.md). Re-adoption as default requires
//! checkpointing with bounded replay plus a redb → aikoql migration story
//! (artifacts/storage-engine/adoption-decision.md addendum).
//!
//! KSE-1 skeleton: an append-only write-ahead log over the kernel's
//! `MemoryEngine` reference semantics. Each batch is serialized to one
//! enveloped log record (magic/format-version/checksum — KSE-3), fsynced,
//! then applied to the in-memory map — durable before visible,
//! all-or-nothing. Open replays the log; a torn tail record (crash
//! mid-append) is truncated, corruption fails closed.
//! ponytail: the log grows unbounded — the checksummed sorted-block format
//! (`Block`, KSE-4) is the compaction unit, but no doc phase mandates
//! compaction, so the engine still replays the full WAL on open. Wire blocks
//! into open/write when the resource phase (KSE-16..19) or a measurement
//! shows replay cost matters.

use aikoql_kernel::knowledge::kom::{KError, KResult};
use aikoql_kernel::storage::store::{MemoryEngine, StorageEngine, WriteBatch};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Mutex;

mod block;
pub mod envelope; // KSE-9 fault injection + tooling need record boundaries

pub use block::Block;

fn se(e: impl std::fmt::Display) -> KError {
    KError::Store(format!("aikoql-storage: {}", e))
}

fn corrupt(what: &str) -> KError {
    KError::Store(format!("aikoql-storage: corrupt log: {}", what))
}

fn poisoned() -> KError {
    KError::Store("aikoql-storage: log lock poisoned".into())
}

/// AIKOQL-native engine: WAL file + in-memory sorted map.
pub struct AikoqlStorageEngine {
    log: Mutex<File>,
    mem: MemoryEngine,
}

// --- batch payload codec (inside the envelope; see envelope.rs) ---
//
// Payload: [u16 n_puts] (u32 klen, k, u32 vlen, v)* [u16 n_dels] (u32 klen, k)*

fn push_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn encode_batch(batch: &WriteBatch) -> Vec<u8> {
    let mut p = Vec::new();
    push_u16(&mut p, batch.puts.len() as u16);
    for (k, v) in &batch.puts {
        push_u32(&mut p, k.len() as u32);
        p.extend_from_slice(k);
        push_u32(&mut p, v.len() as u32);
        p.extend_from_slice(v);
    }
    push_u16(&mut p, batch.dels.len() as u16);
    for k in &batch.dels {
        push_u32(&mut p, k.len() as u32);
        p.extend_from_slice(k);
    }
    p
}

struct Cursor<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> KResult<&'a [u8]> {
        if self.b.len() - self.pos < n {
            return Err(corrupt("record truncated"));
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u16(&mut self) -> KResult<u16> {
        let raw: [u8; 2] = self.take(2)?.try_into().unwrap();
        Ok(u16::from_le_bytes(raw))
    }

    fn u32(&mut self) -> KResult<u32> {
        let raw: [u8; 4] = self.take(4)?.try_into().unwrap();
        Ok(u32::from_le_bytes(raw))
    }

    fn u32_be(&mut self) -> KResult<u32> {
        let raw: [u8; 4] = self.take(4)?.try_into().unwrap();
        Ok(u32::from_be_bytes(raw))
    }
}

fn decode_batch(payload: &[u8]) -> KResult<WriteBatch> {
    let mut c = Cursor { b: payload, pos: 0 };
    let mut batch = WriteBatch::new();
    for _ in 0..c.u16()? {
        let klen = c.u32()? as usize;
        let k = c.take(klen)?.to_vec();
        let vlen = c.u32()? as usize;
        let v = c.take(vlen)?.to_vec();
        batch.put(k, v);
    }
    for _ in 0..c.u16()? {
        let klen = c.u32()? as usize;
        batch.del(c.take(klen)?.to_vec());
    }
    if c.pos != payload.len() {
        return Err(corrupt("trailing bytes in record"));
    }
    Ok(batch)
}

/// Does a complete, checksum-verified record exist at any offset after
/// `pos`? A torn-looking record followed by valid data is middle
/// corruption (KSE-082B), not a crash tail. The scan errs in the safe
/// direction: a false positive needs a whole valid record to hide inside
/// the tail (~2^-64 per candidate checksum), and it would fail closed
/// where recovery could have proceeded — never the reverse.
/// PR#2 review SE-10: only offsets whose 4-byte window is the WAL magic can
/// start a record (parse_at rejects anything else), so the full parse runs
/// only on magic matches. ponytail: the window scan is still O(remaining
/// bytes), once per open, and only when a torn tail exists — clean opens
/// and full replay never pay it. Streaming replay with a bounded buffer
/// belongs to checkpointing (SE-03 re-adoption conditions).
fn valid_record_after(bytes: &[u8], pos: usize) -> bool {
    bytes[pos + 1..]
        .windows(envelope::MAGIC.len())
        .enumerate()
        .filter(|(_, w)| *w == envelope::MAGIC)
        .any(|(i, _)| {
            matches!(
                envelope::parse_at(bytes, pos + 1 + i),
                Ok(envelope::ParseOutcome::Complete { .. })
            )
        })
}

/// Replay `bytes` into a fresh map; returns the offset of the last complete
/// record. A torn tail (crash mid-append) is left out — it was never
/// acknowledged to a caller. Corruption (bad magic, checksum mismatch,
/// unknown type/version) fails closed with a deterministic error.
fn replay(bytes: &[u8], mem: &MemoryEngine) -> KResult<usize> {
    let mut pos = 0usize;
    while pos < bytes.len() {
        match envelope::parse_at(bytes, pos)? {
            envelope::ParseOutcome::Complete { payload, end, .. } => {
                mem.write_batch(&decode_batch(&payload)?)?;
                pos = end;
            }
            envelope::ParseOutcome::TornTail => {
                // A torn tail is legitimate only when nothing complete
                // follows — a middle record whose payload_len was corrupted
                // to overrun EOF would otherwise masquerade as a crash tail
                // and silently truncate acknowledged records (KSE-082B).
                if valid_record_after(bytes, pos) {
                    return Err(corrupt("truncated record followed by valid data"));
                }
                break;
            }
        }
    }
    Ok(pos)
}

impl AikoqlStorageEngine {
    /// Open (or create) a durable store at `path`. Replays the WAL; a torn
    /// tail record is truncated, anything else malformed fails closed.
    pub fn open(path: impl AsRef<Path>) -> KResult<Self> {
        let p = path.as_ref();
        // 1. Read the whole log under a transient read handle. A missing file
        //    is a fresh store.
        let mut bytes = Vec::new();
        match File::open(p) {
            Ok(mut f) => {
                f.read_to_end(&mut bytes)
                    .map_err(|e| se(format!("read: {e}")))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(se(format!("read: {e}"))),
        }
        // 2. Replay into memory.
        let mem = MemoryEngine::new();
        let last_good = replay(&bytes, &mem)?;
        // 3. Drop a torn tail with a transient plain-write handle — on
        //    Windows SetEndOfFile needs FILE_WRITE_DATA, which the append
        //    WAL handle (below) cannot request.
        if last_good != bytes.len() {
            OpenOptions::new()
                .write(true)
                .open(p)
                .and_then(|f| f.set_len(last_good as u64))
                .map_err(|e| se(format!("truncate: {e}")))?;
        }
        // 4. The WAL handle itself: append-only (all reads happened in step 1).
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .map_err(|e| se(format!("open: {e}")))?;
        Ok(AikoqlStorageEngine {
            log: Mutex::new(log),
            mem,
        })
    }
}

impl StorageEngine for AikoqlStorageEngine {
    fn get(&self, key: &[u8]) -> KResult<Option<Vec<u8>>> {
        self.mem.get(key)
    }

    fn scan(&self, prefix: &[u8]) -> KResult<Vec<(Vec<u8>, Vec<u8>)>> {
        self.mem.scan(prefix)
    }

    fn write_batch(&self, batch: &WriteBatch) -> KResult<()> {
        if batch.is_empty() {
            return Ok(()); // KSE-005: no state change, no log record
        }
        let payload = encode_batch(batch);
        let record = envelope::encode_record(envelope::TYPE_BATCH, &payload);
        // WAL: the record is durable before the state change is visible,
        // and the apply happens under the same log lock (KSE-13
        // KSE-120a): log order IS commit order. Applying after releasing
        // the lock would let two writers commit A then B in the log while
        // applying B then A in memory — a crash right then recovers a
        // different store than the one that was serving.
        let mut log = self.log.lock().map_err(|_| poisoned())?;
        log.write_all(&record).map_err(se)?;
        log.sync_data().map_err(se)?;
        // MemoryEngine applies puts before dels — the shared KSE-006 semantics.
        self.mem.write_batch(batch)
    }
}
