//! MVP-QA-002 Suite D — fault injection (QA2-FAULT-008, -009).
//!
//! FAULT-001..007 are already covered (d04/d04b crash at every commit
//! boundary, i10 rebuild, REC-002 atomic schema rows, CON-005/CON-007).
//! These two close the backup/restore gap:
//! - FAULT-008: a failed backup never affects live knowledge.
//! - FAULT-009: an interrupted restore (garbage / truncated snapshot /
//!   non-file source) is never exposed as a valid restored database —
//!   the engine contract (single atomic batch, validate-then-write) pins it.

use aikoql_kernel::*;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

// Temp paths created by THIS thread, swept when the thread exits (the main
// thread's destructor runs at process exit — statics are NOT dropped on
// Windows MSVC, TLS is). Kill-harness children never register a path they
// received via env, so the parent's evidence survives its child.
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
        }
    }
}

fn mk(engine: Arc<MemoryEngine>) -> Kernel {
    Kernel::open(engine, Arc::new(ManualClock::new(10_000)), 0xC0FFEE).unwrap()
}

fn meta(t: &str) -> Metadata {
    Metadata {
        type_name: t.into(),
        tenant: None,
        schema_version: 1,
        tags: vec![],
    }
}

fn alice() -> Subject {
    Subject::new("alice")
}

fn create(k: &Kernel, name: &str, n: i64) -> KOID {
    let mut req = RememberRequest::create(alice(), meta("fact"));
    req.properties
        .insert("name".into(), Value::Text(name.into()));
    req.properties.insert("n".into(), Value::Int(n));
    k.remember(req).unwrap().koid
}

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aikoql_qa2_{tag}_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    TEMP_PATHS.with(|t| t.borrow_mut().paths.push(p.clone()));
    p
}

// ---------------------------------------------------------------------------
// QA2-FAULT-008 — backup failure leaves live knowledge unaffected
// ---------------------------------------------------------------------------

#[test]
fn w2_fault_008_backup_failure_leaves_live_knowledge_unaffected() {
    let engine = Arc::new(MemoryEngine::new());
    let k = mk(engine.clone());
    let a = create(&k, "a", 1);
    let b = create(&k, "b", 2);
    let _c = create(&k, "c", 3);

    // Failure 1: destination is a directory — the backup file cannot be
    // created there.
    let dest_dir = tmp_path("fault008_dir");
    std::fs::create_dir_all(&dest_dir).unwrap();
    assert!(
        k.backup_store_to(&dest_dir).is_err(),
        "backup onto a directory path must fail"
    );

    // Failure 2: destination parent directory does not exist.
    let missing_root = tmp_path("fault008_missing");
    let missing = missing_root.join("no").join("snap.redb");
    assert!(
        k.backup_store_to(&missing).is_err(),
        "backup into a missing directory must fail"
    );

    // Live knowledge unaffected: every read still answers, the journal is
    // intact, and NEW writes still commit on top.
    assert_eq!(
        k.get(alice(), &a).unwrap().properties.get("n"),
        Some(&Value::Int(1))
    );
    assert_eq!(
        k.get(alice(), &b).unwrap().properties.get("n"),
        Some(&Value::Int(2))
    );
    assert_eq!(k.journal().unwrap().len(), 3);
    let d = create(&k, "d", 4);
    assert_eq!(k.journal().unwrap().len(), 4);

    // The same live store backs up fine to a valid path afterwards — the
    // failures consumed nothing.
    let good = tmp_path("fault008_good").join("snap.redb");
    std::fs::create_dir_all(good.parent().unwrap()).unwrap();
    k.backup_store_to(&good).unwrap();
    assert!(good.is_file());
    assert_eq!(
        k.get(alice(), &d).unwrap().properties.get("n"),
        Some(&Value::Int(4))
    );

    let _ = std::fs::remove_dir_all(&dest_dir);
    let _ = std::fs::remove_dir_all(good.parent().unwrap());
    let _ = std::fs::remove_dir_all(&missing_root);
}

// ---------------------------------------------------------------------------
// QA2-FAULT-009 — interrupted restore is never exposed as a valid database
// ---------------------------------------------------------------------------

#[test]
fn w2_fault_009_interrupted_restore_never_exposed_as_valid() {
    let engine = Arc::new(MemoryEngine::new());
    let k = mk(engine.clone());
    let a = create(&k, "a", 1);
    let b = create(&k, "b", 2);
    let c = create(&k, "c", 3);

    // A valid snapshot of the 3-KO state (the restore target for later).
    let good = tmp_path("fault009_good").join("snap.redb");
    std::fs::create_dir_all(good.parent().unwrap()).unwrap();
    k.backup_store_to(&good).unwrap();

    // Interruption 1: garbage source — not a database at all.
    let garbage = tmp_path("fault009_garbage").join("garbage.redb");
    std::fs::create_dir_all(garbage.parent().unwrap()).unwrap();
    std::fs::write(&garbage, b"definitely not a redb database").unwrap();
    assert!(
        k.restore_store_from(&garbage).is_err(),
        "restore from garbage must fail"
    );

    // Interruption 2: a TRUNCATED snapshot — the backup write was cut off
    // mid-flight. This is the interrupted-backup analog and must be
    // rejected before any live row is touched.
    let truncated = tmp_path("fault009_truncated").join("trunc.redb");
    std::fs::create_dir_all(truncated.parent().unwrap()).unwrap();
    let bytes = std::fs::read(&good).unwrap();
    std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
    assert!(
        k.restore_store_from(&truncated).is_err(),
        "restore from a truncated snapshot must fail"
    );

    // Interruption 3: source is not a file (directory) — rejected by the
    // validate-first contract.
    let dir_src = tmp_path("fault009_dirsrc");
    std::fs::create_dir_all(&dir_src).unwrap();
    assert!(
        k.restore_store_from(&dir_src).is_err(),
        "restore from a directory must fail"
    );

    // After every failure the live database still answers with its
    // pre-restore state — an incomplete restore is never exposed as valid.
    for (id, n) in [(a, 1), (b, 2), (c, 3)] {
        assert_eq!(
            k.get(alice(), &id).unwrap().properties.get("n"),
            Some(&Value::Int(n)),
            "live state must survive every failed restore"
        );
    }
    assert_eq!(k.journal().unwrap().len(), 3);

    // A KO written after the snapshot was taken (post-backup knowledge) —
    // the later valid restore must roll the store back to the snapshot.
    let d = create(&k, "d", 4);

    // The real restore succeeds and replaces the store atomically; after
    // the documented restart, the snapshot state (without `d`) is current.
    k.restore_store_from(&good).unwrap();
    drop(k);
    let k2 = mk(engine);
    assert_eq!(
        k2.get(alice(), &a).unwrap().properties.get("n"),
        Some(&Value::Int(1))
    );
    assert_eq!(
        k2.get(alice(), &b).unwrap().properties.get("n"),
        Some(&Value::Int(2))
    );
    assert_eq!(
        k2.get(alice(), &c).unwrap().properties.get("n"),
        Some(&Value::Int(3))
    );
    assert!(
        matches!(k2.get(alice(), &d), Err(KError::NotFound(_))),
        "post-snapshot knowledge must not survive a point-in-time restore"
    );

    let _ = std::fs::remove_dir_all(garbage.parent().unwrap());
    let _ = std::fs::remove_dir_all(truncated.parent().unwrap());
    let _ = std::fs::remove_dir_all(&dir_src);
    let _ = std::fs::remove_dir_all(good.parent().unwrap());
}
