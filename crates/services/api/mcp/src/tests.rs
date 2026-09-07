use crate::{model_store_dir, semantic_status_snapshot, set_semantic_status, validate_listen};

// Temp db paths written by THIS test thread, swept when the thread exits
// (the main thread's destructor runs at process exit — statics are NOT
// dropped on Windows MSVC, TLS is).
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
            // redb sidecar next to the registered stem (`{stem}.redb.artifacts`).
            let Some(name) = p.file_name() else { continue };
            if let Ok(rd) = std::fs::read_dir(p.parent().unwrap_or(std::path::Path::new("."))) {
                let prefix = format!("{}.", name.to_string_lossy());
                for e in rd.flatten() {
                    if e.file_name().to_string_lossy().starts_with(&prefix) {
                        let _ = std::fs::remove_file(e.path());
                        let _ = std::fs::remove_dir_all(e.path());
                    }
                }
            }
        }
    }
}

fn tmp_db(tag: &str) -> String {
    let p = std::env::temp_dir().join(format!("mnemo-{tag}-{}.redb", std::process::id()));
    let _ = std::fs::remove_file(&p);
    TEMP_PATHS.with(|t| t.borrow_mut().paths.push(p.clone()));
    p.to_string_lossy().into_owned()
}
#[test]
fn model_store_dir_flag_wins() {
    let p = model_store_dir(Some("C:/tmp/models"));
    assert_eq!(p, std::path::PathBuf::from("C:/tmp/models"));
}

#[test]
fn model_store_dir_default_ends_in_aikoql_models() {
    let p = model_store_dir(None);
    let mut comps = p.components().rev();
    assert_eq!(
        comps.next().map(|c| c.as_os_str()),
        Some(std::ffi::OsStr::new("models"))
    );
    assert_eq!(
        comps.next().map(|c| c.as_os_str()),
        Some(std::ffi::OsStr::new(".aikoql"))
    );
}

#[test]
fn semantic_status_roundtrip() {
    set_semantic_status("unavailable", "no model installed");
    let s = semantic_status_snapshot();
    assert_eq!(s.state, "unavailable");
    assert_eq!(s.detail, "no model installed");
    set_semantic_status("ready", "live");
    assert_eq!(semantic_status_snapshot().state, "ready");
}

// R1 (review round 3): plaintext TCP is loopback-only — a non-loopback bind
// is rejected fail-closed (the bearer token must not travel unencrypted).

#[test]
fn listen_remote_without_tls_rejected() {
    for bad in ["0.0.0.0:9090", "192.168.1.5:9090"] {
        let err = validate_listen(bad).unwrap_err();
        assert!(
            err.contains("non-loopback"),
            "remote {bad} must be rejected, got: {err}"
        );
    }
}

#[test]
fn listen_loopback_allowed() {
    assert_eq!(validate_listen("127.0.0.1:9090").unwrap(), "127.0.0.1:9090");
    assert_eq!(validate_listen("[::1]:9090").unwrap(), "[::1]:9090");
}

#[test]
fn listen_empty_host_maps_to_loopback() {
    assert_eq!(validate_listen(":9090").unwrap(), "127.0.0.1:9090");
}

#[test]
fn listen_invalid_address_rejected() {
    assert!(validate_listen("not an address").is_err());
}
use crate::http::truncate;

#[test]
fn truncate_never_splits_multibyte_chars() {
    // 25 x 'a' + '—' (bytes 25..28) + 'zzzz' = 32 bytes. max 30 → end 27
    // lands inside the em dash and must back off to a char boundary.
    let s = "aaaaaaaaaaaaaaaaaaaaaaaaa—zzzz";
    let t = truncate(s, 30);
    assert!(t.ends_with("..."));
    assert_eq!(&t[t.len() - 4..], "a...");
}

#[test]
fn truncate_passthrough_short_strings() {
    assert_eq!(truncate("hi", 10), "hi");
}

#[test]
fn enrich_file_contains_adds_file_entities_and_relations() {
    use aikoql_ingestion::{EntityCandidate, Evidence, KnowledgeIr};
    let mut ir = KnowledgeIr {
        entities: vec![
            EntityCandidate {
                name: "graph_api".into(),
                type_hint: Some("Function".into()),
                mentions: vec![],
                confidence: 0.8,
                evidence: Evidence {
                    document_id: Some("src/main.rs".into()),
                    ..Default::default()
                },
            },
            EntityCandidate {
                name: "retry_loop".into(),
                type_hint: Some("Function".into()),
                mentions: vec![],
                confidence: 0.8,
                evidence: Evidence {
                    document_id: Some("src/main.rs".into()),
                    ..Default::default()
                },
            },
            // doc == name fallback path entity: no duplicate File entity,
            // no self-contains relation.
            EntityCandidate {
                name: "src/lib.rs".into(),
                type_hint: Some("file".into()),
                mentions: vec![],
                confidence: 0.8,
                evidence: Evidence {
                    document_id: Some("src/lib.rs".into()),
                    ..Default::default()
                },
            },
        ],
        ..Default::default()
    };
    crate::ingest::enrich_file_contains(&mut ir);
    let files: Vec<&str> = ir
        .entities
        .iter()
        .filter(|e| e.type_hint.as_deref() == Some("file"))
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(files, vec!["src/lib.rs", "src/main.rs"]);
    let contains: Vec<(&str, &str, &str)> = ir
        .relations
        .iter()
        .map(|r| (r.subject.as_str(), r.predicate.as_str(), r.object.as_str()))
        .collect();
    assert_eq!(contains.len(), 2);
    assert!(contains.contains(&("src/main.rs", "contains", "graph_api")));
    assert!(contains.contains(&("src/main.rs", "contains", "retry_loop")));
}

#[test]
fn semantic_scores_parses_caches_and_scores() {
    // Regression check for the EMB_CACHE self-deadlock: the cache-insert
    // branch used to re-lock the mutex it already held via a match
    // scrutinee temporary, wedging the first request (and every request
    // after it) forever. This test walks both branches: parse+insert,
    // then cache-hit.
    let db = tmp_db("sem");
    let _ = std::fs::remove_file(&db);
    let engine = crate::RedbEngine::open(&db).expect("open store");
    let k = crate::Kernel::open(
        std::sync::Arc::new(engine),
        std::sync::Arc::new(crate::SystemClock),
        0,
    )
    .expect("open kernel");

    let mut props = crate::PropertyMap::new();
    props.insert(
        "entity_embeddings".into(),
        crate::Value::Text(r#"{"a::b":[1.0,0.0]}"#.into()),
    );
    let r = k
        .remember(crate::RememberRequest {
            context: crate::KnowledgeContext::from(&crate::Subject::with_roles("test", &["admin"])),
            koid: None,
            expected_version: Some(0),
            idempotency_key: Some("sem-scores-test".into()),
            metadata: crate::Metadata {
                type_name: "aikoql:ingested-directory".into(),
                tenant: None,
                schema_version: 1,
                tags: vec![],
            },
            properties: props,
            semantic: None,
            relationships: vec![],
            security: None,
            extensions: crate::ExtensionMap::new(),
            origin: crate::Origin::Human,
            note: None,
            referential_policy: crate::ReferentialPolicy::Permissive,
        })
        .expect("remember");
    let args = serde_json::json!({"koid": r.koid.to_hex(), "subject": "test", "roles": ["admin"]});

    let scores = crate::tools::semantic_scores(&k, &args, &[1.0, 0.0]).expect("scores");
    assert!((scores["a::b"] - 1.0).abs() < 1e-6);
    let cached = crate::tools::semantic_scores(&k, &args, &[1.0, 0.0]).expect("cached hit");
    assert_eq!(cached.len(), 1);

    drop(k);
    let _ = std::fs::remove_file(&db);
}

#[test]
fn snapshot_manifest_props_carry_source_revision() {
    use crate::ingest::snapshot_manifest_props;
    use crate::Value;
    let with_rev = aikoql_ingestion::KnowledgeIr {
        source_revision: Some("abc123def".into()),
        ..Default::default()
    };
    let props = snapshot_manifest_props(&with_rev, "E:/repo", "{}".into(), "{}".into());
    assert_eq!(
        props.get("source_revision"),
        Some(&Value::Text("abc123def".into())),
        "revision must be a first-class snapshot property"
    );
    assert_eq!(
        props.get("source_path"),
        Some(&Value::Text("E:/repo".into()))
    );
    assert_eq!(props.get("entity_count"), Some(&Value::Int(0)));

    let without_rev = aikoql_ingestion::KnowledgeIr::default();
    let props = snapshot_manifest_props(&without_rev, "E:/repo", "{}".into(), "{}".into());
    assert!(
        !props.contains_key("source_revision"),
        "non-git ingest must omit the revision column"
    );
}

/// PRG-007: execution_id makes execute_program exactly-once — replays with
/// the same id return the stored result without re-running, and the journal
/// (aikoql:execution records) holds exactly one record per id.
#[test]
fn execute_program_idempotency_execution_id_replays() {
    let db = tmp_db("prg7");
    let _ = std::fs::remove_file(&db);
    let k = crate::Kernel::open(
        std::sync::Arc::new(crate::RedbEngine::open(&db).expect("open store")),
        std::sync::Arc::new(crate::SystemClock),
        0,
    )
    .expect("open kernel");
    let subject = crate::Subject::with_roles("test", &["admin"]);

    // Seed: two facts the program can filter on.
    for name in ["Alice", "Bob"] {
        let mut req = crate::RememberRequest::create(
            subject.clone(),
            crate::Metadata {
                type_name: "Doc".into(),
                tenant: None,
                schema_version: 1,
                tags: vec![],
            },
        );
        req.properties
            .insert("name".into(), crate::Value::Text(name.into()));
        k.remember(req).expect("seed fact");
    }

    // Deploy a parameterized program.
    let prog = crate::tools::tool_deploy_program(
        &k,
        &serde_json::json!({
            "subject": "test", "roles": ["admin"],
            "name": "FindDoc",
            "body": "MATCH Doc WHERE name == \"{{who}}\" RETURN *",
            "language": "aikoql"
        }),
    )
    .expect("deploy");
    let prog_koid = prog["koid"].as_str().unwrap().to_string();

    // First execution with an execution_id.
    let exec1 = crate::tools::tool_execute_program(
        &k,
        &serde_json::json!({
            "subject": "test", "roles": ["admin"],
            "koid": &prog_koid, "params": {"who": "Alice"}, "execution_id": "exec-1"
        }),
    )
    .expect("exec1");
    assert_eq!(exec1["count"], 1);
    assert_eq!(exec1["results"][0]["properties"]["name"], "Alice");

    // Same execution_id, different params: replay — must return the stored
    // result of the first run, proving the program was not re-run.
    let replay = crate::tools::tool_execute_program(
        &k,
        &serde_json::json!({
            "subject": "test", "roles": ["admin"],
            "koid": &prog_koid, "params": {"who": "Bob"}, "execution_id": "exec-1"
        }),
    )
    .expect("replay");
    assert_eq!(replay, exec1);

    // A new execution_id runs again.
    let exec2 = crate::tools::tool_execute_program(
        &k,
        &serde_json::json!({
            "subject": "test", "roles": ["admin"],
            "koid": &prog_koid, "params": {"who": "Bob"}, "execution_id": "exec-2"
        }),
    )
    .expect("exec2");
    assert_eq!(exec2["count"], 1);
    assert_eq!(exec2["results"][0]["properties"]["name"], "Bob");

    // Journal: exactly one record per id, carrying the FIRST run's params —
    // the replay did not overwrite it (the write committed exactly once).
    let (rec1_koid, _, _) = k
        .resolve_idempotency(&format!("execute-program-{prog_koid}-exec-1"))
        .expect("resolve")
        .expect("exec-1 record");
    let (rec2_koid, _, _) = k
        .resolve_idempotency(&format!("execute-program-{prog_koid}-exec-2"))
        .expect("resolve")
        .expect("exec-2 record");
    assert_ne!(rec1_koid, rec2_koid);
    let rec1 = k
        .get(crate::KnowledgeContext::from(&subject), &rec1_koid)
        .expect("get exec-1 record");
    assert_eq!(rec1.metadata.type_name, "aikoql:execution");
    assert_eq!(
        rec1.properties.get("program"),
        Some(&crate::Value::Text(prog_koid.clone()))
    );
    assert!(matches!(
        rec1.properties.get("params"),
        Some(crate::Value::Text(s)) if s.contains("\"Alice\"")
    ));

    drop(k);
    let _ = std::fs::remove_file(&db);
}
