//! SE2-M2 — the one-writer policy (design §19): an OS lock on the database
//! directory; a second open fails closed (FormatError::Locked).

mod common;

use aikoql_storage_v2::db::{Config, Db};
use aikoql_storage_v2::format::FormatError;
use common::dir;

#[test]
fn second_open_fails_closed() {
    let d = dir("lock");
    let _first = Db::open(Config::new(d.clone())).unwrap();
    let err = match Db::open(Config::new(d.clone())) {
        Ok(_) => panic!("second open must fail closed"),
        Err(e) => e,
    };
    assert!(
        matches!(err, FormatError::Locked(_)),
        "expected Locked, got {err}"
    );
}

#[test]
fn lock_releases_on_drop() {
    let d = dir("lock-release");
    {
        let _first = Db::open(Config::new(d.clone())).unwrap();
    }
    Db::open(Config::new(d.clone())).unwrap(); // must succeed after release
}
