//! Single open path for every subcommand (MRFC-0020): honors [encryption]
//! settings so no plaintext writer can open an encrypted database — that
//! would silently corrupt it. Backend selection (PR#2 review SE-01/SE-02)
//! is owned by the RuntimeConfig pipeline (defaults → TOML → env → CLI);
//! the public contract and per-backend profiles live in
//! docs/STORAGE-BACKENDS.md.

use crate::config::{RuntimeEncryption, StorageBackend};
use aikoql_kernel::security::crypto::{Aes256Gcm, Crypto};
use aikoql_kernel::security::envelope::Envelope;
use aikoql_kernel::security::field_crypto::EncryptionPolicy;
use aikoql_kernel::security::hkdf::{self, DOMAIN_STORE};
use aikoql_kernel::security::kms::LocalKms;
use aikoql_kernel::security::KeyManager;
use aikoql_kernel::storage::encrypted::EncryptedStore;
use aikoql_kernel::storage::store::StorageEngine;
use aikoql_kernel::storage::store_redb::RedbEngine;
use aikoql_kernel::{KError, KResult, Kernel, SystemClock};
use aikoql_storage::AikoqlStorageEngine;
use aikoql_storage_v2::AikoqlStorageEngineV2;
use std::io::Read;
use std::sync::Arc;

/// Backend resolution (PR#2 review SE-01/SE-02, docs/STORAGE-BACKENDS.md):
/// the config pipeline owns the selection — no direct env reads here. An
/// explicit backend opens exactly that engine (unknown values already
/// failed closed at the config layer). `None` (the default) auto-detects
/// the existing format at `db_path`: a v2 database directory (CURRENT
/// present) opens as aikoql-v2, a file with the v1 WAL magic ("AKQL")
/// opens as aikoql, anything else — a missing path included — is redb, the
/// stable default (redb validates its own format and fails closed on
/// anything else). Detection makes upgrades safe in both directions: a
/// redb database from before the backend switch and a native WAL written
/// while aikoql was the production default both keep working at the same
/// path. A directory that is not a v2 database is an explicit error —
/// never a silent fresh create.
fn open_engine(db_path: &str, backend: Option<StorageBackend>) -> KResult<Arc<dyn StorageEngine>> {
    let backend = match backend {
        Some(b) => b,
        None => detect_backend(db_path)?,
    };
    match backend {
        StorageBackend::Redb => Ok(Arc::new(RedbEngine::open(db_path)?)),
        StorageBackend::Aikoql => Ok(Arc::new(AikoqlStorageEngine::open(db_path)?)),
        StorageBackend::AikoqlV2 => Ok(Arc::new(AikoqlStorageEngineV2::open(db_path)?)),
    }
}

/// Sniff the on-disk format. A <4-byte file falls through to redb, whose
/// own header validation fails closed — the native WAL parser never
/// truncates or reinterprets a non-AKQL file.
fn detect_backend(db_path: &str) -> KResult<StorageBackend> {
    let p = std::path::Path::new(db_path);
    if p.is_dir() {
        if p.join("CURRENT").is_file() {
            return Ok(StorageBackend::AikoqlV2);
        }
        return Err(KError::Store(format!(
            "{db_path} is a directory but not an aikoql-v2 database (no CURRENT): \
             name an explicit backend (--backend / AIKOQL_BACKEND / storage.backend)"
        )));
    }
    match std::fs::File::open(p) {
        Ok(mut f) => {
            let mut magic = [0u8; 4];
            if f.read(&mut magic).ok() == Some(4) && &magic == b"AKQL" {
                return Ok(StorageBackend::Aikoql);
            }
            Ok(StorageBackend::Redb)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StorageBackend::Redb),
        Err(e) => Err(KError::Store(format!("read {db_path}: {e}"))),
    }
}

pub(crate) fn open_kernel(
    db_path: &str,
    enc: &RuntimeEncryption,
    backend: Option<StorageBackend>,
) -> KResult<Kernel> {
    let engine = open_engine(db_path, backend)?;
    if !enc.enabled {
        return Kernel::open(engine, Arc::new(SystemClock), 0xA9C9);
    }
    let Some(pass) = enc.passphrase.as_deref() else {
        return Err(KError::Store(
            "encryption enabled but no passphrase: set AIKOQL_PASSPHRASE or encryption.passphrase"
                .into(),
        ));
    };
    let kms = LocalKms::new(&enc.key_path);
    let kek = kms.master_key(pass).map_err(KError::Store)?;
    // The store key is a domain-separated subkey of the KEK — the KEK itself
    // never encrypts data directly (DEK wrapping uses its own subkey).
    let store_key = hkdf::domain_sep(&kek, DOMAIN_STORE);
    let crypto = Arc::new(Crypto::new(Box::new(Aes256Gcm::new())));
    let envelope = Arc::new(Envelope::init(&kms, pass, crypto.clone()).map_err(KError::Store)?);
    let store: Arc<dyn StorageEngine> =
        Arc::new(EncryptedStore::new(engine, crypto.clone(), store_key));
    let kernel = Kernel::open(store, Arc::new(SystemClock), 0xA9C9)?
        .with_field_encryption(crypto, envelope)?;
    for (type_name, fields) in &enc.policies {
        kernel.set_encryption_policy(type_name, EncryptionPolicy::new(fields.clone()));
    }
    Ok(kernel)
}

/// Subcommand variant: one config pipeline (R10, PR#2 review SE-02) —
/// encryption AND backend both come from `load()` (defaults → TOML → env;
/// subcommand flags are not server config and are not parsed).
pub(crate) fn open_kernel_auto(db_path: &str) -> KResult<Kernel> {
    let cfg = crate::config::load(&[], None, None).map_err(KError::Store)?;
    open_kernel(db_path, &cfg.encryption, cfg.backend)
}

#[cfg(test)]
mod tests {
    use super::open_engine;
    use crate::config::StorageBackend;
    use aikoql_kernel::storage::store::{StorageEngine, WriteBatch};
    use aikoql_kernel::storage::store_redb::RedbEngine;
    use aikoql_storage::AikoqlStorageEngine;
    use aikoql_storage_v2::AikoqlStorageEngineV2;
    use std::io::Read;

    // Temp db paths written by THIS test thread, swept when the thread exits
    // (the main thread's destructor runs at process exit — statics are NOT
    // dropped on Windows MSVC, TLS is).
    thread_local! {
        static TEMP_PATHS: std::cell::RefCell<TempSweeper> =
            const { std::cell::RefCell::new(TempSweeper { paths: Vec::new() }) };
    }

    struct TempSweeper {
        paths: Vec<std::path::PathBuf>,
    }
    impl Drop for TempSweeper {
        fn drop(&mut self) {
            for p in &self.paths {
                let _ = std::fs::remove_file(p);
                let _ = std::fs::remove_dir_all(p);
                // Sidecars next to the registered stem (`{stem}.redb.artifacts`).
                let Some(name) = p.file_name() else { continue };
                if let Ok(rd) = std::fs::read_dir(p.parent().unwrap_or(std::path::Path::new("."))) {
                    let prefix = format!("{}.", name.to_string_lossy());
                    for e in rd.flatten() {
                        if e.file_name().to_string_lossy().starts_with(&prefix) {
                            let _ = std::fs::remove_file(e.path());
                            let _ = std::fs::remove_dir_all(e.path());
                        }
                    }
                }
            }
        }
    }

    fn scratch(tag: &str) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!("aikoql_mcp_backend_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_file(&p);
        TEMP_PATHS.with(|t| t.borrow_mut().paths.push(p.clone()));
        p.to_string_lossy().into_owned()
    }

    /// SE-02 — every explicit backend opens and serves a put/get through the
    /// same selector (no env reads: the selector takes the config value).
    #[test]
    fn backend_matrix_explicit_selection() {
        for (backend, tag) in [
            (Some(StorageBackend::Redb), "redb"),
            (Some(StorageBackend::Aikoql), "aikoql"),
            (Some(StorageBackend::AikoqlV2), "aikoql-v2"),
        ] {
            let engine = open_engine(&scratch(tag), backend).unwrap();
            let mut b = WriteBatch::new();
            b.put(b"k".to_vec(), b"v".to_vec());
            engine.write_batch(&b).unwrap();
            assert_eq!(engine.get(b"k").unwrap(), Some(b"v".to_vec()));
        }
    }

    /// PR#2 review SE-01 regression: a redb database created under the old
    /// behavior opens as redb through the new default path — it is never
    /// reinterpreted (or truncated) by the native WAL parser, and its data
    /// survives byte-exact.
    #[test]
    fn existing_redb_database_is_not_reinterpreted_as_native_wal() {
        let path = scratch("redb-existing");
        {
            let e = RedbEngine::open(&path).unwrap();
            let mut b = WriteBatch::new();
            b.put(b"k".to_vec(), b"v".to_vec());
            e.write_batch(&b).unwrap();
        }
        {
            let engine = open_engine(&path, None).unwrap();
            assert_eq!(engine.get(b"k").unwrap(), Some(b"v".to_vec()));
        } // redb holds a live file lock — read the head bytes after close
        let mut head = [0u8; 4];
        std::fs::File::open(&path)
            .unwrap()
            .read_exact(&mut head)
            .unwrap();
        assert_ne!(
            &head, b"AKQL",
            "the redb file must not be rewritten as a native WAL"
        );
    }

    /// SE-01 both directions: a native WAL written while aikoql was the
    /// production default keeps opening as aikoql through the auto path.
    #[test]
    fn existing_native_wal_auto_detects_v1() {
        let path = scratch("v1-existing");
        {
            let e = AikoqlStorageEngine::open(&path).unwrap();
            let mut b = WriteBatch::new();
            b.put(b"k".to_vec(), b"v".to_vec());
            e.write_batch(&b).unwrap();
        }
        let engine = open_engine(&path, None).unwrap();
        assert_eq!(engine.get(b"k").unwrap(), Some(b"v".to_vec()));
    }

    /// SE-01 — a v2 database directory auto-detects; a directory that is
    /// NOT a v2 database fails closed instead of becoming a fresh store.
    #[test]
    fn v2_directory_auto_detects_and_non_v2_dir_fails_closed() {
        let dir = scratch("v2-existing");
        {
            let e = AikoqlStorageEngineV2::open(&dir).unwrap();
            let mut b = WriteBatch::new();
            b.put(b"k".to_vec(), b"v".to_vec());
            e.write_batch(&b).unwrap();
        }
        let engine = open_engine(&dir, None).unwrap();
        assert_eq!(engine.get(b"k").unwrap(), Some(b"v".to_vec()));

        let plain = scratch("plain-dir");
        std::fs::create_dir_all(&plain).unwrap();
        let err = match open_engine(&plain, None) {
            Err(e) => e,
            Ok(_) => panic!("a non-v2 directory must fail closed, not become a fresh store"),
        };
        assert!(
            format!("{err}").contains("not an aikoql-v2 database"),
            "got: {err}"
        );
    }
}
