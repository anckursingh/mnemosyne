//! SE2-M3 — legacy WAL migration (design §23, IMPLEMENTATION-PLAN-V2
//! SE2-M3): decode the v1 envelope + batch payload, feed the batches into a
//! fresh v2 Db (segments → manifest → CURRENT by the normal flush path),
//! reopen and verify — and only then report done. The source WAL is never
//! modified or deleted: the operator's retention policy decides, the
//! migrator reports.
//!
//! PR#2 review SE-04: the migration streams — frames are read in bounded
//! chunks, decoded one at a time, applied to the destination and verified
//! against it immediately, then discarded. Memory is O(chunk + largest
//! frame), never the complete WAL, all decoded batches, or a full
//! expected-state map. (A corrupted header claiming a huge payload still
//! grows the carry buffer until EOF fails closed — no worse than the
//! pre-streaming whole-file read, and the corruption path ends in an error.)
//!
//! Verification state is the destination itself: every frame's net effect
//! is compared against the live Db right after it is applied (stronger than
//! a running hash — the check is against the destination's actual
//! post-state), and a state fingerprint taken before close and after reopen
//! pins the flush/reopen round-trip.
//!
//! The envelope parser is v1's own validated reader (`envelope::parse_at` —
//! magic/version/type/checksum); only the frozen payload codec is
//! re-implemented here (v1's `decode_batch` is private, and this crate
//! stays decoupled from v1's internals).

use crate::db::{Config, Db};
use crate::format::FormatError;
use crate::wal::Op;
use aikoql_storage::envelope::{self, ParseOutcome};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Read-ahead for the streaming pass: memory is bounded by this plus one
/// frame (a larger frame grows the carry until it completes or EOF fails
/// closed — see the module doc).
const CHUNK: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub struct MigrationReport {
    pub batches: u64,
    pub puts: u64,
    pub deletes: u64,
    /// Live keys in the migrated final state (counted on the reopened
    /// destination — not distinct keys ever written).
    pub keys: u64,
    pub torn_tail_dropped: bool,
    pub source: PathBuf,
    pub dest_dir: PathBuf,
}

// Frozen v1 batch payload codec (aikoql/src/lib.rs) — the layout the
// certified engine writes in production:
// [u16 n_puts] (u32 klen, k, u32 vlen, v)* [u16 n_dels] (u32 klen, k)*  (LE)
struct LegacyCursor<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> LegacyCursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        if self.b.len() - self.pos < n {
            return Err(FormatError::Corrupt(
                "legacy batch payload truncated".into(),
            ));
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u16(&mut self) -> Result<u16, FormatError> {
        let raw: [u8; 2] = self.take(2)?.try_into().unwrap();
        Ok(u16::from_le_bytes(raw))
    }

    fn u32(&mut self) -> Result<u32, FormatError> {
        let raw: [u8; 4] = self.take(4)?.try_into().unwrap();
        Ok(u32::from_le_bytes(raw))
    }
}

fn decode_legacy_batch(payload: &[u8]) -> Result<Vec<Op>, FormatError> {
    let mut c = LegacyCursor { b: payload, pos: 0 };
    let mut ops = Vec::new();
    for _ in 0..c.u16()? {
        let klen = c.u32()? as usize;
        let k = c.take(klen)?.to_vec();
        let vlen = c.u32()? as usize;
        let v = c.take(vlen)?.to_vec();
        ops.push(Op::Put(k, v));
    }
    for _ in 0..c.u16()? {
        let klen = c.u32()? as usize;
        ops.push(Op::Delete(c.take(klen)?.to_vec()));
    }
    if c.pos != payload.len() {
        return Err(FormatError::Corrupt(
            "trailing bytes in legacy record".into(),
        ));
    }
    Ok(ops)
}

/// Does a complete, checksum-verified record exist at any offset after
/// `pos`? A torn-looking record followed by valid data is middle
/// corruption, not a crash tail (KSE-082B, ported from v1's replay). The
/// scan errs in the safe direction: a false positive needs a whole valid
/// record to hide inside the tail (~2^-64 per candidate checksum), and it
/// would fail closed where recovery could have proceeded — never the reverse.
/// ponytail: O(remaining bytes), once per migration, only when a torn tail
/// exists.
fn valid_record_after(bytes: &[u8], pos: usize) -> bool {
    (pos + 1..bytes.len()).any(|off| {
        matches!(
            envelope::parse_at(bytes, off),
            Ok(ParseOutcome::Complete { .. })
        )
    })
}

/// Apply one decoded frame to the destination and verify its net effect
/// against the live Db immediately (SE-04 "update verification state"): the
/// last op per key within the frame must be the destination's post-state.
/// Earlier duplicates of a key are skipped — their intermediate states are
/// not observable once the frame is applied.
fn apply_and_verify(
    db: &Db,
    ops: &[Op],
    puts: &mut u64,
    deletes: &mut u64,
) -> Result<(), FormatError> {
    for op in ops {
        match op {
            Op::Put(..) => *puts += 1,
            Op::Delete(..) => *deletes += 1,
            // SE2-M30/M33 — v1 WAL frames cannot carry ops 3..5 (the v1
            // cursor decodes only ops 1/2), so these arms are unreachable;
            // they exist for the exhaustive match only.
            Op::CreateObject { .. } | Op::PutObject(..) | Op::DeleteObject(..) => {}
        }
    }
    db.write(ops)?;
    let mut seen: HashSet<&[u8]> = HashSet::new();
    for op in ops.iter().rev() {
        let (key, want) = match op {
            Op::Put(k, v) => (&k[..], Some(&v[..])),
            Op::Delete(k) => (&k[..], None),
            // no key to verify (v1 frames cannot carry these)
            Op::CreateObject { .. } | Op::PutObject(..) | Op::DeleteObject(..) => continue,
        };
        if !seen.insert(key) {
            continue; // overwritten by a later op in this frame
        }
        let got = db.get(key)?;
        if got.as_deref() != want {
            return Err(FormatError::Corrupt(format!(
                "frame verification failed for key {}: expected {want:?}, got {got:?}",
                String::from_utf8_lossy(key)
            )));
        }
    }
    Ok(())
}

/// The destination's complete state as a (live-key count, digest) pair.
/// DefaultHasher is process-local: the two fingerprints are compared within
/// one process run, which is the only claim made here.
/// ponytail: upgrade to sha256 only if the digest is ever reported.
fn fingerprint(db: &Db) -> Result<(u64, u64), FormatError> {
    let rows = db.scan(b"")?;
    let mut hasher = DefaultHasher::new();
    for (k, v) in &rows {
        k.len().hash(&mut hasher);
        k.hash(&mut hasher);
        v.len().hash(&mut hasher);
        v.hash(&mut hasher);
    }
    Ok((rows.len() as u64, hasher.finish()))
}

/// Migrate a v1 WAL file into a fresh v2 database (§23 pipeline: decode →
/// build segments → manifest → atomically publish CURRENT → reopen and
/// verify → only then report done). The source file is read-only here —
/// never modified, never deleted.
pub fn migrate_v1_wal(source: &Path, config: Config) -> Result<MigrationReport, FormatError> {
    let mut file = std::fs::File::open(source)
        .map_err(|e| FormatError::Io(format!("read {}: {e}", source.display())))?;

    let db = Db::open(config.clone())?;
    let mut buf: Vec<u8> = Vec::new();
    let mut pos = 0usize; // invariant: bytes before `pos` are consumed frames
    let mut eof = false;
    let mut batches = 0u64;
    let mut puts = 0u64;
    let mut deletes = 0u64;
    let mut torn_tail_dropped = false;
    let mut chunk = vec![0u8; CHUNK];
    loop {
        match envelope::parse_at(&buf, pos) {
            Ok(ParseOutcome::Complete { payload, end }) => {
                let ops = decode_legacy_batch(&payload)?;
                apply_and_verify(&db, &ops, &mut puts, &mut deletes)?;
                batches += 1;
                pos = end;
                if pos >= CHUNK {
                    buf.drain(..pos); // amortized: only past the read-ahead
                    pos = 0;
                }
            }
            Ok(ParseOutcome::TornTail) => {
                if eof {
                    if !buf[pos..].is_empty() {
                        if valid_record_after(&buf, pos) {
                            return Err(FormatError::Corrupt(
                                "truncated record followed by valid data".into(),
                            ));
                        }
                        torn_tail_dropped = true;
                    }
                    break;
                }
                if pos > 0 {
                    buf.drain(..pos); // compact before growing
                    pos = 0;
                }
                let n = file
                    .read(&mut chunk)
                    .map_err(|e| FormatError::Io(format!("read {}: {e}", source.display())))?;
                if n == 0 {
                    eof = true;
                } else {
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
            // v1 reports corruption/incompatibility as KError::Store; every
            // reachable case here is damage (the format version is frozen
            // at 1, so an unknown version would need a forged checksum too).
            Err(e) => return Err(FormatError::Corrupt(e.to_string())),
        }
    }
    drop(file);

    // The streaming pass verified every frame against the live destination;
    // pin the flush/reopen round-trip: the reopened state must fingerprint
    // identically, and its live-key count is the report cell.
    db.flush()?;
    let before = fingerprint(&db)?;
    drop(db);
    let db = Db::open(config.clone())?;
    let after = fingerprint(&db)?;
    if before != after {
        return Err(FormatError::Corrupt(format!(
            "flush/reopen changed the migrated state ({} live keys -> {})",
            before.0, after.0
        )));
    }

    Ok(MigrationReport {
        batches,
        puts,
        deletes,
        keys: after.0,
        torn_tail_dropped,
        source: source.to_path_buf(),
        dest_dir: config.dir.clone(),
    })
}
