//! SE2-M34 — block format v3 (docs/IMPLEMENTATION-PLAN-V2.md SE2-M34,
//! docs/TESTING-PLAN-V2.md row SE2-M34).
//!
//! v3 data blocks = the v2 restart table + a per-entry `replica_id u64`
//! appended after the flags byte. The segment-level layout is untouched
//! and the v1/v2 read path never changes: the reader dispatches on the
//! block-header version (1 = plain, 2 = restarts, 3 = restarts + rid) and
//! the v1/v2 writers never emit the field — their bytes stay golden-pinned.
//! The write side adds `new_v3` + `publish_with_anchors`: a flushed
//! identity-carrying memtable becomes v3 blocks whose entries carry their
//! owning replica id, and the per-rid anchor (the max-seq entry's
//! block/offset) comes back with the publication for the placement
//! directory.

mod common;

use aikoql_storage_v2::identity::{ObjectId, ReplicaId};
use aikoql_storage_v2::segment::{
    SegmentEntry, SegmentReader, SegmentWriter, FLAG_DELETE, FLAG_PUT, FLAG_VERSION,
};
use common::{dir, entry, hex, tmp};
use std::collections::HashMap;

fn oid(byte: u8) -> ObjectId {
    ObjectId([byte; 16])
}

fn e3(key: &str, value: &str, seq: u64, flags: u8, rid: u64) -> SegmentEntry {
    SegmentEntry {
        key: key.as_bytes().to_vec(),
        value: value.as_bytes().to_vec(),
        seq,
        flags,
        replica_id: ReplicaId(rid),
    }
}

/// Anchors come back in no guaranteed order (the writer groups by rid in a
/// map) — sort by rid for deterministic asserts.
fn sorted(
    mut anchors: Vec<aikoql_storage_v2::segment::SegmentAnchor>,
) -> Vec<aikoql_storage_v2::segment::SegmentAnchor> {
    anchors.sort_by_key(|a| a.replica_id.0);
    anchors
}

#[test]
fn block_v3_roundtrip_with_rids() {
    // Entries across two blocks (tiny target), several rids, multi-key
    // objects: every read surface returns the rid the writer was given —
    // the v3 decode is lossless.
    let mut w = SegmentWriter::new_v3(64);
    w.push(e3("a1", "v1", 5, FLAG_PUT, 11));
    w.push(e3("a2", "v2", 7, FLAG_VERSION, 22));
    w.push(e3("a3", "v3", 9, FLAG_DELETE, 11));
    w.push(e3("b1", "v4", 6, FLAG_PUT, 22));
    let path = tmp("blockv3-roundtrip");
    w.publish(&path).unwrap();
    let reader = SegmentReader::open(&path).unwrap();
    let got = reader.get(b"a1").unwrap().unwrap();
    assert_eq!(got.replica_id, ReplicaId(11));
    assert_eq!(got.value, b"v1".to_vec());
    let got = reader.get(b"a3").unwrap().unwrap();
    assert_eq!(
        got.replica_id,
        ReplicaId(11),
        "rids are per entry, not per key"
    );
    let versions = reader.versions(b"a2").unwrap();
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].replica_id, ReplicaId(22));
    let all = reader.scan(b"", b"\xff").unwrap();
    let rids: Vec<_> = all.iter().map(|e| e.replica_id).collect();
    assert_eq!(
        rids,
        vec![ReplicaId(11), ReplicaId(22), ReplicaId(11), ReplicaId(22)],
        "every entry carries its rid through the scan"
    );
}

#[test]
fn block_v3_matches_v2_for_zero_rids() {
    // The same rid-0 entry set through the v2 and v3 writers: identical
    // answers on every read surface. v3 is v2 + the rid field — with zero
    // rids the two formats serve the same logical rows.
    let mut entries = Vec::new();
    for i in 0..40u64 {
        entries.push(entry(
            &format!("k{i:03}"),
            &format!("v{i:03}"),
            1000 + i,
            FLAG_PUT,
        ));
    }
    let mut w2 = SegmentWriter::new_v2(200);
    let mut w3 = SegmentWriter::new_v3(200);
    for e in &entries {
        w2.push(e.clone());
        w3.push(e.clone());
    }
    let p2 = tmp("blockv3-parity-v2");
    w2.publish(&p2).unwrap();
    let p3 = tmp("blockv3-parity-v3");
    w3.publish(&p3).unwrap();
    let r2 = SegmentReader::open(&p2).unwrap();
    let r3 = SegmentReader::open(&p3).unwrap();
    assert_eq!(
        r2.scan(b"", b"\xff").unwrap(),
        r3.scan(b"", b"\xff").unwrap(),
        "v2 and v3 scans must be identical for zero rids"
    );
    for e in &entries {
        assert_eq!(r2.get(&e.key).unwrap(), r3.get(&e.key).unwrap());
        assert_eq!(r2.versions(&e.key).unwrap(), r3.versions(&e.key).unwrap());
    }
    let a: Vec<SegmentEntry> = r2.iter().map(|r| r.unwrap()).collect();
    let b: Vec<SegmentEntry> = r3.iter().map(|r| r.unwrap()).collect();
    assert_eq!(a, b, "v2 and v3 streaming iterators must be identical");
}

#[test]
fn block_v3_anchors_point_at_each_rids_head() {
    // publish_with_anchors returns one anchor per rid: the max-seq entry's
    // (block, offset). The oracle is the full decode — no anchor may name
    // anything but that rid's newest entry, and byte-API rows (rid 0)
    // never anchor.
    let mut w = SegmentWriter::new_v3(64);
    // rid 1: three keys, head seq 9. rid 2: two keys, head seq 8. Plus a
    // byte-API row (rid 0, seq 10) that must NOT anchor.
    w.push(e3("a1", "v1", 5, FLAG_PUT, 1));
    w.push(e3("a2", "v2", 9, FLAG_PUT, 1));
    w.push(e3("a3", "v3", 7, FLAG_PUT, 1));
    w.push(e3("b1", "v4", 8, FLAG_PUT, 2));
    w.push(e3("b2", "v5", 4, FLAG_PUT, 2));
    w.push(e3("byte", "raw", 10, FLAG_PUT, 0));
    let path = tmp("blockv3-anchors");
    let (file_size, checksum, anchors) = w.publish_with_anchors(&path).unwrap();

    // publish() is the thin wrapper: same file, same (size, checksum).
    let mut w2 = SegmentWriter::new_v3(64);
    w2.push(e3("a1", "v1", 5, FLAG_PUT, 1));
    w2.push(e3("a2", "v2", 9, FLAG_PUT, 1));
    w2.push(e3("a3", "v3", 7, FLAG_PUT, 1));
    w2.push(e3("b1", "v4", 8, FLAG_PUT, 2));
    w2.push(e3("b2", "v5", 4, FLAG_PUT, 2));
    w2.push(e3("byte", "raw", 10, FLAG_PUT, 0));
    let path2 = tmp("blockv3-anchors-wrapper");
    assert_eq!(w2.publish(&path2).unwrap(), (file_size, checksum));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        std::fs::read(&path2).unwrap(),
        "publish() must write exactly the bytes publish_with_anchors writes"
    );

    let reader = SegmentReader::open(&path).unwrap();
    let anchors = sorted(anchors);
    assert_eq!(anchors.len(), 2, "one anchor per non-zero rid");
    let all = reader.scan(b"", b"\xff").unwrap();
    let max_seq: HashMap<ReplicaId, u64> = all
        .iter()
        .filter(|e| e.replica_id != ReplicaId(0))
        .fold(HashMap::new(), |mut m, e| {
            let cur = m.entry(e.replica_id).or_insert(0);
            *cur = (*cur).max(e.seq);
            m
        });
    for a in &anchors {
        assert_eq!(a.seq, max_seq[&a.replica_id], "anchor = the rid's head");
        let e = reader
            .entry_at(a.block_id, a.entry_offset)
            .unwrap()
            .expect("anchor names an entry");
        assert_eq!(e.replica_id, a.replica_id);
        assert_eq!(e.seq, a.seq);
        let want = if a.replica_id == ReplicaId(1) {
            b"a2".to_vec()
        } else {
            b"b1".to_vec()
        };
        assert_eq!(e.key, want, "the anchor is each rid's head key");
    }
    assert_eq!(anchors[0].replica_id, ReplicaId(1));
    assert_eq!(anchors[1].replica_id, ReplicaId(2));
}

#[test]
fn block_v3_anchors_span_blocks() {
    // Enough entries to force several blocks: anchors must land in the
    // block their entry actually occupies (block ids and offsets are
    // block-local, verified against the full decode per block via
    // entry_at over every block).
    let mut w = SegmentWriter::new_v3(48);
    for i in 0..60u64 {
        // Two rids alternating; each rid's head is the LAST key it owns
        // (seq = i, strictly increasing).
        w.push(e3(
            &format!("k{i:03}"),
            &format!("v{i:03}"),
            i,
            FLAG_PUT,
            i % 2 + 1,
        ));
    }
    let path = tmp("blockv3-anchors-multiblock");
    let (_, _, anchors) = w.publish_with_anchors(&path).unwrap();
    let reader = SegmentReader::open(&path).unwrap();
    assert_eq!(anchors.len(), 2);
    for a in &anchors {
        let e = reader
            .entry_at(a.block_id, a.entry_offset)
            .unwrap()
            .unwrap();
        assert_eq!(e.replica_id, a.replica_id);
        // Even keys are rid 1's (last 58), odd are rid 2's (last 59).
        assert_eq!(
            e.key,
            format!("k{:03}", 58 + a.replica_id.0 - 1).into_bytes(),
            "the head is each rid's newest key"
        );
    }
    assert!(
        anchors.iter().any(|a| a.block_id.0 > 0),
        "with 48-byte blocks the anchors must span past block 0"
    );
}

#[test]
fn block_v3_lookup_with_rid_filter_is_bounded() {
    // One 10 000-entry v3 block: the rid-filtered lookup (the get_object
    // segment path) decodes at most one restart interval — the same bound
    // the byte-API v2 lookup holds (QA M2 gate), the rid filter
    // notwithstanding.
    const N: usize = 10_000;
    let mut cfg = aikoql_storage_v2::db::Config::new(dir("blockv3-bounded"));
    cfg.memtable_bytes = usize::MAX;
    cfg.block_target = 1 << 20; // one block for the whole dataset
    let db = aikoql_storage_v2::db::Db::open(cfg).unwrap();
    let a = oid(0xA1);
    for i in 0..N {
        if i % 2 == 0 {
            // One object (the even keys); the odd keys are byte-API rows.
            // get_object must answer the object's rows alone (§11).
            db.put_object(a, format!("key-{i:06}").as_bytes(), b"value")
                .unwrap();
        } else {
            db.put(format!("key-{i:06}").as_bytes(), b"raw").unwrap();
        }
    }
    db.flush().unwrap();
    let last = format!("key-{:06}", N - 2);
    assert_eq!(
        db.get_object(a, last.as_bytes()).unwrap(),
        Some(b"value".to_vec())
    );
    let absent = format!("key-{:06}", N - 1);
    assert_eq!(
        db.get_object(a, absent.as_bytes()).unwrap(),
        None,
        "a byte-API row never answers an object read"
    );
    let s = db.read_path_stats();
    assert!(
        s.entries_decoded <= 2 * 16,
        "two rid-filtered lookups decode at most two restart intervals, decoded {}",
        s.entries_decoded
    );
}

#[test]
fn block_v3_entry_at_bounds() {
    let mut w = SegmentWriter::new_v3(64);
    w.push(e3("a1", "v1", 5, FLAG_PUT, 11));
    let path = tmp("blockv3-entry-at");
    w.publish(&path).unwrap();
    let reader = SegmentReader::open(&path).unwrap();
    assert!(reader
        .entry_at(aikoql_storage_v2::placement::BlockId(0), 0)
        .unwrap()
        .is_some());
    assert!(
        reader
            .entry_at(aikoql_storage_v2::placement::BlockId(0), 1)
            .unwrap()
            .is_none(),
        "past the block's entries"
    );
    assert!(
        reader
            .entry_at(aikoql_storage_v2::placement::BlockId(1), 0)
            .unwrap()
            .is_none(),
        "past the segment's blocks"
    );
}

#[test]
fn block_v3_golden() {
    // The fixture was computed independently in python (hashlib + struct)
    // from the v3 payload spec — same inputs as the v1/v2 goldens plus a
    // rid per entry — so a format change is a visible diff. Everything
    // outside the data block (header, index, bloom, footer construction)
    // is shared with the v2 golden: the two files differ ONLY in the data
    // block (version 3, +8 rid bytes per entry, checksums).
    let mut w = SegmentWriter::new_v3(4096);
    w.push(e3("a3", "v3", 9, FLAG_DELETE, 3));
    w.push(e3("a1", "v1", 5, FLAG_PUT, 1));
    w.push(e3("a2", "v2", 7, FLAG_VERSION, 2));
    let path = tmp("block-v3-golden");
    w.publish(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        hex(&bytes),
        concat!(
            "414b534501000100000003000000000000000200000061310200000061330500",
            "0000000000000900000000000000c4db5c102ef23785414b424c030000000300",
            "00005f0000005f0000002e4f7329e8e0b01c1000010000000a00000000000200",
            "6131020000007631050000000000000001010000000000000001000100320200",
            "0000763207000000000000000402000000000000000100010033020000007633",
            "0900000000000000020300000000000000414b424c0100010001000000140000",
            "0014000000742fcc49dbdc873202006131020061333600000000000000030000",
            "00414b424c01000200030000000800000008000000f6d1b0a32f1ff4ae1e0000",
            "000ca50139414b4654010003000000000000004a97bb36434616e8",
        ),
        "segment golden bytes changed — format break"
    );
}
