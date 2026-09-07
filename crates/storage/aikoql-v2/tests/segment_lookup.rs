//! SE2-M1 — random lookup + versions (TESTING-PLAN-V2 row V2-M1).
//!
//! 240 entries with a 256-byte block target → ~30 blocks, so the index
//! (per-block first/last key) and cross-block version scans are exercised
//! for real, not in the single-block degenerate case.

mod common;

use aikoql_storage_v2::segment::{SegmentReader, SegmentWriter, FLAG_PUT, FLAG_VERSION};
use common::{entry, tmp};

const TARGET: usize = 256;
const BASE: usize = 200;
const EXTRA: usize = 40; // keys k000..k039 get a second, higher-seq version

fn build(path: &std::path::Path) {
    let mut w = SegmentWriter::new(TARGET);
    // Push the extra versions first — publish must sort, not assume order.
    for i in 0..EXTRA {
        w.push(entry(
            &format!("k{i:03}"),
            &format!("w{i:03}"),
            100_000 + i as u64,
            FLAG_VERSION,
        ));
    }
    for i in 0..BASE {
        w.push(entry(
            &format!("k{i:03}"),
            &format!("v{i:03}"),
            i as u64,
            FLAG_PUT,
        ));
    }
    w.publish(path).unwrap();
}

#[test]
fn lookup_returns_exact_head() {
    let path = tmp("segment-lookup");
    build(&path);
    let r = SegmentReader::open(&path).unwrap();
    assert!(r.block_count() > 10, "fixture must span many blocks");

    for i in 0..BASE {
        let key = format!("k{i:03}");
        let got = r.get(key.as_bytes()).unwrap().expect("known key");
        if i < EXTRA {
            assert_eq!(
                got,
                entry(&key, &format!("w{i:03}"), 100_000 + i as u64, FLAG_VERSION)
            );
        } else {
            assert_eq!(got, entry(&key, &format!("v{i:03}"), i as u64, FLAG_PUT));
        }
    }
}

#[test]
fn versions_are_seq_descending() {
    let path = tmp("segment-versions");
    build(&path);
    let r = SegmentReader::open(&path).unwrap();

    let got = r.versions(b"k000").unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0], entry("k000", "w000", 100_000, FLAG_VERSION));
    assert_eq!(got[1], entry("k000", "v000", 0, FLAG_PUT));
    assert!(got[0].seq > got[1].seq);

    // Single-version key.
    assert_eq!(
        r.versions(b"k150").unwrap(),
        vec![entry("k150", "v150", 150, FLAG_PUT)]
    );
}

#[test]
fn missing_keys_return_none() {
    let path = tmp("segment-missing");
    build(&path);
    let r = SegmentReader::open(&path).unwrap();

    assert_eq!(r.get(b"zzz").unwrap(), None);
    assert_eq!(r.get(b"k999").unwrap(), None);
    assert_eq!(r.get(b"").unwrap(), None);
    assert!(r.versions(b"zzz").unwrap().is_empty());
}

#[test]
fn bloom_never_misses_a_known_key() {
    let path = tmp("segment-bloom");
    build(&path);
    let r = SegmentReader::open(&path).unwrap();

    for i in 0..BASE {
        assert!(r.bloom_may_contain(format!("k{i:03}").as_bytes()));
    }
}
