//! SE2-M2 — durability modes (design §7): Sync is the default and no mode
//! may silently downgrade. GroupCommit/Async take explicit opt-in and skip
//! the per-batch fsync; the group-commit machinery landed in SE2-M6
//! (committer thread, one fsync per group, apply-before-ack).

mod common;

use aikoql_storage_v2::db::{Config, Db, DurabilityMode};
use common::dir;

#[test]
fn default_durability_is_sync() {
    assert_eq!(DurabilityMode::default(), DurabilityMode::Sync);
    let cfg = Config::new(dir("modes-default"));
    assert_eq!(cfg.durability, DurabilityMode::Sync);
    let cfg2 = Config {
        ..Config::new(dir("modes-default-2"))
    };
    assert_eq!(
        cfg2.durability,
        DurabilityMode::Sync,
        "struct-update construction must not change the default"
    );
}

#[test]
fn explicit_weaker_modes_write_and_read() {
    for mode in [DurabilityMode::GroupCommit, DurabilityMode::Async] {
        let d = dir(&format!("modes-{mode:?}"));
        let mut cfg = Config::new(d.clone());
        cfg.durability = mode; // explicit opt-in — never silent
        let db = Db::open(cfg).unwrap();
        db.put(b"k1", b"v1").unwrap();
        db.put(b"k2", b"v2").unwrap();
        assert_eq!(db.get(b"k1").unwrap(), Some(b"v1".to_vec()));
        assert_eq!(db.get(b"k2").unwrap(), Some(b"v2".to_vec()));
    }
}
