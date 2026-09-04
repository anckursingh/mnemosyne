//! SE2-M3 — legacy WAL migration (design §23, IMPLEMENTATION-PLAN-V2
//! SE2-M3): decode the v1 envelope + batch payload, feed the batches into a
//! fresh v2 Db (segments → manifest → CURRENT by the normal flush path),
//! reopen and verify every migrated key against the state decoded from the
//! source — and only then report done. The source WAL is never modified or
//! deleted: the operator's retention policy decides, the migrator reports.
//!
//! The envelope parser is v1's own validated reader (`envelope::parse_at` —
//! magic/version/type/checksum); only the frozen payload codec is
//! re-implemented here (v1's `decode_batch` is private, and this crate
//! stays decoupled from v1's internals).

use crate::db::{Config, Db};
use crate::format::FormatError;
use crate::wal::Op;
use aikoql_storage::envelope::{self, ParseOutcome};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct MigrationReport {
    pub batches: u64,
    pub puts: u64,
    pub deletes: u64,
    /// Distinct keys in the migrated final state.
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

/// Migrate a v1 WAL file into a fresh v2 database (§23 pipeline: decode →
/// build segments → manifest → atomically publish CURRENT → reopen and
/// verify → only then report done). The source file is read-only here —
/// never modified, never deleted.
pub fn migrate_v1_wal(source: &Path, config: Config) -> Result<MigrationReport, FormatError> {
    let bytes = std::fs::read(source)
        .map_err(|e| FormatError::Io(format!("read {}: {e}", source.display())))?;

    let mut batches: Vec<Vec<Op>> = Vec::new();
    let mut expected: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
    let mut puts = 0u64;
    let mut deletes = 0u64;
    let mut torn_tail_dropped = false;
    let mut pos = 0usize;
    while pos < bytes.len() {
        match envelope::parse_at(&bytes, pos) {
            Ok(ParseOutcome::Complete { payload, end }) => {
                let ops = decode_legacy_batch(&payload)?;
                // v1 apply order: puts before dels (KSE-006)
                for op in &ops {
                    match op {
                        Op::Put(k, v) => {
                            puts += 1;
                            expected.insert(k.clone(), Some(v.clone()));
                        }
                        Op::Delete(k) => {
                            deletes += 1;
                            expected.insert(k.clone(), None);
                        }
                    }
                }
                batches.push(ops);
                pos = end;
            }
            Ok(ParseOutcome::TornTail) => {
                if valid_record_after(&bytes, pos) {
                    return Err(FormatError::Corrupt(
                        "truncated record followed by valid data".into(),
                    ));
                }
                torn_tail_dropped = true;
                break;
            }
            // v1 reports corruption/incompatibility as KError::Store; every
            // reachable case here is damage (the format version is frozen
            // at 1, so an unknown version would need a forged checksum too).
            Err(e) => return Err(FormatError::Corrupt(e.to_string())),
        }
    }

    // Feed the batches through the normal write path (segments → manifest →
    // CURRENT by the standard flush machinery), then close and reopen.
    {
        let db = Db::open(config.clone())?;
        for ops in &batches {
            db.write(ops)?;
        }
        db.flush()?;
    }
    let db = Db::open(config.clone())?;
    for (key, want) in &expected {
        let got = db.get(key)?;
        if got.as_deref() != want.as_deref() {
            return Err(FormatError::Corrupt(format!(
                "migration verification failed for key {}: expected {want:?}, got {got:?}",
                String::from_utf8_lossy(key)
            )));
        }
    }

    Ok(MigrationReport {
        batches: batches.len() as u64,
        puts,
        deletes,
        keys: expected.len() as u64,
        torn_tail_dropped,
        source: source.to_path_buf(),
        dest_dir: config.dir.clone(),
    })
}
