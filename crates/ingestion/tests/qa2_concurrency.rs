//! MVP-QA-002 Suite A — QA2-CONC-001 (four reader shapes under write load,
//! incl. context compilation) and QA2-CONC-002b (concurrent same-dataset
//! ingestion yields identical IR — no dup explosion, deterministic state).

use aikoql_ingestion::{
    compile_context, compile_markdown_string, ingest_directory, parallel_ingest_directory,
    render_context_markdown, KnowledgeIr,
};
use aikoql_kernel::*;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

// Temp paths created by THIS thread, swept when the thread exits (the main
// thread's destructor runs at process exit — statics are NOT dropped on
// Windows MSVC, TLS is). Kill-harness children never register a path they
// received via env, so the parent's evidence survives its child.
thread_local! {
    static TEMP_PATHS: std::cell::RefCell<TempSweeper> =
        const { std::cell::RefCell::new(TempSweeper { paths: Vec::new() }) };
}

struct TempSweeper {
    paths: Vec<std::path::PathBuf>,
}
impl Drop for TempSweeper {
    fn drop(&mut self) {
        for p in &self.paths {
            let _ = std::fs::remove_file(p);
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

fn mk() -> Kernel {
    Kernel::open(
        Arc::new(MemoryEngine::new()),
        Arc::new(ManualClock::new(10_000)),
        0xC0FFEE,
    )
    .unwrap()
}

fn meta(t: &str) -> Metadata {
    Metadata {
        type_name: t.into(),
        tenant: None,
        schema_version: 1,
        tags: vec![],
    }
}

fn alice() -> Subject {
    Subject::new("alice")
}

fn tmp_corpus(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aikoql_qa2_{}_{}_{}",
        name,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    TEMP_PATHS.with(|t| t.borrow_mut().paths.push(p.clone()));
    p
}

// ---------------------------------------------------------------------------
// QA2-CONC-001 — every reader shape sees only committed state
// ---------------------------------------------------------------------------

#[test]
fn w2_conc_001_all_four_reader_shapes_see_only_committed_state() {
    let k = Arc::new(mk());

    // Seed graph: hot (mutated) + h1 -> h2 -> h3 chain (traversal target)
    // + snapshot (the versioned ir_json KO the context compiler reads).
    let mut hot_req = RememberRequest::create(alice(), meta("fact"));
    hot_req.properties.insert("n".into(), Value::Int(-1));
    let hot = k.remember(hot_req).unwrap().koid;
    let h1 = k
        .remember(RememberRequest::create(alice(), meta("fact")))
        .unwrap()
        .koid;
    let h2 = k
        .remember(RememberRequest::create(alice(), meta("fact")))
        .unwrap()
        .koid;
    let h3 = k
        .remember(RememberRequest::create(alice(), meta("fact")))
        .unwrap()
        .koid;
    let mut chain = RememberRequest::update(alice(), h1, meta("fact"));
    chain.relationships.push(RelationshipRef {
        rel_type: "next".into(),
        target: h2,
        direction: Direction::Outbound,
    });
    k.remember(chain).unwrap();
    let mut chain = RememberRequest::update(alice(), h2, meta("fact"));
    chain.relationships.push(RelationshipRef {
        rel_type: "next".into(),
        target: h3,
        direction: Direction::Outbound,
    });
    k.remember(chain).unwrap();

    // The context compiler's input: a markdown corpus compiled to IR and
    // stored as the versioned ir_json snapshot (the MCP ingest-dir flow).
    let md = "# Kernel\n\nThe kernel commits knowledge objects with MVCC isolation.\n\n## Rules\n\n- must validate constraints at commit time\n";
    let ir = compile_markdown_string(md, Some("qa2-conc-001.md".into())).unwrap();
    let ir_json = serde_json::to_string(&ir).unwrap();
    let expected_render = {
        let pkg = compile_context("kernel commits knowledge objects", &ir, 400);
        let s = render_context_markdown(&pkg);
        assert!(
            !s.is_empty(),
            "expected pack must be non-empty for a matching task"
        );
        s
    };
    let mut snap_req = RememberRequest::create(alice(), meta("snapshot"));
    snap_req
        .properties
        .insert("ir_json".into(), Value::Text(ir_json.clone()));
    snap_req.properties.insert("n".into(), Value::Int(0));
    let snap = k.remember(snap_req).unwrap().koid;

    // Writer storm: two threads hammer the hot KO, one keeps bumping the
    // snapshot KO's version while restating the same ir_json.
    let mut handles = Vec::new();
    for t in 0..2u64 {
        let k = k.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..30u64 {
                let mut req = RememberRequest::update(alice(), hot, meta("fact"));
                req.properties
                    .insert("n".into(), Value::Int((t * 1000 + i) as i64));
                k.remember(req).unwrap();
            }
        }));
    }
    {
        let k = k.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..20u64 {
                loop {
                    let head = k.get(alice(), &snap).unwrap();
                    let mut req = RememberRequest::update(alice(), snap, meta("snapshot"));
                    req.expected_version = Some(head.version);
                    req.properties
                        .insert("ir_json".into(), Value::Text(ir_json.clone()));
                    req.properties.insert("n".into(), Value::Int(i as i64));
                    match k.remember(req) {
                        Ok(_) => break,
                        Err(KError::VersionConflict { .. }) => continue,
                        Err(e) => panic!("snapshot writer: {e}"),
                    }
                }
            }
        }));
    }

    // Reader A — current query.
    {
        let k = k.clone();
        handles.push(std::thread::spawn(move || {
            let mut last_v = 0u64;
            for _ in 0..150 {
                let ko = k.get(alice(), &hot).unwrap();
                assert!(ko.version >= last_v, "reader A: version regressed");
                last_v = ko.version;
                match ko.properties.get("n") {
                    Some(Value::Int(n)) => assert!(
                        *n == -1 || (0..2000i64).contains(n),
                        "reader A: torn value {n}"
                    ),
                    other => panic!("reader A: hot KO lost n: {other:?}"),
                }
            }
        }));
    }
    // Reader B — graph traversal (walks the h1 -> h2 -> h3 chain).
    {
        let k = k.clone();
        handles.push(std::thread::spawn(move || {
            let mut last = [0u64; 3];
            for _ in 0..150 {
                for (i, id) in [h1, h2, h3].iter().enumerate() {
                    let ko = k.get(alice(), id).unwrap();
                    assert!(
                        ko.version >= last[i],
                        "reader B: version regressed on hop {i}"
                    );
                    last[i] = ko.version;
                    if *id != h3 {
                        assert_eq!(
                            ko.relationships.len(),
                            1,
                            "reader B: torn relation list on hop {i}"
                        );
                    }
                }
            }
        }));
    }
    // Reader C — historical query (lineage of the hot KO).
    {
        let k = k.clone();
        handles.push(std::thread::spawn(move || {
            let mut last_len = 0usize;
            for _ in 0..150 {
                let lin = k.trace(alice(), &hot).unwrap();
                assert!(lin.versions.len() >= last_len, "reader C: history shrank");
                last_len = lin.versions.len();
                // Within one snapshot, history must be commit-ordered and
                // version-ordered (concurrent snapshots may overlap, so
                // order is only asserted inside a single trace call).
                let mut last_ts = 0u64;
                let mut last_v = 0u64;
                for v in &lin.versions {
                    assert!(
                        v.commit_ts >= last_ts,
                        "reader C: history out of commit order"
                    );
                    assert!(v.version > last_v, "reader C: history out of version order");
                    last_ts = v.commit_ts;
                    last_v = v.version;
                }
            }
        }));
    }
    // Reader D — context compilation: read the versioned snapshot, parse the
    // IR, compile — the package must be byte-identical every time and the
    // ir_json must never tear while the version advances.
    {
        let k = k.clone();
        handles.push(std::thread::spawn(move || {
            let mut last_v = 0u64;
            for _ in 0..100 {
                let ko = k.get(alice(), &snap).unwrap();
                assert!(ko.version >= last_v, "reader D: version regressed");
                last_v = ko.version;
                let json = match ko.properties.get("ir_json") {
                    Some(Value::Text(s)) => s,
                    other => panic!("reader D: torn ir_json: {other:?}"),
                };
                let parsed: KnowledgeIr =
                    serde_json::from_str(json).expect("reader D: ir_json must deserialize");
                let pkg = compile_context("kernel commits knowledge objects", &parsed, 400);
                assert_eq!(
                    render_context_markdown(&pkg),
                    expected_render,
                    "reader D: context package must be deterministic"
                );
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    // Every write landed exactly once, gapless: 5 creates + 2 chain
    // updates + 2*30 hot updates + 20 snapshot restates.
    let journal = k.journal().unwrap();
    assert_eq!(journal.len(), 5 + 2 + 2 * 30 + 20);
    for (i, ke) in journal.iter().enumerate() {
        assert_eq!(ke.seq, (i + 1) as u64);
    }
}

// ---------------------------------------------------------------------------
// QA2-CONC-002b — concurrent same-dataset ingestion yields identical IR
// ---------------------------------------------------------------------------

#[test]
fn w2_conc_002b_concurrent_ingest_same_dataset_identical_ir() {
    let root = tmp_corpus("conc002b");
    std::fs::write(
        root.join("a.md"),
        "# Kernel\n\nThe kernel commits knowledge objects with MVCC isolation.\n\n## Rules\n\n- must use MVCC for all writes\n- must validate constraints at commit time\n",
    )
    .unwrap();
    std::fs::write(
        root.join("b.rs"),
        "//! aikoql kernel\n\n/// The transaction engine handles all writes.\npub struct Kernel {}\n",
    )
    .unwrap();
    let root_s = root.to_string_lossy().to_string();

    let mut handles = Vec::new();
    for _ in 0..4 {
        let root_s = root_s.clone();
        handles.push(std::thread::spawn(move || {
            let res = parallel_ingest_directory(&root_s).expect("parallel ingest");
            serde_json::to_string(&res.ir).expect("serialize")
        }));
    }
    let mut json_set: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let sequential =
        serde_json::to_string(&ingest_directory(&root_s).expect("sequential ingest").ir)
            .expect("serialize");
    json_set.push(sequential);

    // All runs — parallel and sequential — extract the byte-identical IR:
    // ingestion is deterministic regardless of scheduling (W2-11).
    for w in json_set.windows(2) {
        assert_eq!(w[0], w[1], "concurrent ingestion must produce identical IR");
    }
    assert!(
        json_set[0].contains("MVCC"),
        "the corpus must actually extract facts (sanity)"
    );

    let _ = std::fs::remove_dir_all(&root);
}
