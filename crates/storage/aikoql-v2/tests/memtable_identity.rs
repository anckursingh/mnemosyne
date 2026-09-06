//! SE2-M33 — memtable identity integration (spec §35 milestone 5, §14/§15/
//! §17): MT-001..005 — the object write/update/read surface. `put_object`
//! on an unknown ObjectId IS the create (Create + Put in ONE frame — the
//! §14 write path, atomic by construction); updates and deletes resolve
//! the SAME ids — update NEVER allocates (§15 invariant, by construction:
//! the resolve path has no allocator). Memtable entries carry ReplicaId
//! (0 = byte API); reads through the identity path filter on it — an
//! object never reads another layer's rows (§11).

mod common;

use aikoql_storage_v2::db::{Config, Db, DurabilityMode};
use aikoql_storage_v2::identity::directory::{IdentityResolver, LocalIdentityDirectory};
use aikoql_storage_v2::identity::topology::{LocalReplicaDirectory, ReplicaDirectory};
use aikoql_storage_v2::identity::{LogicalId, ObjectId, ReplicaId};
use aikoql_storage_v2::wal::{encode_frame, replay_frames, Op, WalFrame};
use common::dir;
use std::time::Duration;

fn oid(byte: u8) -> ObjectId {
    ObjectId([byte; 16])
}

#[test]
fn mt001_new_object_write_allocates_stable_identity() {
    let db = Db::open(Config::new(dir("mt001"))).unwrap();
    let a = oid(0xA1);
    let b = oid(0xB2);

    // §14 — a put to an unknown ObjectId allocates the identity triple.
    db.put_object(a, b"k", b"v1").unwrap();
    let lid_a = LocalIdentityDirectory::new(&db)
        .resolve(a)
        .unwrap()
        .unwrap();
    let rid_a = LocalReplicaDirectory::new(&db)
        .resolve_local(lid_a)
        .unwrap()
        .unwrap();

    // Two objects never share an identity — the allocation is real.
    db.put_object(b, b"k", b"v1").unwrap();
    let lid_b = LocalIdentityDirectory::new(&db)
        .resolve(b)
        .unwrap()
        .unwrap();
    let rid_b = LocalReplicaDirectory::new(&db)
        .resolve_local(lid_b)
        .unwrap()
        .unwrap();
    assert_ne!(lid_a, lid_b);
    assert_ne!(rid_a, rid_b);

    // Stable: a second put on the same oid resolves the SAME identity.
    db.put_object(a, b"k", b"v2").unwrap();
    assert_eq!(
        LocalIdentityDirectory::new(&db).resolve(a).unwrap(),
        Some(lid_a)
    );
    assert_eq!(
        LocalReplicaDirectory::new(&db)
            .resolve_local(lid_a)
            .unwrap(),
        Some(rid_a)
    );
}

#[test]
fn mt002_update_preserves_object_id() {
    let db = Db::open(Config::new(dir("mt002"))).unwrap();
    let a = oid(0xA1);
    db.put_object(a, b"k", b"v1").unwrap();
    let lid_before = LocalIdentityDirectory::new(&db)
        .resolve(a)
        .unwrap()
        .unwrap();

    // The §15 update path: same ObjectId in, same ObjectId resolved —
    // no new identity is allocated for an update.
    db.put_object(a, b"k", b"v2").unwrap();
    db.put_object(a, b"k", b"v3").unwrap();
    assert_eq!(
        LocalIdentityDirectory::new(&db).resolve(a).unwrap(),
        Some(lid_before),
        "update must not allocate a new LogicalId"
    );
    assert_eq!(
        LocalIdentityDirectory::new(&db).resolve(a).unwrap(),
        Some(lid_before),
        "the ObjectId keeps resolving to the same LogicalId"
    );
}

#[test]
fn mt003_update_preserves_logical_id() {
    let db = Db::open(Config::new(dir("mt003"))).unwrap();
    let a = oid(0xA1);
    db.put_object(a, b"k", b"v1").unwrap();
    let lid = LocalIdentityDirectory::new(&db)
        .resolve(a)
        .unwrap()
        .unwrap();

    db.put_object(a, b"k", b"v2").unwrap();
    assert_eq!(
        LocalIdentityDirectory::new(&db).resolve(a).unwrap(),
        Some(lid)
    );

    // §16 — deletion must not destroy identity metadata: the mapping
    // survives the tombstone.
    db.delete_object(a, b"k").unwrap();
    assert_eq!(
        LocalIdentityDirectory::new(&db).resolve(a).unwrap(),
        Some(lid),
        "delete keeps the identity directory entry"
    );
}

#[test]
fn mt004_update_preserves_replica_id() {
    let db = Db::open(Config::new(dir("mt004"))).unwrap();
    let a = oid(0xA1);
    db.put_object(a, b"k", b"v1").unwrap();
    let lid = LocalIdentityDirectory::new(&db)
        .resolve(a)
        .unwrap()
        .unwrap();
    let rid = LocalReplicaDirectory::new(&db)
        .resolve_local(lid)
        .unwrap()
        .unwrap();

    db.put_object(a, b"k", b"v2").unwrap();
    db.delete_object(a, b"k").unwrap();
    db.put_object(a, b"k", b"v3").unwrap();
    assert_eq!(
        LocalReplicaDirectory::new(&db).resolve_local(lid).unwrap(),
        Some(rid),
        "update/delete/re-write must never allocate a new ReplicaId"
    );
}

#[test]
fn mt005_memtable_read_returns_correct_value_through_identity_path() {
    let db = Db::open(Config::new(dir("mt005"))).unwrap();
    let a = oid(0xA1);

    // The §13 read path: resolve → read the object's own entry.
    assert_eq!(db.get_object(a, b"k").unwrap(), None, "never written");
    db.put_object(a, b"k", b"v1").unwrap();
    assert_eq!(db.get_object(a, b"k").unwrap(), Some(b"v1".to_vec()));
    db.put_object(a, b"k", b"v2").unwrap();
    assert_eq!(db.get_object(a, b"k").unwrap(), Some(b"v2".to_vec()));
    db.delete_object(a, b"k").unwrap();
    assert_eq!(db.get_object(a, b"k").unwrap(), None, "tombstoned");

    // §11 — the identity filter: a byte-API row at the same key is
    // another layer's data and must never answer an object read, even
    // when it is newer.
    db.put_object(a, b"k", b"object-value").unwrap();
    db.put(b"k", b"byte-value").unwrap(); // newer seq, rid 0
    assert_eq!(
        db.get(b"k").unwrap(),
        Some(b"byte-value".to_vec()),
        "the plain byte API sees the raw key-space head"
    );
    assert_eq!(
        db.get_object(a, b"k").unwrap(),
        Some(b"object-value".to_vec()),
        "the object reads its OWN newest entry, never the byte row"
    );
}

#[test]
fn mt006_replay_restores_memtable_identity() {
    let d = dir("mt006");
    let a = oid(0xA1);
    let b = oid(0xB2);
    {
        let db = Db::open(Config::new(d.clone())).unwrap();
        db.put_object(a, b"ka", b"va").unwrap();
        db.put_object(b, b"kb", b"vb").unwrap();
        db.put_object(a, b"ka", b"va2").unwrap();
    }
    // No flush — the WAL alone rebuilds the memtable, entries carrying
    // their rid, so the identity read path works after reopen.
    let db = Db::open(Config::new(d)).unwrap();
    assert_eq!(db.get_object(a, b"ka").unwrap(), Some(b"va2".to_vec()));
    assert_eq!(db.get_object(b, b"kb").unwrap(), Some(b"vb".to_vec()));
    assert!(LocalIdentityDirectory::new(&db)
        .resolve(a)
        .unwrap()
        .is_some());
}

#[test]
fn mt007_get_object_reads_flushed_segment() {
    // SE2-M34 lifted the M33 fail-closed boundary: v3 blocks carry the rid
    // on disk, so get_object answers through the segment read path after
    // the flush — and the §11 filter holds on disk too: a newer byte-API
    // row at the same key never answers the object, and the object's own
    // tombstone does.
    let db = Db::open(Config::new(dir("mt007"))).unwrap();
    let a = oid(0xA1);
    db.put_object(a, b"k", b"v1").unwrap();
    db.flush().unwrap();
    assert_eq!(db.get_object(a, b"k").unwrap(), Some(b"v1".to_vec()));
    db.put(b"k", b"byte-value").unwrap(); // newer seq, rid 0, in the memtable
    assert_eq!(
        db.get_object(a, b"k").unwrap(),
        Some(b"v1".to_vec()),
        "the byte row never answers the object read"
    );
    db.delete_object(a, b"k").unwrap();
    db.flush().unwrap(); // the tombstone lands in its own v3 segment
    assert_eq!(db.get_object(a, b"k").unwrap(), None, "tombstoned on disk");
}

#[test]
fn mt008_group_commit_applies_object_ops() {
    let d = dir("mt008");
    let mut cfg = Config::new(d.clone());
    cfg.durability = DurabilityMode::GroupCommit;
    cfg.max_wait_duration = Duration::from_millis(200);
    let db = Db::open(cfg).unwrap();
    let a = oid(0xA1);
    // The committer thread applies the object ops — acked == visible.
    db.put_object(a, b"k", b"v1").unwrap();
    assert_eq!(db.get_object(a, b"k").unwrap(), Some(b"v1".to_vec()));
    drop(db);
    let db = Db::open(Config::new(d)).unwrap();
    assert_eq!(db.get_object(a, b"k").unwrap(), Some(b"v1".to_vec()));
}

#[test]
fn mt009_object_wal_ops_roundtrip() {
    let ops = vec![
        Op::CreateObject {
            oid: oid(0xA1),
            lid: LogicalId(5),
            rid: ReplicaId(7),
            pgen: 3,
        },
        Op::PutObject(ReplicaId(7), b"k1".to_vec(), b"v1".to_vec()),
        Op::DeleteObject(ReplicaId(7), b"k2".to_vec()),
    ];
    let frame = encode_frame(9, &ops).unwrap();
    let (frames, consumed) = replay_frames(&frame).unwrap();
    assert_eq!(consumed, frame.len());
    assert_eq!(frames, vec![WalFrame { seq: 9, ops }]);

    // Byte-API frames (ops 1/2) are untouched — the new op bytes are
    // additive, the existing shapes byte-identical.
    let old = encode_frame(1, &[Op::Put(b"k".to_vec(), b"v".to_vec())]).unwrap();
    let (old_frames, old_consumed) = replay_frames(&old).unwrap();
    assert_eq!(old_consumed, old.len());
    assert_eq!(
        old_frames,
        vec![WalFrame {
            seq: 1,
            ops: vec![Op::Put(b"k".to_vec(), b"v".to_vec())]
        }]
    );
}
