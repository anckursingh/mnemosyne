//! SE2-M30 — the identity and replica directories' persistent form
//! (spec §8.1/§8.2, §23 publication order, §24 crash states): per-generation
//! fixed-record delta logs, one family per directory. A log records the
//! records CREATED since the previous flush; recovery applies the logs at
//! or below CURRENT's generation (oldest first) and then the active WAL, so
//! every §24 window lands on exactly one authoritative mapping.
//!
//! On-disk shapes (all little-endian; the crate's sha256-8 checksum):
//! `AKID | format_version u16 | generation u64 | record_count u32 |
//!  records (ObjectId 16 | LogicalId 8) | checksum8`
//! `AKRP | format_version u16 | generation u64 | record_count u32 |
//!  records (LogicalId 8 | NodeId 8 | ReplicaId 8) | checksum8`
//!
//! Publication is the established write-temp → fsync → rename (atomic);
//! decode order is magic → version → structure → checksum — an
//! unknown-but-clean version is Unsupported (a newer-format file is not
//! damaged), anything else fails closed. Files are named
//! `IDENTITY-{gen:06}.log` / `REPLICA-{gen:06}.log` beside the manifest.

use crate::format::{checksum8, publish_atomic, Cursor, FormatError, FORMAT_VERSION};
use crate::identity::{LogicalId, NodeId, ObjectId, ReplicaId};
use std::path::{Path, PathBuf};

const IDENTITY_MAGIC: &[u8; 4] = b"AKID";
const REPLICA_MAGIC: &[u8; 4] = b"AKRP";
/// oid 16 + lid 8 (both fixed width — no length prefixes).
const IDENTITY_RECORD_LEN: usize = 24;
/// lid 8 + node 8 + rid 8 (both fixed width).
const REPLICA_RECORD_LEN: usize = 24;
const HEADER_LEN: usize = 18; // magic 4 + version 2 + generation 8 + count 4

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityRecord {
    pub oid: ObjectId,
    pub lid: LogicalId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicaRecord {
    pub lid: LogicalId,
    pub node: NodeId,
    pub rid: ReplicaId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityLog {
    pub format_version: u16,
    pub generation: u64,
    pub records: Vec<IdentityRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaLog {
    pub format_version: u16,
    pub generation: u64,
    pub records: Vec<ReplicaRecord>,
}

pub fn identity_log_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("IDENTITY-{generation:06}.log"))
}

pub fn replica_log_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("REPLICA-{generation:06}.log"))
}

impl IdentityLog {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes =
            Vec::with_capacity(HEADER_LEN + self.records.len() * IDENTITY_RECORD_LEN + 8);
        bytes.extend_from_slice(IDENTITY_MAGIC);
        bytes.extend_from_slice(&self.format_version.to_le_bytes());
        bytes.extend_from_slice(&self.generation.to_le_bytes());
        bytes.extend_from_slice(&(self.records.len() as u32).to_le_bytes());
        for r in &self.records {
            bytes.extend_from_slice(r.oid.as_bytes());
            bytes.extend_from_slice(&r.lid.to_bytes());
        }
        bytes.extend_from_slice(&checksum8(&bytes));
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut cur = Cursor::new(bytes);
        if cur.take(4)? != IDENTITY_MAGIC {
            return Err(FormatError::Corrupt("identity log bad magic".into()));
        }
        let format_version = cur.u16()?;
        if format_version != FORMAT_VERSION {
            return Err(FormatError::Unsupported(format!(
                "identity log format version {format_version} (this build: {FORMAT_VERSION})"
            )));
        }
        let generation = cur.u64()?;
        let count = cur.u32()? as usize;
        // Plausibility cap before allocating (the manifest precedent): each
        // record is fixed-width, so a count that cannot fit is corruption.
        if count > cur.remaining() / IDENTITY_RECORD_LEN {
            return Err(FormatError::Corrupt(format!(
                "identity log record_count {count} cannot fit in {} remaining bytes",
                cur.remaining()
            )));
        }
        let mut records = Vec::with_capacity(count);
        for _ in 0..count {
            let oid = ObjectId::from_bytes(cur.take(16)?.try_into().expect("16-byte slice"));
            let lid = LogicalId::from_bytes(cur.take(8)?.try_into().expect("8-byte slice"));
            records.push(IdentityRecord { oid, lid });
        }
        let stored_ck = cur.take(8)?;
        if !cur.is_empty() {
            return Err(FormatError::Corrupt("identity log trailing bytes".into()));
        }
        if checksum8(&bytes[..bytes.len() - 8]) != stored_ck {
            return Err(FormatError::Corrupt(
                "identity log checksum mismatch".into(),
            ));
        }
        Ok(IdentityLog {
            format_version,
            generation,
            records,
        })
    }

    pub fn publish(path: &Path, log: &Self) -> Result<(), FormatError> {
        publish_atomic(path, &log.encode())
    }
}

impl ReplicaLog {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes =
            Vec::with_capacity(HEADER_LEN + self.records.len() * REPLICA_RECORD_LEN + 8);
        bytes.extend_from_slice(REPLICA_MAGIC);
        bytes.extend_from_slice(&self.format_version.to_le_bytes());
        bytes.extend_from_slice(&self.generation.to_le_bytes());
        bytes.extend_from_slice(&(self.records.len() as u32).to_le_bytes());
        for r in &self.records {
            bytes.extend_from_slice(&r.lid.to_bytes());
            bytes.extend_from_slice(&r.node.to_bytes());
            bytes.extend_from_slice(&r.rid.to_bytes());
        }
        bytes.extend_from_slice(&checksum8(&bytes));
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut cur = Cursor::new(bytes);
        if cur.take(4)? != REPLICA_MAGIC {
            return Err(FormatError::Corrupt("replica log bad magic".into()));
        }
        let format_version = cur.u16()?;
        if format_version != FORMAT_VERSION {
            return Err(FormatError::Unsupported(format!(
                "replica log format version {format_version} (this build: {FORMAT_VERSION})"
            )));
        }
        let generation = cur.u64()?;
        let count = cur.u32()? as usize;
        if count > cur.remaining() / REPLICA_RECORD_LEN {
            return Err(FormatError::Corrupt(format!(
                "replica log record_count {count} cannot fit in {} remaining bytes",
                cur.remaining()
            )));
        }
        let mut records = Vec::with_capacity(count);
        for _ in 0..count {
            let lid = LogicalId::from_bytes(cur.take(8)?.try_into().expect("8-byte slice"));
            let node = NodeId::from_bytes(cur.take(8)?.try_into().expect("8-byte slice"));
            let rid = ReplicaId::from_bytes(cur.take(8)?.try_into().expect("8-byte slice"));
            records.push(ReplicaRecord { lid, node, rid });
        }
        let stored_ck = cur.take(8)?;
        if !cur.is_empty() {
            return Err(FormatError::Corrupt("replica log trailing bytes".into()));
        }
        if checksum8(&bytes[..bytes.len() - 8]) != stored_ck {
            return Err(FormatError::Corrupt("replica log checksum mismatch".into()));
        }
        Ok(ReplicaLog {
            format_version,
            generation,
            records,
        })
    }

    pub fn publish(path: &Path, log: &Self) -> Result<(), FormatError> {
        publish_atomic(path, &log.encode())
    }
}

/// Parse the generation out of a `STEM-{gen:06}.log` name.
fn log_generation(name: &str, stem: &str) -> Option<u64> {
    name.strip_prefix(stem)
        .and_then(|s| s.strip_suffix(".log"))
        .and_then(|g| g.parse::<u64>().ok())
}

/// Delta logs at or below CURRENT's generation, oldest first. Each file
/// must decode — a damaged authoritative log fails closed (identity state
/// is unrecoverable without it). Gaps are normal: a generation with no
/// identity work publishes no log.
pub fn load_identity_logs(
    dir: &Path,
    current_generation: u64,
) -> Result<Vec<IdentityLog>, FormatError> {
    let mut gens = Vec::new();
    for entry in std::fs::read_dir(dir)
        .map_err(|e| FormatError::Io(format!("read identity logs in {}: {e}", dir.display())))?
        .flatten()
    {
        let name = entry.file_name();
        let Some(gen) = log_generation(&name.to_string_lossy(), "IDENTITY-") else {
            continue;
        };
        if gen <= current_generation {
            gens.push(gen);
        }
    }
    gens.sort_unstable();
    gens.into_iter()
        .map(|gen| {
            let bytes = std::fs::read(identity_log_path(dir, gen))
                .map_err(|e| FormatError::Io(format!("read IDENTITY-{gen:06}.log: {e}")))?;
            let log = IdentityLog::decode(&bytes)?;
            if log.generation != gen {
                return Err(FormatError::Corrupt(format!(
                    "IDENTITY-{gen:06}.log carries generation {}",
                    log.generation
                )));
            }
            Ok(log)
        })
        .collect()
}

pub fn load_replica_logs(
    dir: &Path,
    current_generation: u64,
) -> Result<Vec<ReplicaLog>, FormatError> {
    let mut gens = Vec::new();
    for entry in std::fs::read_dir(dir)
        .map_err(|e| FormatError::Io(format!("read replica logs in {}: {e}", dir.display())))?
        .flatten()
    {
        let name = entry.file_name();
        let Some(gen) = log_generation(&name.to_string_lossy(), "REPLICA-") else {
            continue;
        };
        if gen <= current_generation {
            gens.push(gen);
        }
    }
    gens.sort_unstable();
    gens.into_iter()
        .map(|gen| {
            let bytes = std::fs::read(replica_log_path(dir, gen))
                .map_err(|e| FormatError::Io(format!("read REPLICA-{gen:06}.log: {e}")))?;
            let log = ReplicaLog::decode(&bytes)?;
            if log.generation != gen {
                return Err(FormatError::Corrupt(format!(
                    "REPLICA-{gen:06}.log carries generation {}",
                    log.generation
                )));
            }
            Ok(log)
        })
        .collect()
}

/// Logs past CURRENT's generation — a crash between log publish and
/// CURRENT (the §24 state-C window). Reported and ignored at open: the
/// WAL still holds the frames, so replay rebuilds the same records.
pub fn orphan_identity_logs(dir: &Path, current_generation: u64) -> Vec<u64> {
    orphan_logs(dir, current_generation, "IDENTITY-")
}

pub fn orphan_replica_logs(dir: &Path, current_generation: u64) -> Vec<u64> {
    orphan_logs(dir, current_generation, "REPLICA-")
}

fn orphan_logs(dir: &Path, current_generation: u64, stem: &str) -> Vec<u64> {
    let mut orphans = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return orphans;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(gen) = log_generation(&name.to_string_lossy(), stem) else {
            continue;
        };
        if gen > current_generation {
            orphans.push(gen);
        }
    }
    orphans.sort_unstable();
    orphans
}
