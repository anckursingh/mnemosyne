//! SE2-M1 — range/prefix scans byte-exact (TESTING-PLAN-V2 row V2-M1).
//!
//! Scan semantics: keys in [start, end) byte order; within a key, versions
//! in seq-descending order. The fixture has a multi-version key ("a/1") and
//! keys straddling block boundaries are covered by the lookup suite.

mod common;

use aikoql_storage_v2::segment::{SegmentReader, SegmentWriter, FLAG_PUT, FLAG_VERSION};
use common::{entry, tmp};

fn build(path: &std::path::Path) {
    let mut w = SegmentWriter::new(4096);
    // Pushed unsorted on purpose.
    w.push(entry("b/1", "bv1", 1, FLAG_PUT));
    w.push(entry("a/1", "av1", 1, FLAG_PUT));
    w.push(entry("a/2", "av2", 2, FLAG_PUT));
    w.push(entry("c/1", "cv1", 1, FLAG_PUT));
    w.push(entry("a/1", "av3", 3, FLAG_VERSION)); // second version of a/1
    w.push(entry("b/2", "bv2", 2, FLAG_PUT));
    w.publish(path).unwrap();
}

/// The fixture in the canonical on-disk order (key asc, seq desc).
fn sorted_fixture() -> Vec<(String, String, u64, u8)> {
    let all = vec![
        ("a/1", "av3", 3u64, FLAG_VERSION),
        ("a/1", "av1", 1, FLAG_PUT),
        ("a/2", "av2", 2, FLAG_PUT),
        ("b/1", "bv1", 1, FLAG_PUT),
        ("b/2", "bv2", 2, FLAG_PUT),
        ("c/1", "cv1", 1, FLAG_PUT),
    ];
    all.into_iter()
        .map(|(k, v, s, f)| (k.to_string(), v.to_string(), s, f))
        .collect()
}

fn entries(rows: &[(String, String, u64, u8)]) -> Vec<aikoql_storage_v2::segment::SegmentEntry> {
    rows.iter()
        .map(|(k, v, s, f)| entry(k, v, *s, *f))
        .collect()
}

#[test]
fn prefix_scan_byte_exact() {
    let path = tmp("segment-scan-prefix");
    build(&path);
    let r = SegmentReader::open(&path).unwrap();

    // All "a/…" keys, versions seq-desc within the key.
    let got = r.scan(b"a", b"b").unwrap();
    assert_eq!(got, entries(&sorted_fixture()[..3]));
}

#[test]
fn range_scan_boundaries() {
    let path = tmp("segment-scan-range");
    build(&path);
    let r = SegmentReader::open(&path).unwrap();

    // start inclusive, end exclusive — byte comparisons.
    assert_eq!(
        r.scan(b"a/2", b"b/2").unwrap(),
        entries(&sorted_fixture()[2..4])
    );
    assert_eq!(r.scan(b"c", b"d").unwrap(), entries(&sorted_fixture()[5..]));
    assert_eq!(r.scan(b"x", b"y").unwrap(), vec![]);
}

#[test]
fn full_scan_is_sorted() {
    let path = tmp("segment-scan-full");
    build(&path);
    let r = SegmentReader::open(&path).unwrap();

    let got = r.scan(b"", b"~").unwrap();
    assert_eq!(got, entries(&sorted_fixture()));
}

#[test]
fn multi_version_key_head_and_versions() {
    let path = tmp("segment-scan-versions");
    build(&path);
    let r = SegmentReader::open(&path).unwrap();

    assert_eq!(
        r.get(b"a/1").unwrap(),
        Some(entry("a/1", "av3", 3, FLAG_VERSION))
    );
    let got = r.versions(b"a/1").unwrap();
    assert_eq!(got, entries(&sorted_fixture()[..2]));
}
