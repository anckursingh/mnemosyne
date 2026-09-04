//! SE2-M9 — block format v2 REDs (docs/IMPLEMENTATION-PLAN-V2.md SE2-M9,
//! docs/TESTING-PLAN-V2.md row SE2-M9).
//!
//! v2 data blocks carry restart points: `restart_interval u16 |
//! restart_count u32 | restart offsets u32[] (absolute payload positions) |
//! entries` — a point lookup binary-searches the restarts (each restart
//! entry encodes its full key, shared = 0) and decodes only the one
//! interval slice it lands in. The segment-level layout is untouched —
//! header, index, bloom, footer — and v1 blocks stay readable: the block
//! header's version field (v1) is now 2 for data blocks, and the reader
//! serves both, failing closed on anything newer.

mod common;

use aikoql_kernel::knowledge::kom::sha256;
use aikoql_storage_v2::db::{Config, Db};
use aikoql_storage_v2::format::FormatError;
use aikoql_storage_v2::segment::{
    SegmentEntry, SegmentReader, SegmentWriter, FLAG_DELETE, FLAG_PUT, FLAG_VERSION,
};
use common::{dir, entry, hex, tmp};
use std::collections::HashMap;

/// sha256-8 — the format's checksum, re-implemented from the spec so the
/// fail-closed test does not lean on the engine's code.
fn c8(bytes: &[u8]) -> [u8; 8] {
    let d = sha256(bytes);
    d[..8].try_into().expect("sha256-8 slice")
}

/// xorshift64 — deterministic across tests.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

#[test]
fn block_v2_golden() {
    // The fixture was computed independently in python (hashlib + struct)
    // from the v2 payload spec, before the writer existed — a format change
    // is a visible diff. Same inputs as the M1 v1 golden, so the two files
    // differ ONLY in the data block: version 2 + restart table, everything
    // else (header, index, bloom, footer) byte-identical construction.
    let mut w = SegmentWriter::new_v2(4096);
    w.push(entry("a3", "v3", 9, FLAG_DELETE));
    w.push(entry("a1", "v1", 5, FLAG_PUT));
    w.push(entry("a2", "v2", 7, FLAG_VERSION));
    let path = tmp("block-v2-golden");
    w.publish(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        hex(&bytes),
        concat!(
            "414b534501000100000003000000000000000200000061310200000061330500",
            "0000000000000900000000000000c4db5c102ef23785414b424c020000000300",
            "00004700000047000000689cfde4219135221000010000000a00000000000200",
            "6131020000007631050000000000000001010001003202000000763207000000",
            "00000000040100010033020000007633090000000000000002414b424c010001",
            "00010000001400000014000000742fcc49dbdc87320200613102006133360000",
            "000000000003000000414b424c01000200030000000800000008000000f6d1b0",
            "a32f1ff4ae1e0000000ca50139414b465401000300000000000000c9853883c7",
            "5249ce",
        ),
        "segment golden bytes changed — format break"
    );
}

#[test]
fn block_v2_lookup_bounded() {
    // One 10 000-entry block: a point lookup decodes at most one restart
    // interval (16 entries) — the QA M2 gate — while answers stay exact.
    // Odd keys exist inside the block's range but are absent: the bounded
    // decode must answer None there too (not just for present keys).
    const N: usize = 10_000;
    let mut cfg = Config::new(dir("blockv2-bounded"));
    cfg.memtable_bytes = usize::MAX;
    cfg.block_target = 1 << 20; // one block for the whole dataset
    let db = Db::open(cfg).unwrap();
    for i in 0..N {
        if i % 2 == 0 {
            db.put(format!("key-{i:06}").as_bytes(), &[b'v'; 16][..])
                .unwrap();
        }
    }
    db.flush().unwrap();
    let last = format!("key-{:06}", N - 2);
    assert_eq!(
        db.get(last.as_bytes()).unwrap().as_deref(),
        Some(&[b'v'; 16][..])
    );
    let absent = format!("key-{:06}", N - 1);
    assert_eq!(db.get(absent.as_bytes()).unwrap(), None);
    let s = db.read_path_stats();
    assert_eq!(s.lookups, 2);
    assert_eq!(
        s.blocks_read, 1,
        "the second lookup hits the cache — one physical read total"
    );
    assert!(
        s.entries_decoded <= 2 * 16,
        "two lookups decode at most two restart intervals, decoded {}",
        s.entries_decoded
    );
}

#[test]
fn restart_search_correctness() {
    // Random sorted keys across many blocks × 100k lookups (present and
    // absent, in-range and out-of-range) vs a brute-force map: the
    // binary-searched bounded decode must agree with a full decode — 0
    // mismatches (the QA TC-PERF-0202 shape).
    const N: usize = 1000;
    const LOOKUPS: u64 = 100_000;
    let mut rng = Rng(0x9a11_5eed);
    let mut set: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    while set.len() < N {
        let k: Vec<u8> = (0..16).map(|_| rng.next() as u8).collect();
        let v: Vec<u8> = (0..8).map(|_| rng.next() as u8).collect();
        set.entry(k).or_insert(v);
    }
    let mut keys: Vec<Vec<u8>> = set.keys().cloned().collect();
    keys.sort();
    let mut writer = SegmentWriter::new_v2(4096);
    for k in &keys {
        writer.push(SegmentEntry {
            key: k.clone(),
            value: set[k].clone(),
            seq: 1,
            flags: FLAG_PUT,
        });
    }
    let path = tmp("blockv2-restart");
    writer.publish(&path).unwrap();
    let reader = SegmentReader::open(&path).unwrap();
    for _ in 0..LOOKUPS {
        let r = rng.next();
        let key: Vec<u8> = if r & 1 == 0 {
            keys[(r >> 1) as usize % N].clone()
        } else {
            // Absent random bytes — mostly out of range, sometimes inside a
            // block's range; both must answer None.
            (0..16).map(|_| rng.next() as u8).collect()
        };
        let want = set.get(&key).cloned();
        let got = reader.get(&key).unwrap().map(|e| e.value);
        assert_eq!(got, want, "restart search mismatch for {key:?}");
    }
}

#[test]
fn block_v2_scan_matches_v1() {
    // The same entry set through both writers: point lookups, scans,
    // version walks and the streaming iterator must agree byte-for-byte —
    // the v2 full-decode path (scans, compaction) serves the same answers
    // as v1, restarts notwithstanding.
    let mut entries = Vec::new();
    for i in 0..100u64 {
        entries.push(entry(
            &format!("k{i:03}"),
            &format!("v{i:03}"),
            1000 + i,
            FLAG_PUT,
        ));
        if i % 10 == 0 {
            entries.push(entry(
                &format!("k{i:03}"),
                &format!("old{i:03}"),
                100 + i,
                FLAG_PUT,
            ));
        }
    }
    entries.push(entry("k050", "", 9999, FLAG_DELETE));
    let mut w1 = SegmentWriter::new(1024);
    let mut w2 = SegmentWriter::new_v2(1024);
    for e in &entries {
        w1.push(e.clone());
        w2.push(e.clone());
    }
    let p1 = tmp("blockv2-scan-v1");
    w1.publish(&p1).unwrap();
    let p2 = tmp("blockv2-scan-v2");
    w2.publish(&p2).unwrap();
    let r1 = SegmentReader::open(&p1).unwrap();
    let r2 = SegmentReader::open(&p2).unwrap();
    assert_eq!(
        r1.scan(b"", b"\xff").unwrap(),
        r2.scan(b"", b"\xff").unwrap(),
        "v1 and v2 scans must be identical"
    );
    for e in &entries {
        assert_eq!(r1.get(&e.key).unwrap(), r2.get(&e.key).unwrap());
        assert_eq!(r1.versions(&e.key).unwrap(), r2.versions(&e.key).unwrap());
    }
    let a: Vec<SegmentEntry> = r1.iter().map(|r| r.unwrap()).collect();
    let b: Vec<SegmentEntry> = r2.iter().map(|r| r.unwrap()).collect();
    assert_eq!(a, b, "v1 and v2 streaming iterators must be identical");
}

#[test]
fn block_v2_future_version_fails_closed() {
    // The M1 v1 golden inputs (segment_golden.rs pins the v1 writer
    // byte-exact) with the data-block version patched 1 → 3: with the block
    // and skeleton checksums recomputed per spec (independently, via c8
    // above), open() must say Unsupported (a newer format, not damage);
    // with the stale checksums it must say Corrupt. Either way it fails
    // closed — a future block is never served or misread.
    let mut w = SegmentWriter::new(4096);
    w.push(entry("a3", "v3", 9, FLAG_DELETE));
    w.push(entry("a1", "v1", 5, FLAG_PUT));
    w.push(entry("a2", "v2", 7, FLAG_VERSION));
    let base = tmp("blockv2-future-base");
    w.publish(&base).unwrap();
    let base = std::fs::read(&base).unwrap();

    // Layout: 54-byte segment header, then the data block (28-byte header,
    // 0x3d-byte payload), index block (0x14 payload), bloom block (8-byte
    // payload), 22-byte footer.
    let payload_len = 0x3dusize;
    let data_off = 54usize;
    let index_off = data_off + 28 + payload_len;
    let bloom_off = index_off + 28 + 0x14;
    let footer_off = bloom_off + 28 + 8;
    assert_eq!(
        base.len(),
        footer_off + 22,
        "v1 golden layout changed — update these offsets"
    );

    // Valid checksums → Unsupported.
    let mut bytes = base.clone();
    bytes[data_off + 4..data_off + 6].copy_from_slice(&3u16.to_le_bytes());
    let mut sk = Vec::with_capacity(20 + payload_len);
    sk.extend_from_slice(&bytes[data_off..data_off + 20]);
    sk.extend_from_slice(&bytes[data_off + 28..data_off + 28 + payload_len]);
    bytes[data_off + 20..data_off + 28].copy_from_slice(&c8(&sk));
    let mut skeleton = Vec::new();
    skeleton.extend_from_slice(&bytes[..data_off]); // segment header
    skeleton.extend_from_slice(&bytes[data_off..data_off + 28]); // data block header
    skeleton.extend_from_slice(&bytes[index_off..footer_off]); // index + bloom whole
    skeleton.extend_from_slice(&bytes[footer_off..footer_off + 14]); // footer fields
    bytes[footer_off + 14..footer_off + 22].copy_from_slice(&c8(&skeleton));
    let p = tmp("blockv2-future");
    std::fs::write(&p, &bytes).unwrap();
    let err = SegmentReader::open(&p).unwrap_err();
    assert!(
        matches!(err, FormatError::Unsupported(_)),
        "valid-checksum future block version must be Unsupported, got {err:?}"
    );

    // Stale checksums → Corrupt.
    let mut bytes = base;
    bytes[data_off + 4..data_off + 6].copy_from_slice(&3u16.to_le_bytes());
    let p = tmp("blockv2-future-corrupt");
    std::fs::write(&p, &bytes).unwrap();
    let err = SegmentReader::open(&p).unwrap_err();
    assert!(
        matches!(err, FormatError::Corrupt(_)),
        "stale-checksum future block version must be Corrupt, got {err:?}"
    );
}

#[test]
fn block_v2_equal_key_run_restarts() {
    // SE2-M14 — the compaction retention shape: V versions of the SAME user
    // key (seq descending) landed in one block. Restart points must skip
    // equal keys — the reader's fail-closed strictly-increasing restart
    // check is what the binary search stands on, and a restart inside the
    // run would hide its higher-seq heads.
    let mut w = SegmentWriter::new_v2(4096);
    for i in 0..20u64 {
        w.push(entry("k", &format!("v{i:02}"), 100 - i, FLAG_PUT));
    }
    let path = tmp("blockv2-equal-keys");
    w.publish(&path).unwrap();
    let reader = SegmentReader::open(&path).unwrap();
    let head = reader.get(b"k").unwrap();
    assert_eq!(
        head.as_ref().map(|e| e.seq),
        Some(100),
        "the highest seq is the head of the run"
    );
    let all = reader.versions(b"k").unwrap();
    assert_eq!(all.len(), 20, "all 20 versions survive");
    assert_eq!(all[0].seq, 100);
    assert_eq!(all[19].seq, 81);
}
