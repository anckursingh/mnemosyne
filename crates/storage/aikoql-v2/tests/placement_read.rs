//! SE2-M39 §13 — the placement-direct read path. A get_object for a
//! Segment-placed replica must decode O(RESTART_INTERVAL) entries from its
//! stored position — never the key's whole equal-key run. The placement
//! directory IS the per-replica index (flush/compact maintain it); the v4
//! dense cadence table makes a stored entry index decode standalone.
//!
//! RED until the v4 writer + `SegmentReader::get_entry_at` land: the
//! rid-filtered scan decodes ~n entries for the tail replica.

use aikoql_storage_v2::db::{Config, Db};
use aikoql_storage_v2::identity::directory::{IdentityResolver, LocalIdentityDirectory};
use aikoql_storage_v2::identity::topology::{LocalReplicaDirectory, ReplicaDirectory};
use aikoql_storage_v2::identity::ObjectId;
use aikoql_storage_v2::placement::directory::{
    LocalPlacementResolver, Placement, PlacementResolver,
};

mod common;

fn oid_of(i: u64) -> ObjectId {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&i.to_le_bytes());
    ObjectId(b)
}

#[test]
fn pd001_tail_replica_decodes_bounded() {
    let d = common::dir("pd001");
    let db = Db::open(Config::new(d.to_path_buf())).unwrap();
    let n = 20_000u64;
    for i in 0..n {
        db.put_object(oid_of(i), b"k0", format!("v{i}").as_bytes())
            .unwrap();
    }
    db.flush().unwrap();
    // The first-created object's row is at the tail of k0's seq-descending
    // run: the rid scan decodes ~n entries, the placement-direct path ≤ the
    // cadence (16). A few more samples exercise dense entries at different
    // cadence offsets.
    for &i in &[0u64, n / 4, n / 2, 3 * n / 4, n - 1] {
        let before = db.read_path_stats().entries_decoded;
        let got = db.get_object(oid_of(i), b"k0").unwrap();
        let after = db.read_path_stats().entries_decoded;
        assert_eq!(
            got.as_deref(),
            Some(format!("v{i}").as_bytes()),
            "value at tail replica {i}"
        );
        assert!(
            after - before <= 16,
            "get_object decoded {} entries for replica {i} — the \
             placement-direct path must bound the decode (O(run) scan)",
            after - before
        );
    }
}

#[test]
fn pd002_put_after_flush_flips_placement_to_memtable() {
    // A write moves the replica's newest row into the active memtable —
    // the placement must flip BEFORE the ack, or the §13 direct read
    // trusts a stale Segment anchor and hides the newest row (the oracle
    // caught exactly this: put → compact → restart diverged). The flip
    // must survive a restart: the record rides the WAL replay.
    let d = common::dir("pd002");
    let db = Db::open(Config::new(d.to_path_buf())).unwrap();
    let oid = oid_of(1);
    db.put_object(oid, b"k0", b"v1").unwrap();
    db.flush().unwrap();
    let lid = LocalIdentityDirectory::new(&db)
        .resolve(oid)
        .unwrap()
        .unwrap();
    let rid = LocalReplicaDirectory::new(&db)
        .resolve_local(lid)
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            LocalPlacementResolver::new(&db).resolve(rid).unwrap(),
            Some(Placement::Segment(_))
        ),
        "flushed replica is Segment-placed"
    );
    // The newest-wins write lands in the memtable — placement flips.
    db.put_object(oid, b"k0", b"v2").unwrap();
    assert!(
        matches!(
            LocalPlacementResolver::new(&db).resolve(rid).unwrap(),
            Some(Placement::Memtable { .. })
        ),
        "a put flips the placement back to the memtable before its ack"
    );
    assert_eq!(
        db.get_object(oid, b"k0").unwrap(),
        Some(b"v2".to_vec()),
        "the memtable holds the newest row and answers pre-flush"
    );
    drop(db);
    let db = Db::open(Config::new(d.to_path_buf())).unwrap();
    assert!(
        matches!(
            LocalPlacementResolver::new(&db).resolve(rid).unwrap(),
            Some(Placement::Memtable { .. })
        ),
        "WAL replay re-applies the flip"
    );
    assert_eq!(
        db.get_object(oid, b"k0").unwrap(),
        Some(b"v2".to_vec()),
        "the newest row answers across the restart"
    );
}
