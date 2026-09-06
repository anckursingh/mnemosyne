//! SE2-M31 — local replica directory + topology (spec §33 milestone 3):
//! RP-001..005 — one local ReplicaId per LogicalId, stable resolution,
//! restart persistence, no reuse, and the type-level distinctness pin.

mod common;

use aikoql_storage_v2::db::{Config, Db};
use aikoql_storage_v2::identity::topology::{
    LocalReplicaDirectory, LocalTopology, ReplicaDirectory, ReplicaTopology,
};
use aikoql_storage_v2::identity::{LogicalId, NodeId, ReplicaId, LOCAL_NODE_ID};
use common::dir;
use std::any::TypeId;
use std::collections::HashSet;

#[test]
fn rp001_logical_id_resolves_to_exactly_one_local_replica() {
    let d = dir("topo-rp001");
    let db = Db::open(Config::new(d)).unwrap();
    let directory = LocalReplicaDirectory::new(&db);
    let topology = LocalTopology::new(&db);

    let mut rids = HashSet::new();
    for _ in 0..32 {
        let oid = db.create_object().unwrap();
        let lid = db.resolve_object(oid).expect("created object resolves");
        let rid = directory
            .resolve_local(lid)
            .unwrap()
            .expect("a local replica exists");
        assert!(rids.insert(rid), "two logical ids must not share a replica");
        // Exactly one descriptor, on the local node (the MVP topology
        // shape — §10: the future shape is the list of nodes).
        let descriptors = topology.replicas_for(lid).unwrap();
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].node, LOCAL_NODE_ID);
        assert_eq!(descriptors[0].replica, rid);
    }
    assert_eq!(rids.len(), 32);
}

#[test]
fn rp002_same_logical_id_always_resolves_to_same_replica() {
    let d = dir("topo-rp002");
    let db = Db::open(Config::new(d)).unwrap();
    let oid = db.create_object().unwrap();
    let lid = db.resolve_object(oid).unwrap();

    // Resolution is stable across independently constructed views.
    let a = LocalReplicaDirectory::new(&db).resolve_local(lid).unwrap();
    let b = LocalReplicaDirectory::new(&db).resolve_local(lid).unwrap();
    assert_eq!(a, b);
    assert!(a.is_some());

    // And the topology surface agrees with the directory surface.
    let descriptors = LocalTopology::new(&db).replicas_for(lid).unwrap();
    assert_eq!(descriptors.len(), 1);
    assert_eq!(descriptors[0].replica, a.unwrap());
}

#[test]
fn rp003_restart_preserves_logical_to_replica() {
    let d = dir("topo-rp003");
    let mappings = {
        let db = Db::open(Config::new(d.clone())).unwrap();
        let mut lids = Vec::new();
        for _ in 0..3 {
            let oid = db.create_object().unwrap();
            lids.push(db.resolve_object(oid).unwrap());
        }
        db.flush().unwrap(); // the delta-log recovery path
        for _ in 0..2 {
            let oid = db.create_object().unwrap();
            lids.push(db.resolve_object(oid).unwrap());
        } // the WAL replay recovery path
        let directory = LocalReplicaDirectory::new(&db);
        lids.into_iter()
            .map(|lid| {
                let rid = directory.resolve_local(lid).unwrap().expect("resolves");
                (lid, rid)
            })
            .collect::<Vec<_>>()
    };

    let db = Db::open(Config::new(d)).unwrap();
    let directory = LocalReplicaDirectory::new(&db);
    for (lid, rid) in mappings {
        assert_eq!(directory.resolve_local(lid).unwrap(), Some(rid));
    }
}

#[test]
fn rp004_replica_id_is_never_reused() {
    let d = dir("topo-rp004");
    let (max_before, seen) = {
        let db = Db::open(Config::new(d.clone())).unwrap();
        let directory = LocalReplicaDirectory::new(&db);
        let mut seen = HashSet::new();
        for _ in 0..16 {
            let oid = db.create_object().unwrap();
            let lid = db.resolve_object(oid).unwrap();
            seen.insert(directory.resolve_local(lid).unwrap().unwrap());
        }
        db.flush().unwrap();
        (*seen.iter().max().unwrap(), seen)
    };

    // After restart the allocator resumes past the observed maximum —
    // nothing previously handed out (even a crashed gap) comes back.
    let db = Db::open(Config::new(d)).unwrap();
    let directory = LocalReplicaDirectory::new(&db);
    for _ in 0..16 {
        let oid = db.create_object().unwrap();
        let lid = db.resolve_object(oid).unwrap();
        let rid = directory.resolve_local(lid).unwrap().unwrap();
        assert!(rid > max_before, "allocator must not rewind across restart");
        assert!(!seen.contains(&rid), "replica ids are never reused");
    }
}

#[test]
fn rp005_logical_and_replica_ids_remain_distinct_types() {
    // The §6.3/§28.2 rule, in the topology context: identical size,
    // different TypeId — distinctness is type-level, never size-level.
    assert_ne!(TypeId::of::<LogicalId>(), TypeId::of::<ReplicaId>());
    assert_eq!(
        std::mem::size_of::<LogicalId>(),
        std::mem::size_of::<ReplicaId>()
    );
    assert_eq!(
        std::mem::size_of::<ReplicaId>(),
        std::mem::size_of::<NodeId>()
    );

    // §9/§10: the traits are declared Send + Sync — the MVP views must
    // satisfy the bound (a compile-time pin, no value needed).
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<LocalReplicaDirectory<'static>>();
    assert_send_sync::<LocalTopology<'static>>();
}
