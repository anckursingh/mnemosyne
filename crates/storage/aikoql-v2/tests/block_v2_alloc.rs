//! SE2-M9 — allocation regression for v2 point lookups (the QA TC-PERF-0203
//! shape): a warm lookup on a 10 000-entry block must cost a small constant
//! number of allocations — the winner's key + value clones, the decode
//! scratch, the cached-payload Arc — never O(block). One test in its own
//! binary: the global-allocator counter is process-wide, and a lone test
//! means no sibling test thread skews the delta.

mod common;

use aikoql_storage_v2::db::{Config, Db};
use common::dir;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static A: Counting = Counting;

#[test]
fn block_v2_allocation_regression() {
    const N: usize = 10_000;
    let mut cfg = Config::new(dir("blockv2-alloc"));
    cfg.memtable_bytes = usize::MAX;
    cfg.block_target = 1 << 20; // one block for the whole dataset
    let db = Db::open(cfg).unwrap();
    let keys: Vec<Vec<u8>> = (0..N).map(|i| format!("key-{i:06}").into_bytes()).collect();
    for k in &keys {
        db.put(k, &[b'v'; 16][..]).unwrap();
    }
    db.flush().unwrap();

    // Warmup: the first lookup pays the block read + cache insert.
    for _ in 0..3 {
        assert_eq!(
            db.get(&keys[N - 1]).unwrap().as_deref(),
            Some(&[b'v'; 16][..])
        );
    }

    // Warm steady state: cache hit, bounded decode, winner clone.
    ALLOCS.store(0, Ordering::Relaxed);
    for k in &keys {
        assert_eq!(db.get(k).unwrap().as_deref(), Some(&[b'v'; 16][..]));
    }
    let per = ALLOCS.load(Ordering::Relaxed) / N;
    assert!(
        per <= 5,
        "{per} allocations per warm v2 lookup — the budget is 1 memtable \
         probe bound (SE2-M10) + restart-keys vec + scratch + winner key + \
         winner value; the decode must be borrowed, not owned"
    );
}
