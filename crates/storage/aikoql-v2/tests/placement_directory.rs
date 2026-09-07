//! SE2-M32 — placement directory (spec §34 milestone 4): PL-001..005 —
//! ReplicaId → PhysicalLocation resolution, unknown → None, restart
//! persistence, atomic replacement, and the generation gate — plus the
//! M0-pattern format contracts (round-trip, damage classes, structural
//! validation).

mod common;

use aikoql_storage_v2::db::{Config, Db};
use aikoql_storage_v2::format::FormatError;
use aikoql_storage_v2::identity::topology::{LocalReplicaDirectory, ReplicaDirectory};
use aikoql_storage_v2::identity::ReplicaId;
use aikoql_storage_v2::placement::{
    placement_log_path, validate_segment_location, ApplyOutcome, BlockId, LocalPlacementResolver,
    PhysicalLocation, Placement, PlacementDirectory, PlacementLog, PlacementRecord,
    PlacementResolver, SegmentId,
};
use common::dir;
use std::collections::HashSet;

fn loc(segment: u64, block: u32, entry: u32, generation: u64) -> PhysicalLocation {
    PhysicalLocation {
        segment_id: SegmentId(segment),
        block_id: BlockId(block),
        entry_offset: entry,
        generation,
    }
}

/// The full identity → replica chain, through the M30/M31 surfaces: a
/// created object gets its LogicalId and then its one local ReplicaId.
fn create_rid(db: &Db) -> ReplicaId {
    let oid = db.create_object().unwrap();
    let lid = db.resolve_object(oid).unwrap();
    LocalReplicaDirectory::new(db)
        .resolve_local(lid)
        .unwrap()
        .unwrap()
}

#[test]
fn pl001_replica_resolves_to_a_physical_location() {
    let mut directory = PlacementDirectory::default();
    let segment = loc(42, 18, 7, 99);
    directory
        .apply(PlacementRecord {
            rid: ReplicaId(501),
            placement: Placement::Segment(segment),
        })
        .unwrap();
    assert_eq!(
        directory.resolve(ReplicaId(501)),
        Some(&Placement::Segment(segment))
    );

    // §14: a fresh create's initial placement is Memtable — it resolves too.
    directory
        .apply(PlacementRecord {
            rid: ReplicaId(502),
            placement: Placement::Memtable { generation: 7 },
        })
        .unwrap();
    assert_eq!(
        directory.resolve(ReplicaId(502)),
        Some(&Placement::Memtable { generation: 7 })
    );
}

#[test]
fn pl002_unknown_replica_resolves_none() {
    let mut directory = PlacementDirectory::default();
    directory
        .apply(PlacementRecord {
            rid: ReplicaId(501),
            placement: Placement::Memtable { generation: 1 },
        })
        .unwrap();
    assert_eq!(
        directory.resolve(ReplicaId(501)),
        Some(&Placement::Memtable { generation: 1 })
    );
    assert_eq!(directory.resolve(ReplicaId(999)), None);
    assert_eq!(directory.resolve(ReplicaId(0)), None);
}

#[test]
fn pl003_placement_survives_restart() {
    let d = dir("plac-pl003");
    let rids = {
        let db = Db::open(Config::new(d.clone())).unwrap();
        let mut rids = Vec::new();
        for _ in 0..3 {
            rids.push(create_rid(&db));
        }
        db.flush().unwrap(); // the placement-log recovery path
        for _ in 0..2 {
            rids.push(create_rid(&db));
        } // the WAL replay recovery path
        let resolver = LocalPlacementResolver::new(&db);
        for &rid in &rids {
            // §14 — physical placement initially = Memtable.
            assert!(
                matches!(
                    resolver.resolve(rid).unwrap(),
                    Some(Placement::Memtable { .. })
                ),
                "fresh create resolves to a Memtable placement"
            );
        }
        rids
    };

    let db = Db::open(Config::new(d)).unwrap();
    let resolver = LocalPlacementResolver::new(&db);
    for rid in rids {
        assert!(
            matches!(
                resolver.resolve(rid).unwrap(),
                Some(Placement::Memtable { generation })
                    if generation >= 1
            ),
            "placement survives restart, still Memtable"
        );
    }
}

#[test]
fn pl004_placement_update_replaces_location_atomically() {
    let mut directory = PlacementDirectory::default();
    directory
        .apply(PlacementRecord {
            rid: ReplicaId(501),
            placement: Placement::Memtable { generation: 5 },
        })
        .unwrap();
    let segment = loc(42, 1, 0, 6);
    directory
        .apply(PlacementRecord {
            rid: ReplicaId(501),
            placement: Placement::Segment(segment),
        })
        .unwrap();
    // The whole placement is replaced — no Memtable residue, one entry.
    assert_eq!(
        directory.resolve(ReplicaId(501)),
        Some(&Placement::Segment(segment))
    );
    assert_eq!(directory.len(), 1);
}

#[test]
fn pl005_older_generation_never_overwrites_newer() {
    let mut directory = PlacementDirectory::default();
    let rid = ReplicaId(501);
    let newer = loc(42, 1, 0, 6);
    directory
        .apply(PlacementRecord {
            rid,
            placement: Placement::Segment(newer),
        })
        .unwrap();

    // An older generation is stale: ignored, never replaces (§25).
    let stale = PlacementRecord {
        rid,
        placement: Placement::Memtable { generation: 5 },
    };
    assert_eq!(directory.apply(stale).unwrap(), ApplyOutcome::Stale);
    assert_eq!(directory.resolve(rid), Some(&Placement::Segment(newer)));

    // An identical repeat (crash-window double-apply) is a no-op.
    let duplicate = PlacementRecord {
        rid,
        placement: Placement::Segment(newer),
    };
    assert_eq!(directory.apply(duplicate).unwrap(), ApplyOutcome::Duplicate);
    assert_eq!(directory.resolve(rid), Some(&Placement::Segment(newer)));

    // A strictly newer generation replaces.
    let newer2 = loc(99, 2, 1, 7);
    assert_eq!(
        directory
            .apply(PlacementRecord {
                rid,
                placement: Placement::Segment(newer2)
            })
            .unwrap(),
        ApplyOutcome::Applied
    );
    assert_eq!(directory.resolve(rid), Some(&Placement::Segment(newer2)));

    // Equal generation, different record = protocol violation (every
    // update allocates a fresh generation) — fails closed.
    let err = directory
        .apply(PlacementRecord {
            rid,
            placement: Placement::Memtable { generation: 7 },
        })
        .unwrap_err();
    assert!(
        matches!(err, FormatError::Corrupt(_)),
        "equal-generation replacement must fail closed"
    );
    assert_eq!(directory.resolve(rid), Some(&Placement::Segment(newer2)));
}

#[test]
fn placement_log_roundtrips_all_variants() {
    let log = PlacementLog {
        format_version: 1,
        generation: 3,
        records: vec![
            PlacementRecord {
                rid: ReplicaId(501),
                placement: Placement::Memtable { generation: 5 },
            },
            PlacementRecord {
                rid: ReplicaId(501),
                placement: Placement::Segment(loc(42, 18, 7, 99)),
            },
            PlacementRecord {
                rid: ReplicaId(502),
                placement: Placement::Retired { generation: 8 },
            },
        ],
    };
    let bytes = log.encode();
    let decoded = PlacementLog::decode(&bytes).unwrap();
    assert_eq!(decoded, log);
    assert_eq!(bytes.len(), 18 + 3 * 33 + 8); // header + fixed records + checksum
}

#[test]
fn placement_log_fails_closed_on_damage() {
    let log = PlacementLog {
        format_version: 1,
        generation: 1,
        records: vec![PlacementRecord {
            rid: ReplicaId(501),
            placement: Placement::Segment(loc(42, 18, 7, 99)),
        }],
    };
    let bytes = log.encode();
    assert_eq!(PlacementLog::decode(&bytes).unwrap(), log);

    // Bad magic.
    let mut bad = bytes.clone();
    bad[0] ^= 0xFF;
    assert!(matches!(
        PlacementLog::decode(&bad),
        Err(FormatError::Corrupt(_))
    ));
    // Unknown format version is Unsupported (a newer-format file is not
    // damaged), not Corrupt.
    let mut newer = bytes.clone();
    newer[4..6].copy_from_slice(&2u16.to_le_bytes());
    assert!(matches!(
        PlacementLog::decode(&newer),
        Err(FormatError::Unsupported(_))
    ));
    // Truncation.
    assert!(matches!(
        PlacementLog::decode(&bytes[..bytes.len() - 3]),
        Err(FormatError::Corrupt(_))
    ));
    // Trailing bytes.
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(matches!(
        PlacementLog::decode(&trailing),
        Err(FormatError::Corrupt(_))
    ));
    // Checksum mismatch.
    let mut flipped = bytes.clone();
    let n = flipped.len();
    flipped[n - 9] ^= 0x01; // a record byte, not the checksum tail
    assert!(matches!(
        PlacementLog::decode(&flipped),
        Err(FormatError::Corrupt(_))
    ));
}

#[test]
fn placement_log_publish_and_recover_roundtrip() {
    let d = dir("plac-log-roundtrip");
    let log = PlacementLog {
        format_version: 1,
        generation: 2,
        records: vec![PlacementRecord {
            rid: ReplicaId(501),
            placement: Placement::Segment(loc(42, 18, 7, 99)),
        }],
    };
    PlacementLog::publish(&placement_log_path(&d, 2), &log).unwrap();

    // recover applies logs <= CURRENT and validates Segment placements
    // through the caller's closure (the Db passes manifest + reader bounds).
    let mut validated = Vec::new();
    let directory = PlacementDirectory::recover(&d, 2, &mut |loc| {
        validated.push(*loc);
        Ok(())
    })
    .unwrap();
    assert_eq!(validated.len(), 1);
    assert_eq!(
        directory.resolve(ReplicaId(501)),
        Some(&Placement::Segment(loc(42, 18, 7, 99)))
    );

    // A validation failure fails closed — no partial state.
    let err = PlacementDirectory::recover(&d, 2, &mut |_| {
        Err(FormatError::Corrupt("validation refused".into()))
    })
    .unwrap_err();
    assert!(matches!(err, FormatError::Corrupt(_)));
}

#[test]
fn segment_placement_validation() {
    let good = loc(42, 18, 7, 99);
    let ids: HashSet<u64> = HashSet::from([42u64]);

    // Valid: segment in the manifest set, block and entry within range.
    assert!(validate_segment_location(&good, &ids, Some(16)).is_ok());

    // Segment not referenced by the manifest.
    let unknown = loc(43, 0, 0, 99);
    assert!(matches!(
        validate_segment_location(&unknown, &ids, Some(16)),
        Err(FormatError::Corrupt(_))
    ));

    // Block id past the segment's block count (None = no such block).
    let bad_block = loc(42, 32, 0, 99);
    assert!(matches!(
        validate_segment_location(&bad_block, &ids, None),
        Err(FormatError::Corrupt(_))
    ));

    // Entry offset past the block's entry count.
    let bad_entry = loc(42, 18, 16, 99);
    assert!(matches!(
        validate_segment_location(&bad_entry, &ids, Some(16)),
        Err(FormatError::Corrupt(_))
    ));
}
