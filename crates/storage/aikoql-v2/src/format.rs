//! SE2-M0 format contracts — CURRENT, manifest, checksums, atomic
//! publication (docs/AIKOQL_Storage_Engine_V2_Production_Design.md §17–§20,
//! docs/IMPLEMENTATION-PLAN-V2.md SE2-M0).
//!
//! On-disk shapes (all integers little-endian):
//!
//! CURRENT — fixed 22 bytes:
//! `AKCV | format_version u16 | manifest_generation u64 | sha256-8(first 14)`
//!
//! MANIFEST:
//! `AKMV | format_version u16 | generation u64 | segment_count u32 |
//!  segment records | wal_count u32 | wal ids u64 | sha256-8(everything before)`
//! Segment record:
//! `segment_id u64 | level u8 | key_min_len u32 | key_min | key_max_len u32 |
//!  key_max | seq_lo u64 | seq_hi u64 | record_count u64 | file_size u64 |
//!  checksum u64`
//!
//! Publication is write-temp → fsync → rename (atomic on POSIX and NTFS).
//! Decode order is magic → version → checksum: an unknown-but-integrity-clean
//! version classifies as Unsupported (a newer-format file is not damaged);
//! anything else fails closed as Corrupt. Filesystem conditions are Io —
//! in particular a missing CURRENT is Io, which the future open() path
//! treats as a fresh database, not corruption.

use aikoql_kernel::knowledge::kom::sha256;
use std::fmt;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const CURRENT_LEN: usize = 22;
pub const FORMAT_VERSION: u16 = 1;

const CURRENT_MAGIC: &[u8; 4] = b"AKCV";
const MANIFEST_MAGIC: &[u8; 4] = b"AKMV";
/// Minimum encoded size of one segment record (both keys empty).
const MIN_SEGMENT_RECORD: usize = 57;

#[derive(Debug, Clone)]
pub enum FormatError {
    /// Byte-level damage: bad magic, bad checksum, truncation, impossible
    /// structure. The file must not be trusted.
    Corrupt(String),
    /// Integrity-clean but a newer format this build does not know.
    Unsupported(String),
    /// Filesystem condition (missing, permissions, rename failure…).
    Io(String),
    /// Caller misuse — rejected before anything touches disk (empty segment,
    /// duplicate (key, seq), zero block target…).
    Invalid(String),
    /// The database directory is held by another process (design §19:
    /// one process owns one database directory).
    Locked(String),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Corrupt(m) => write!(f, "corrupt: {m}"),
            FormatError::Unsupported(m) => write!(f, "unsupported: {m}"),
            FormatError::Io(m) => write!(f, "io: {m}"),
            FormatError::Invalid(m) => write!(f, "invalid: {m}"),
            FormatError::Locked(m) => write!(f, "locked: {m}"),
        }
    }
}

impl std::error::Error for FormatError {}

/// v1 convention: the first 8 bytes of the sha256 carry the integrity
/// check — an INTEGRITY FINGERPRINT (PR#2 review SE-07 threat model):
/// accidental corruption detection YES (false-positive ~2^-64), cryptographic
/// authenticity NO, attacker modification resistance NO. Anything adversarial
/// belongs to the encrypted envelope (MRFC-0020); this checksum only catches
/// bit rot, truncation and torn tails.
pub fn checksum8(bytes: &[u8]) -> [u8; 8] {
    let full = sha256(bytes);
    full[..8].try_into().expect("sha256-8 slice")
}

// ---------------------------------------------------------------------------
// CURRENT

#[derive(Debug, Clone)]
pub struct Current {
    pub format_version: u16,
    pub manifest_generation: u64,
}

impl Current {
    pub fn new(format_version: u16, manifest_generation: u64) -> Self {
        Current {
            format_version,
            manifest_generation,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(CURRENT_LEN);
        bytes.extend_from_slice(CURRENT_MAGIC);
        bytes.extend_from_slice(&self.format_version.to_le_bytes());
        bytes.extend_from_slice(&self.manifest_generation.to_le_bytes());
        bytes.extend_from_slice(&checksum8(&bytes));
        debug_assert_eq!(bytes.len(), CURRENT_LEN);
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        if bytes.len() != CURRENT_LEN {
            return Err(FormatError::Corrupt(format!(
                "CURRENT must be {CURRENT_LEN} bytes, got {}",
                bytes.len()
            )));
        }
        if &bytes[0..4] != CURRENT_MAGIC {
            return Err(FormatError::Corrupt("CURRENT bad magic".into()));
        }
        let format_version = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
        if format_version != FORMAT_VERSION {
            return Err(FormatError::Unsupported(format!(
                "CURRENT format version {format_version} (this build: {FORMAT_VERSION})"
            )));
        }
        if checksum8(&bytes[..14]) != bytes[14..22] {
            return Err(FormatError::Corrupt("CURRENT checksum mismatch".into()));
        }
        Ok(Current {
            format_version,
            manifest_generation: u64::from_le_bytes(bytes[6..14].try_into().unwrap()),
        })
    }

    pub fn read(path: &Path) -> Result<Self, FormatError> {
        let bytes = std::fs::read(path)
            .map_err(|e| FormatError::Io(format!("read CURRENT {}: {e}", path.display())))?;
        Self::decode(&bytes)
    }

    pub fn publish(path: &Path, current: &Self) -> Result<(), FormatError> {
        publish_atomic(path, &current.encode())
    }
}

// ---------------------------------------------------------------------------
// Manifest

#[derive(Debug, Clone)]
pub struct SegmentRecord {
    pub segment_id: u64,
    pub level: u8,
    pub key_min: Vec<u8>,
    pub key_max: Vec<u8>,
    pub seq_lo: u64,
    pub seq_hi: u64,
    pub record_count: u64,
    pub file_size: u64,
    pub checksum: u64,
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub format_version: u16,
    pub generation: u64,
    pub segments: Vec<SegmentRecord>,
    pub wal_ids: Vec<u64>,
}

impl Manifest {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MANIFEST_MAGIC);
        bytes.extend_from_slice(&self.format_version.to_le_bytes());
        bytes.extend_from_slice(&self.generation.to_le_bytes());
        bytes.extend_from_slice(&(self.segments.len() as u32).to_le_bytes());
        for s in &self.segments {
            bytes.extend_from_slice(&s.segment_id.to_le_bytes());
            bytes.push(s.level);
            bytes.extend_from_slice(&(s.key_min.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&s.key_min);
            bytes.extend_from_slice(&(s.key_max.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&s.key_max);
            bytes.extend_from_slice(&s.seq_lo.to_le_bytes());
            bytes.extend_from_slice(&s.seq_hi.to_le_bytes());
            bytes.extend_from_slice(&s.record_count.to_le_bytes());
            bytes.extend_from_slice(&s.file_size.to_le_bytes());
            bytes.extend_from_slice(&s.checksum.to_le_bytes());
        }
        bytes.extend_from_slice(&(self.wal_ids.len() as u32).to_le_bytes());
        for id in &self.wal_ids {
            bytes.extend_from_slice(&id.to_le_bytes());
        }
        bytes.extend_from_slice(&checksum8(&bytes));
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut cur = Cursor::new(bytes);
        if cur.take(4)? != MANIFEST_MAGIC {
            return Err(FormatError::Corrupt("manifest bad magic".into()));
        }
        let format_version = cur.u16()?;
        if format_version != FORMAT_VERSION {
            return Err(FormatError::Unsupported(format!(
                "manifest format version {format_version} (this build: {FORMAT_VERSION})"
            )));
        }
        let generation = cur.u64()?;
        let segment_count = cur.u32()? as usize;
        // Plausibility cap before allocating: each record is at least
        // MIN_SEGMENT_RECORD bytes, so a count that cannot fit is corruption
        // (and must not drive a huge allocation).
        if segment_count > cur.remaining() / MIN_SEGMENT_RECORD {
            return Err(FormatError::Corrupt(format!(
                "manifest segment_count {segment_count} cannot fit in {} remaining bytes",
                cur.remaining()
            )));
        }
        let mut segments = Vec::with_capacity(segment_count);
        for _ in 0..segment_count {
            let segment_id = cur.u64()?;
            let level = cur.u8()?;
            let key_min = cur.vec()?;
            let key_max = cur.vec()?;
            let seq_lo = cur.u64()?;
            let seq_hi = cur.u64()?;
            let record_count = cur.u64()?;
            let file_size = cur.u64()?;
            let checksum = cur.u64()?;
            segments.push(SegmentRecord {
                segment_id,
                level,
                key_min,
                key_max,
                seq_lo,
                seq_hi,
                record_count,
                file_size,
                checksum,
            });
        }
        let wal_count = cur.u32()? as usize;
        if wal_count > cur.remaining() / 8 {
            return Err(FormatError::Corrupt(format!(
                "manifest wal_count {wal_count} cannot fit in {} remaining bytes",
                cur.remaining()
            )));
        }
        let mut wal_ids = Vec::with_capacity(wal_count);
        for _ in 0..wal_count {
            wal_ids.push(cur.u64()?);
        }
        let checksum = cur.take(8)?.to_vec();
        if !cur.is_empty() {
            return Err(FormatError::Corrupt("manifest trailing bytes".into()));
        }
        if checksum8(&bytes[..bytes.len() - 8]) != checksum[..] {
            return Err(FormatError::Corrupt("manifest checksum mismatch".into()));
        }
        Ok(Manifest {
            format_version,
            generation,
            segments,
            wal_ids,
        })
    }

    pub fn read(path: &Path) -> Result<Self, FormatError> {
        let bytes = std::fs::read(path)
            .map_err(|e| FormatError::Io(format!("read manifest {}: {e}", path.display())))?;
        Self::decode(&bytes)
    }

    pub fn publish(path: &Path, manifest: &Self) -> Result<(), FormatError> {
        Self::publish_staged(path, manifest, None)
    }

    /// SE2-M36 — the §38 crash windows on the compaction path.
    pub fn publish_staged(
        path: &Path,
        manifest: &Self,
        stage: Option<&str>,
    ) -> Result<(), FormatError> {
        publish_atomic_staged(path, &manifest.encode(), stage)
    }
}

/// The CURRENT↔manifest pair check: the pointer must name the manifest that
/// is actually on disk, otherwise publication half-failed or a file is
/// wrong. Fail closed — never pick.
pub fn verify_pair(current: &Current, manifest: &Manifest) -> Result<(), FormatError> {
    if current.manifest_generation != manifest.generation {
        return Err(FormatError::Corrupt(format!(
            "CURRENT points at generation {} but manifest is generation {}",
            current.manifest_generation, manifest.generation
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Publication + decode cursor

/// write temp (via the closure) → fsync → rename over the target →
/// best-effort directory fsync. The temp lives beside the target (same
/// volume) so the rename is atomic; a torn write is only ever in the temp,
/// which nobody reads. The closure's value is returned after the rename.
/// SE2-M36 — the staged variant parks at the §38 injection points:
/// `AIKOQL_V2_PLACE_PARK` naming `FAIL_AFTER_{stage}_WRITE` / `_FSYNC`
/// holds the process at the exact boundary (the child-kill harness kills
/// it there); `stage` None disables (production and every unstaged path).
pub(crate) fn publish_atomic_writer<T>(
    path: &Path,
    write: impl FnOnce(&mut File) -> std::io::Result<T>,
) -> Result<T, FormatError> {
    publish_atomic_writer_staged(path, None, write)
}

pub(crate) fn publish_atomic_writer_staged<T>(
    path: &Path,
    stage: Option<&str>,
    write: impl FnOnce(&mut File) -> std::io::Result<T>,
) -> Result<T, FormatError> {
    let dir = path.parent().ok_or_else(|| {
        FormatError::Io(format!("publish {}: no parent directory", path.display()))
    })?;
    let name = path
        .file_name()
        .ok_or_else(|| FormatError::Io("publish: no file name".into()))?
        .to_string_lossy();
    let tmp: PathBuf = dir.join(format!(".{name}.tmp.{}", std::process::id()));
    let written = (|| -> std::io::Result<T> {
        let mut f = File::create(&tmp)?;
        let v = write(&mut f)?;
        if let Some(stage) = stage {
            crash_park(
                "AIKOQL_V2_PLACE_PARK",
                dir,
                &format!("FAIL_AFTER_{stage}_WRITE"),
            );
        }
        f.sync_all()?;
        if let Some(stage) = stage {
            crash_park(
                "AIKOQL_V2_PLACE_PARK",
                dir,
                &format!("FAIL_AFTER_{stage}_FSYNC"),
            );
        }
        drop(f);
        std::fs::rename(&tmp, path)?;
        Ok(v)
    })();
    match written {
        Ok(v) => {
            // Best-effort: flush the directory entry so the rename itself is
            // durable. On Windows std cannot open a directory handle (NTFS
            // rename metadata is journaled anyway) — ignore that failure.
            if let Ok(d) = File::open(dir) {
                let _ = d.sync_all();
            }
            Ok(v)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(FormatError::Io(format!("publish {}: {e}", path.display())))
        }
    }
}

pub(crate) fn publish_atomic(path: &Path, bytes: &[u8]) -> Result<(), FormatError> {
    publish_atomic_writer(path, |f| f.write_all(bytes))
}

pub(crate) fn publish_atomic_staged(
    path: &Path,
    bytes: &[u8],
    stage: Option<&str>,
) -> Result<(), FormatError> {
    publish_atomic_writer_staged(path, stage, |f| f.write_all(bytes))
}

/// Park forever when `var` names this stage — the crash-window harness
/// (no-op unset). The marker file tells the parent the park was reached.
/// SE2-M36 — moved here from db.rs: the staged publishers (same crate)
/// park at the write/fsync boundaries.
pub(crate) fn crash_park(var: &str, dir: &Path, stage: &str) {
    if std::env::var(var).ok().as_deref() != Some(stage) {
        return;
    }
    std::fs::write(dir.join(stage), b"1").ok();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

pub(crate) struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Cursor { bytes, pos: 0 }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        if self.remaining() < n {
            return Err(FormatError::Corrupt(format!(
                "truncated: need {n} bytes at offset {}, {} remain",
                self.pos,
                self.remaining()
            )));
        }
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, FormatError> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, FormatError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, FormatError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, FormatError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    /// A u32 length-prefixed byte string (the cursor bounds the read).
    pub(crate) fn vec(&mut self) -> Result<Vec<u8>, FormatError> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }
}
