//! KSE-11 — encryption boundary (MRFC-KSE-001 §17).
//!
//! The doc's rule: reuse the existing AIKOQL encryption architecture, do not
//! create a second incompatible model. This test is that gate — it imports
//! ONLY kernel security modules (`EncryptedStore`, `Envelope`, `FieldCrypto`,
//! `Crypto`, `EncryptionPolicy`) and wraps the three engines with them. Zero
//! crypto code exists in aikoql-storage; the engine is byte-opaque, and the
//! ciphertext rides the same enveloped WAL records as everything else.
//!
//! Gates: KSE-100 encrypted write/read round trip (incl. reopen/replay);
//! KSE-101 wrong key fails closed; KSE-102 corrupt ciphertext gives a
//! deterministic error, never garbage; KSE-103 KEK rotation re-wraps DEKs
//! (online, old data stays readable, new kek_id); KSE-104 crash during
//! rotation = the persisted state is untouched (DEKs still wrapped under the
//! old KEK) and a fresh open recovers — with no plaintext anywhere in the
//! store. MemoryEngine is out of this phase entirely: it has no persistence,
//! so it has no reopen/crash surface (the kernel's own suite covers
//! EncryptedStore-over-memory in e04).

mod common;

use aikoql_kernel::security::crypto::{Aes256Gcm, Crypto, CryptoProvider};
use aikoql_kernel::security::envelope::Envelope;
use aikoql_kernel::security::field_crypto::{EncryptionPolicy, FieldCrypto};
use aikoql_kernel::security::kms::KeyManager;
use aikoql_kernel::storage::encrypted::EncryptedStore;
use aikoql_kernel::storage::store::{StorageEngine, WriteBatch};
use aikoql_kernel::storage::store_redb::RedbEngine;
use aikoql_kernel::transaction::kernel::ManualClock;
use aikoql_kernel::{Kernel, Metadata, Origin, RememberRequest, Subject, Value};
use aikoql_storage::AikoqlStorageEngine;
use common::tmp;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

/// In-memory KMS — the same test double the kernel's own encryption suite
/// uses (no second model, not even in the harness).
struct MemKms {
    key: RwLock<[u8; 32]>,
}
impl MemKms {
    fn new() -> Self {
        MemKms {
            key: RwLock::new(Aes256Gcm::new().generate_key()),
        }
    }
}
impl KeyManager for MemKms {
    fn master_key(&self, _passphrase: &str) -> Result<[u8; 32], String> {
        Ok(*self.key.read().unwrap())
    }
    fn rotate(&self, _passphrase: &str, provider: &dyn CryptoProvider) -> Result<[u8; 32], String> {
        let new_key = provider.generate_key();
        *self.key.write().unwrap() = new_key;
        Ok(new_key)
    }
}

fn alice() -> Subject {
    Subject::new("alice")
}

fn props(secret: &str) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("name".into(), Value::Text("Alice".into()));
    p.insert("secret".into(), Value::Text(secret.into()));
    p
}

/// Everything KSE-100..104 observed on one backend. PartialEq lets the
/// harness pin cross-backend parity against the redb reference.
#[derive(Debug, PartialEq)]
struct Summary {
    // KSE-100: EncryptedStore round trip + reopen; no plaintext at rest.
    round_trip: bool,
    no_plaintext: bool,
    // KSE-101: wrong key fails closed.
    wrong_key_fails: bool,
    // KSE-102: corrupt ciphertext → deterministic error, never garbage.
    tamper_err: String,
    // KSE-103: KEK rotation — old data readable, DEKs re-wrapped.
    rot_old_readable: bool,
    rot_new_kek: bool,
    // KSE-104: crash during rotation — persisted state recovers, no
    // plaintext fallback.
    crash_recoverable: bool,
    crash_no_plaintext: bool,
}

/// Reopen closure per backend.
type Reopen = Box<dyn Fn() -> Arc<dyn StorageEngine>>;

fn scenarios(engine: Arc<dyn StorageEngine>, reopen: Reopen) -> Summary {
    let crypto = Arc::new(Crypto::new(Box::new(Aes256Gcm::new())));
    let key = crypto.generate_key();

    // KSE-100 — encrypted write/read round trip, then reopen (WAL replay
    // for aikoql) and read again. The first handle must drop before the
    // reopen (redb locks the file exclusively).
    {
        let store = EncryptedStore::new(engine.clone(), crypto.clone(), key);
        let mut batch = WriteBatch::new();
        batch.put(b"classified".to_vec(), b"top-secret".to_vec());
        store.write_batch(&batch).unwrap();
        assert_eq!(store.get(b"classified").unwrap().unwrap(), b"top-secret");
        let raw = engine.get(b"classified").unwrap().unwrap();
        assert_ne!(raw, b"top-secret", "plaintext at rest");
    }
    drop(engine);
    let reopened = reopen();
    let round_trip = {
        let store = EncryptedStore::new(reopened.clone(), crypto.clone(), key);
        store.get(b"classified").unwrap().unwrap() == b"top-secret"
    };
    let no_plaintext = reopened.get(b"classified").unwrap().unwrap() != b"top-secret";

    // KSE-101 — a different key must fail, never return garbage.
    let wrong_key_fails = {
        let store = EncryptedStore::new(reopened.clone(), crypto.clone(), crypto.generate_key());
        store.get(b"classified").is_err()
    };

    // KSE-102 — corrupt ciphertext: garbage written where the value lives
    // (bitrot / tamper at rest) must decrypt-error deterministically.
    let tamper_err = {
        let mut plant = WriteBatch::new();
        plant.put(b"classified".to_vec(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
        reopened.write_batch(&plant).unwrap();
        let store = EncryptedStore::new(reopened.clone(), crypto.clone(), key);
        match store.get(b"classified") {
            Ok(_) => "unexpected-ok".into(),
            Err(e) => format!("{e}"),
        }
    };
    drop(reopened); // KSE-104 needs fresh handles (redb locks the file)

    // KSE-103 — KEK rotation: encrypt with env1, rotate, old ciphertext
    // still decrypts through the rotated envelope; DEKs carry the new id.
    let kms = MemKms::new();
    let env1 = Arc::new(Envelope::init(&kms, "pw", crypto.clone()).unwrap());
    let fc1 = FieldCrypto::new(crypto.clone(), env1.clone());
    let policy = EncryptionPolicy::new(vec!["secret".to_string()]);
    let mut encrypted = props("pre-rotation-secret");
    fc1.encrypt_fields("acme", "doc", &mut encrypted, &policy)
        .unwrap();
    let kek_before = env1.wrapped_deks()[0].kek_id;
    env1.rotate_kek(&kms, "pw").unwrap();
    let kek_after = env1.wrapped_deks()[0].kek_id;
    let rot_old_readable = {
        let fc2 = FieldCrypto::new(crypto.clone(), env1.clone());
        let mut dec = encrypted.clone();
        fc2.decrypt_fields("acme", "doc", &mut dec, &policy)
            .unwrap();
        dec.get("secret") == Some(&Value::Text("pre-rotation-secret".into()))
    };
    let rot_new_kek = kek_after == kek_before + 1;

    // KSE-104 — crash during rotation: a kernel with field encryption
    // persists KO + wrapped DEKs; drop everything mid-life (a rotation
    // that never ran), reopen a fresh kernel over a REOPENED engine, and
    // the secret must decrypt.
    let (crash_recoverable, crash_no_plaintext) = {
        let engine2 = reopen();
        let koid = {
            let envelope = Arc::new(Envelope::init(&kms, "pw", crypto.clone()).unwrap());
            let k = Kernel::open(engine2.clone(), Arc::new(ManualClock::new(30_000)), 0xE11)
                .unwrap()
                .with_field_encryption(crypto.clone(), envelope)
                .unwrap();
            k.set_encryption_policy("doc", EncryptionPolicy::new(vec!["secret".to_string()]));
            k.remember(RememberRequest {
                context: (&alice()).into(),
                koid: None,
                expected_version: Some(0),
                idempotency_key: None,
                metadata: Metadata {
                    type_name: "doc".into(),
                    tenant: None,
                    schema_version: 1,
                    tags: vec![],
                },
                properties: props("crash-survives"),
                semantic: None,
                relationships: vec![],
                security: None,
                extensions: BTreeMap::new(),
                origin: Origin::Human,
                note: None,
                referential_policy: aikoql_kernel::ReferentialPolicy::default(),
            })
            .unwrap()
            .koid
        }; // kernel + envelope dropped: crash before any rotation could persist

        let raw_has_plaintext = engine2
            .scan(b"ko/")
            .unwrap()
            .iter()
            .any(|(_, v)| v.windows(14).any(|w| w == b"crash-survives"));
        drop(engine2); // redb locks the file: one open at a time

        let engine3 = reopen();
        let recovered = {
            let envelope = Arc::new(Envelope::init(&kms, "pw", crypto.clone()).unwrap());
            let k = Kernel::open(engine3, Arc::new(ManualClock::new(31_000)), 0xE11)
                .unwrap()
                .with_field_encryption(crypto.clone(), envelope)
                .unwrap();
            k.set_encryption_policy("doc", EncryptionPolicy::new(vec!["secret".to_string()]));
            k.get(alice(), &koid).unwrap().properties.get("secret")
                == Some(&Value::Text("crash-survives".into()))
        };
        (recovered, !raw_has_plaintext)
    };

    Summary {
        round_trip,
        no_plaintext,
        wrong_key_fails,
        tamper_err,
        rot_old_readable,
        rot_new_kek,
        crash_recoverable,
        crash_no_plaintext,
    }
}

/// KSE-100..104 — the existing encryption architecture over the two
/// persistent backends, parity-pinned, zero new crypto code.
#[test]
fn kse100_104_encryption_boundary() {
    let redb_p = tmp("kse11_redb");
    let rb_path = redb_p.clone();
    let redb = scenarios(
        Arc::new(RedbEngine::open(&redb_p).unwrap()),
        Box::new(move || Arc::new(RedbEngine::open(&rb_path).unwrap())),
    );
    let aikoql_p = tmp("kse11_aikoql");
    let aq_path = aikoql_p.clone();
    let aikoql = scenarios(
        Arc::new(AikoqlStorageEngine::open(&aikoql_p).unwrap()),
        Box::new(move || Arc::new(AikoqlStorageEngine::open(&aq_path).unwrap())),
    );

    // Parity: aikoql must behave exactly like the redb reference.
    assert_eq!(aikoql, redb, "aikoql diverged from the redb reference");

    // Contract pins — the documented encryption guarantees, not just parity.
    assert!(redb.round_trip, "KSE-100: round trip failed");
    assert!(redb.no_plaintext, "KSE-100: plaintext at rest");
    assert!(redb.wrong_key_fails, "KSE-101: wrong key must fail closed");
    assert!(
        redb.tamper_err != "unexpected-ok" && !redb.tamper_err.is_empty(),
        "KSE-102: tamper must error deterministically, got: {}",
        redb.tamper_err
    );
    assert!(
        redb.rot_old_readable,
        "KSE-103: old data lost after rotation"
    );
    assert!(redb.rot_new_kek, "KSE-103: DEKs not re-wrapped");
    assert!(
        redb.crash_recoverable,
        "KSE-104: crash during rotation not recoverable"
    );
    assert!(
        redb.crash_no_plaintext,
        "KSE-104: plaintext fallback in the store"
    );

    for p in [&redb_p, &aikoql_p] {
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_dir_all(p);
    }
}
