//! SE2-M29 — strong identity types (artifacts/storage-engine-v2/
//! logical-id-physical-id.md §31 milestone 1; docs/TESTING-PLAN-V2.md row
//! SE2-M29).
//!
//! The identity layer's foundation: ObjectId, LogicalId, ReplicaId, NodeId,
//! SegmentId, BlockId are distinct Rust newtypes. ID-004 (substitution must
//! not compile) lives as a compile_fail doc-test on the identity module —
//! the §6.3 example verbatim. ID-005: representations persist byte-exactly,
//! and distinctness is type-level even where the representation is
//! identical (LogicalId(42) and ReplicaId(42) share size 8 but not TypeId).

use aikoql_storage_v2::identity::{LogicalId, NodeId, ObjectId, ReplicaId, LOCAL_NODE_ID};
use aikoql_storage_v2::placement::{BlockId, SegmentId};
use std::any::TypeId;
use std::collections::HashSet;
use std::mem::size_of;

#[test]
fn object_id_equality_and_hash() {
    let a = ObjectId::from_bytes([0x01; 16]);
    let b = ObjectId::from_bytes([0x01; 16]);
    let c = ObjectId::from_bytes([0x02; 16]);
    assert_eq!(a, b);
    assert_ne!(a, c);
    let mut set = HashSet::new();
    set.insert(a);
    assert!(set.contains(&b));
    assert!(!set.contains(&c));
}

#[test]
fn logical_id_equality_and_hash() {
    let a = LogicalId(42);
    let b = LogicalId(42);
    let c = LogicalId(43);
    assert_eq!(a, b);
    assert_ne!(a, c);
    let mut set = HashSet::new();
    set.insert(a);
    assert!(set.contains(&b));
    assert!(!set.contains(&c));
}

#[test]
fn replica_id_equality_and_hash() {
    let a = ReplicaId(501);
    let b = ReplicaId(501);
    let c = ReplicaId(502);
    assert_eq!(a, b);
    assert_ne!(a, c);
    let mut set = HashSet::new();
    set.insert(a);
    assert!(set.contains(&b));
    assert!(!set.contains(&c));
}

#[test]
fn node_id_equality_and_hash() {
    let a = NodeId(7);
    let b = NodeId(7);
    let c = NodeId(8);
    assert_eq!(a, b);
    assert_ne!(a, c);
    // §6.4 — the MVP's one storage node; the replica directory keys on it.
    assert_eq!(LOCAL_NODE_ID, NodeId(1));
}

#[test]
fn segment_and_block_id_equality_and_hash() {
    let a = SegmentId(42);
    let b = SegmentId(42);
    let c = SegmentId(99);
    assert_eq!(a, b);
    assert_ne!(a, c);
    let mut set = HashSet::new();
    set.insert(a);
    assert!(set.contains(&b));
    assert!(!set.contains(&c));

    let x = BlockId(18);
    let y = BlockId(18);
    let z = BlockId(19);
    assert_eq!(x, y);
    assert_ne!(x, z);
    let mut set = HashSet::new();
    set.insert(x);
    assert!(set.contains(&y));
    assert!(!set.contains(&z));
}

#[test]
fn ids_order_documenting_the_ordering_requirement() {
    // §6.1 — ordering requirements documented by the derives: u64 ids order
    // by value, ObjectId byte-lexicographic (map-key friendly, deterministic).
    assert!(LogicalId(1) < LogicalId(2));
    assert!(ReplicaId(9) > ReplicaId(3));
    assert!(NodeId(1) < NodeId(2));
    assert!(SegmentId(10) < SegmentId(99));
    assert!(BlockId(4) < BlockId(17));
    assert!(ObjectId::from_bytes([0u8; 16]) < ObjectId::from_bytes([1u8; 16]));
    assert!(
        ObjectId::from_bytes([0xff; 16]) > ObjectId::from_bytes([0x00; 16]),
        "byte-lexicographic, not numeric"
    );
}

#[test]
fn distinct_types_even_with_equal_representation() {
    // §6.3 / §28.2 — LogicalId and ReplicaId share size and value domain,
    // but the type system must keep them apart: same size, different types.
    // (The compile-time half of the rule is ID-004's compile_fail doc-test.)
    assert_eq!(size_of::<LogicalId>(), 8);
    assert_eq!(size_of::<ReplicaId>(), 8);
    assert_eq!(size_of::<NodeId>(), 8);
    assert_eq!(size_of::<SegmentId>(), 8);
    assert_eq!(size_of::<BlockId>(), 4);
    assert_eq!(size_of::<ObjectId>(), 16);
    assert_ne!(TypeId::of::<LogicalId>(), TypeId::of::<ReplicaId>());
    assert_ne!(TypeId::of::<LogicalId>(), TypeId::of::<NodeId>());
    assert_ne!(TypeId::of::<ReplicaId>(), TypeId::of::<NodeId>());
    assert_ne!(TypeId::of::<SegmentId>(), TypeId::of::<LogicalId>());
    assert_ne!(TypeId::of::<SegmentId>(), TypeId::of::<BlockId>());
}

#[test]
fn object_id_persists_byte_exactly() {
    // ID-005 — persistence is the raw 16 bytes, never a printable form.
    let bytes = [
        0xde, 0xad, 0xbe, 0xef, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa,
        0xbb,
    ];
    let id = ObjectId::from_bytes(bytes);
    assert_eq!(id.to_bytes(), bytes, "ObjectId golden bytes");
    assert_eq!(ObjectId::from_bytes(id.to_bytes()), id);
}

#[test]
fn u64_ids_persist_byte_exactly_little_endian() {
    let l = LogicalId(0x0102_0304_0506_0708);
    assert_eq!(
        l.to_bytes(),
        [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01],
        "LogicalId little-endian golden"
    );
    assert_eq!(LogicalId::from_bytes(l.to_bytes()), l);

    let r = ReplicaId(0x1122_3344_5566_7788);
    assert_eq!(ReplicaId::from_bytes(r.to_bytes()), r);

    let n = NodeId(0xdead_beef_cafe_f00d);
    assert_eq!(NodeId::from_bytes(n.to_bytes()), n);
}

#[test]
fn segment_and_block_ids_persist_byte_exactly() {
    let s = SegmentId(0x0102_0304_0506_0708);
    assert_eq!(SegmentId::from_bytes(s.to_bytes()), s);

    let b = BlockId(0x0102_0304);
    assert_eq!(b.to_bytes(), [0x04, 0x03, 0x02, 0x01], "BlockId LE golden");
    assert_eq!(BlockId::from_bytes(b.to_bytes()), b);
}

#[test]
fn object_id_display_is_lowercase_hex() {
    // Diagnostics: stable, dense, copy-pasteable — never a Debug dump.
    let id = ObjectId::from_bytes([0xab; 16]);
    assert_eq!(id.to_string(), "ab".repeat(16));
    assert_eq!(
        format!("{:?}", ObjectId::from_bytes([0x01; 16])),
        "ObjectId(01010101010101010101010101010101)"
    );
}
