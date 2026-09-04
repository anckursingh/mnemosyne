//! Shared live-connector harness (MVP-QA-001 Suite D/E, GATE-04).
#![allow(dead_code)] // harness pieces are consumed as TDD items 2..13 land
//!
//! Env-gate convention (same as real_model_bench.rs): each `Live::*`
//! constructor returns `None` with a `[SKIP]` notice when its `AIKOQL_TEST_*`
//! variable is unset; when the variable IS set the constructor probes the
//! live database and panics on failure — env set means the operator wants
//! the live test, and a dead database must fail loudly, never silently skip.
//!
//! ```text
//! docker compose --profile full up -d
//! $env:AIKOQL_TEST_PG_DSN      = "host=localhost port=5433 user=aikoql password=aikoql-dev-only dbname=knowledge"
//! $env:AIKOQL_TEST_PGVECTOR_DSN = $env:AIKOQL_TEST_PG_DSN
//! $env:AIKOQL_TEST_MONGO_URI   = "mongodb://localhost:27017"
//! $env:AIKOQL_TEST_MONGO_DB    = "knowledge"
//! $env:AIKOQL_TEST_NEO4J_URI   = "http://localhost:7474"
//! $env:AIKOQL_TEST_NEO4J_USER  = "neo4j"
//! $env:AIKOQL_TEST_NEO4J_PASSWORD = "password-dev-only"
//! cargo test -p aikoql-mcp --test connector_certification -- --nocapture
//! ```

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// One live source, probed at construction (connect + trivial query).
#[derive(Debug, Clone)]
pub struct Live {
    /// Source label used in messages: "pg", "pgvector", "mongo", "neo4j".
    pub kind: &'static str,
    /// PostgreSQL/PGVector conn string (`host=... user=... dbname=...`).
    pub dsn: String,
    /// MongoDB URI.
    pub mongo_uri: String,
    /// MongoDB database name.
    pub mongo_db: String,
    /// Neo4j base URL.
    pub neo4j_uri: String,
    pub neo4j_user: String,
    pub neo4j_password: String,
}

impl Live {
    fn env_opt(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.trim().is_empty())
    }

    fn skip(kind: &str, vars: &[&str]) -> Option<Live> {
        eprintln!("[SKIP] no live {kind} — unset: {}", vars.join(", "));
        None
    }

    pub fn pg() -> Option<Live> {
        let dsn = match Self::env_opt("AIKOQL_TEST_PG_DSN") {
            Some(d) => d,
            None => return Self::skip("postgres", &["AIKOQL_TEST_PG_DSN"]),
        };
        let mut conn = aikoql_postgres::PostgresConnector::connect(&dsn)
            .unwrap_or_else(|e| panic!("live pg probe failed: {e}"));
        conn.list_tables()
            .unwrap_or_else(|e| panic!("live pg probe failed: {e}"));
        Some(Live {
            kind: "pg",
            dsn,
            ..Live::blank()
        })
    }

    pub fn pgvector() -> Option<Live> {
        let dsn = match Self::env_opt("AIKOQL_TEST_PGVECTOR_DSN") {
            Some(d) => d,
            None => return Self::skip("pgvector", &["AIKOQL_TEST_PGVECTOR_DSN"]),
        };
        let mut conn = aikoql_postgres::PostgresConnector::connect(&dsn)
            .unwrap_or_else(|e| panic!("live pgvector probe failed: {e}"));
        conn.list_tables()
            .unwrap_or_else(|e| panic!("live pgvector probe failed: {e}"));
        Some(Live {
            kind: "pgvector",
            dsn,
            ..Live::blank()
        })
    }

    pub fn mongo() -> Option<Live> {
        let (uri, db) = match (
            Self::env_opt("AIKOQL_TEST_MONGO_URI"),
            Self::env_opt("AIKOQL_TEST_MONGO_DB"),
        ) {
            (Some(u), Some(d)) => (u, d),
            _ => {
                return Self::skip(
                    "mongodb",
                    &["AIKOQL_TEST_MONGO_URI", "AIKOQL_TEST_MONGO_DB"],
                )
            }
        };
        let conn = aikoql_mongodb::MongoConnector::connect(&uri, &db)
            .unwrap_or_else(|e| panic!("live mongo probe failed: {e}"));
        conn.list_collections()
            .unwrap_or_else(|e| panic!("live mongo probe failed: {e}"));
        Some(Live {
            kind: "mongo",
            mongo_uri: uri,
            mongo_db: db,
            ..Live::blank()
        })
    }

    pub fn neo4j() -> Option<Live> {
        let uri = match Self::env_opt("AIKOQL_TEST_NEO4J_URI") {
            Some(u) => u,
            None => {
                return Self::skip(
                    "neo4j",
                    &[
                        "AIKOQL_TEST_NEO4J_URI",
                        "AIKOQL_TEST_NEO4J_USER",
                        "AIKOQL_TEST_NEO4J_PASSWORD",
                    ],
                )
            }
        };
        let user = Self::env_opt("AIKOQL_TEST_NEO4J_USER").unwrap_or_else(|| "neo4j".into());
        let password =
            Self::env_opt("AIKOQL_TEST_NEO4J_PASSWORD").unwrap_or_else(|| "password".into());
        let conn = aikoql_neo4j::Neo4jConnector::connect(&uri, &user, &password)
            .unwrap_or_else(|e| panic!("live neo4j probe failed: {e}"));
        conn.list_labels()
            .unwrap_or_else(|e| panic!("live neo4j probe failed: {e}"));
        Some(Live {
            kind: "neo4j",
            neo4j_uri: uri,
            neo4j_user: user,
            neo4j_password: password,
            ..Live::blank()
        })
    }

    fn blank() -> Live {
        Live {
            kind: "",
            dsn: String::new(),
            mongo_uri: String::new(),
            mongo_db: String::new(),
            neo4j_uri: String::new(),
            neo4j_user: String::new(),
            neo4j_password: String::new(),
        }
    }
}

// Temp db paths written by THIS test thread, swept when the thread exits
// (the main thread's destructor runs at process exit — statics are NOT
// dropped on Windows MSVC, TLS is). The 1,587-file mcp-* pile in TEMP is
// what this sweeps: every helper below registers before returning.
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

/// Fresh temp db path for this process (deleted if it exists), same pattern
/// as mcp_real_world.rs.
pub fn temp_db(suffix: &str) -> String {
    let path = std::env::temp_dir().join(format!("mcp-{suffix}-{}.redb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    TEMP_PATHS.with(|t| t.borrow_mut().paths.push(path.clone()));
    path.to_string_lossy().into_owned()
}

/// Freshest-built aikoql-mcp binary (same resolution as McpClient::start in
/// mcp_real_world.rs — a stale release binary would silently test old code).
pub fn binary_path() -> PathBuf {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let exe = if cfg!(windows) {
        "aikoql-mcp.exe"
    } else {
        "aikoql-mcp"
    };
    let release_bin = workspace_root.join("target/release").join(exe);
    let debug_bin = workspace_root.join("target/debug").join(exe);
    let newest = |a: &std::path::Path, b: &std::path::Path| -> bool {
        let m = |p: &std::path::Path| {
            p.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH)
        };
        m(a) >= m(b)
    };
    let bin = match (release_bin.exists(), debug_bin.exists()) {
        (true, true) if newest(&debug_bin, &release_bin) => debug_bin,
        (true, false) => release_bin,
        _ => debug_bin,
    };
    assert!(
        bin.exists(),
        "aikoql-mcp binary not built: {}",
        bin.display()
    );
    eprintln!("Using binary: {}", bin.display());
    bin
}

pub struct ImportOut {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// Run `aikoql-mcp <args...>` (args start with the subcommand, e.g.
/// `["import","postgres",conn_str,db]`); captures stdout+stderr.
pub fn run_import(args: &[&str]) -> ImportOut {
    let bin = binary_path();
    let out = Command::new(&bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()));
    ImportOut {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Panic with the captured output — makes red-test failures self-explanatory.
pub fn assert_import_ok(out: &ImportOut, what: &str) {
    assert!(
        out.status.success(),
        "import {what} failed ({}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status,
        out.stdout,
        out.stderr
    );
}

/// The failure twin: the import must NOT succeed (outage guards).
pub fn assert_import_fails(out: &ImportOut, what: &str) {
    assert!(
        !out.status.success(),
        "import {what} unexpectedly succeeded:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.stdout,
        out.stderr
    );
}

// ---------------------------------------------------------------------------
// Seeding + state reads (TDD items 2..13)
// ---------------------------------------------------------------------------

/// Run SQL against the live PostgreSQL (DDL/seed/update) through the same
/// `postgres` driver the provider uses — the connector's own client is
/// private, and a test backdoor in the prod API would be worse than a second
/// connection here.
pub fn pg_exec(dsn: &str, stmts: &[&str]) {
    // postgres::Client::connect is sync (runs its own internal runtime) —
    // wrapping it in block_on panics with "runtime from within a runtime".
    let mut client = postgres::Client::connect(dsn, postgres::NoTls)
        .unwrap_or_else(|e| panic!("pg seed connect failed: {e}"));
    for s in stmts {
        client
            .batch_execute(s)
            .unwrap_or_else(|e| panic!("pg seed failed ({s}): {e}"));
    }
}

// ---------------------------------------------------------------------------
// MongoDB seeding (TDD item 5)
// ---------------------------------------------------------------------------

fn mongo_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap_or_else(|e| panic!("mongo tokio runtime: {e}"))
}

/// Drop + seed one collection. The drop clears leftovers from earlier runs —
/// the live mongo database is shared state across test invocations, unlike
/// the per-run redb files.
pub fn mongo_seed(uri: &str, db: &str, coll: &str, docs: Vec<mongodb::bson::Document>) {
    let rt = mongo_rt();
    rt.block_on(async {
        let client = mongodb::Client::with_uri_str(uri)
            .await
            .unwrap_or_else(|e| panic!("mongo seed connect: {e}"));
        let c = client
            .database(db)
            .collection::<mongodb::bson::Document>(coll);
        c.drop()
            .await
            .unwrap_or_else(|e| panic!("mongo drop {coll}: {e}"));
        c.insert_many(docs)
            .await
            .unwrap_or_else(|e| panic!("mongo seed {coll}: {e}"));
    });
}

pub fn mongo_update(
    uri: &str,
    db: &str,
    coll: &str,
    filter: mongodb::bson::Document,
    update: mongodb::bson::Document,
) {
    let rt = mongo_rt();
    rt.block_on(async {
        let client = mongodb::Client::with_uri_str(uri)
            .await
            .unwrap_or_else(|e| panic!("mongo update connect: {e}"));
        client
            .database(db)
            .collection::<mongodb::bson::Document>(coll)
            .update_one(filter, update)
            .await
            .unwrap_or_else(|e| panic!("mongo update {coll}: {e}"));
    });
}

pub fn mongo_delete(uri: &str, db: &str, coll: &str, filter: mongodb::bson::Document) {
    let rt = mongo_rt();
    rt.block_on(async {
        let client = mongodb::Client::with_uri_str(uri)
            .await
            .unwrap_or_else(|e| panic!("mongo delete connect: {e}"));
        client
            .database(db)
            .collection::<mongodb::bson::Document>(coll)
            .delete_one(filter)
            .await
            .unwrap_or_else(|e| panic!("mongo delete {coll}: {e}"));
    });
}

// ---------------------------------------------------------------------------
// Neo4j seeding (TDD item 6)
// ---------------------------------------------------------------------------

/// Run Cypher statements against the live Neo4j through the same HTTP JSON
/// API the provider uses (ureq). One transaction per call, statements in
/// order. Per-test label/rel-type names are mandatory — the live graph is
/// shared state across parallel test fns, like the mongo database.
pub fn neo4j_exec(uri: &str, user: &str, pass: &str, stmts: &[&str]) {
    let auth = format!(
        "Basic {}",
        aikoql_neo4j::base64_encode(&format!("{user}:{pass}"))
    );
    let body = serde_json::json!({
        "statements": stmts
            .iter()
            .map(|s| serde_json::json!({ "statement": s }))
            .collect::<Vec<_>>()
    });
    let resp = ureq::post(&format!("{}/db/neo4j/tx/commit", uri.trim_end_matches('/')))
        .set("Authorization", &auth)
        .set("Content-Type", "application/json")
        .send_json(&body)
        .unwrap_or_else(|e| panic!("neo4j seed request: {e}"));
    let neo: serde_json::Value = resp
        .into_json()
        .unwrap_or_else(|e| panic!("neo4j seed response: {e}"));
    let errors = neo["errors"].as_array().map(Vec::len).unwrap_or(0);
    assert_eq!(errors, 0, "neo4j seed failed: {}", neo);
}

/// Create a throwaway database for this test and return the dsn pointed at
/// it. Parallel test fns share one live server, so any filterless import
/// (FK item, E2E items) gets its own database instead of racing the other
/// fns' DROP/CREATE on shared tables.
pub fn pg_private_db(dsn: &str, name: &str) -> String {
    pg_exec(
        dsn,
        &[
            &format!("DROP DATABASE IF EXISTS {name}"),
            &format!("CREATE DATABASE {name}"),
        ],
    );
    dsn_with_dbname(dsn, name)
}

/// ponytail: the DSN format is ours (space-separated key=value, env-var
/// controlled) — swap the dbname token in place.
pub fn dsn_with_dbname(dsn: &str, dbname: &str) -> String {
    dsn.split(' ')
        .map(|tok| match tok.strip_prefix("dbname=") {
            Some(_) => format!("dbname={dbname}"),
            None => tok.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Open the imported db for reads (same engine + id_seed as the CLI — see
/// mcp src/engine.rs open_kernel_auto). The import process has exited by the
/// time this runs, so the store file lock is free. The engine must mirror
/// src/engine.rs `open_engine`'s production default — the child imports as
/// redb (the stable default, PR#2 review SE-01), and reading those bytes
/// back as the native WAL engine is the post-gate equivalent of the REC-002
/// snapshot bug.
pub fn open_kernel(db: &str) -> aikoql_kernel::Kernel {
    use aikoql_kernel::storage::store::StorageEngine;
    use aikoql_kernel::{Kernel, SystemClock};
    let store: std::sync::Arc<dyn StorageEngine> = std::sync::Arc::new(
        aikoql_kernel::storage::store_redb::RedbEngine::open(db)
            .unwrap_or_else(|e| panic!("open {db}: {e}")),
    );
    Kernel::open(store, std::sync::Arc::new(SystemClock), 0xA9C9)
        .unwrap_or_else(|e| panic!("open kernel {db}: {e}"))
}

/// All head KOs of one type. Reads as the importer subject (owner of
/// everything a connector import committed) — authz-clean by construction.
pub fn scan_type(
    k: &aikoql_kernel::Kernel,
    subject: &str,
    type_name: &str,
) -> Vec<aikoql_kernel::KnowledgeObject> {
    k.scan_by_type(&aikoql_kernel::Subject::new(subject), type_name)
        .unwrap_or_else(|e| panic!("scan {type_name}: {e}"))
}

// ---------------------------------------------------------------------------
// MCP query client (TDD items 12..13)
// ---------------------------------------------------------------------------

/// Minimal MCP stdio client — spawns `aikoql-mcp serve <db>`, completes the
/// JSON-RPC initialize handshake, and exposes one `call` (tool name + args).
/// ponytail: a third copy of mcp_real_world/mcp_stdio's private client; those
/// predate this harness and consolidating them is churn outside the TDD items.
pub struct McpQueryClient {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    reader: std::io::BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl McpQueryClient {
    pub fn start(db: &str) -> Self {
        let mut child = Command::new(binary_path())
            .args(["serve", db])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // crash output lands in CI logs
            .spawn()
            .unwrap_or_else(|e| panic!("start MCP server: {e}"));
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut client = McpQueryClient {
            child,
            stdin,
            reader: std::io::BufReader::new(stdout),
            next_id: 1,
        };
        let _ = client.exchange(serde_json::json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "connector-cert", "version": "0"}}
        }));
        // Notifications get no response — write, don't read.
        use std::io::Write;
        client
            .stdin
            .write_all(
                (serde_json::to_string(&serde_json::json!({
                    "jsonrpc": "2.0", "method": "notifications/initialized"
                }))
                .unwrap()
                    + "\n")
                    .as_bytes(),
            )
            .unwrap();
        client.stdin.flush().unwrap();
        client
    }

    /// One JSON-RPC request, one response line (stdio transport).
    fn exchange(&mut self, req: serde_json::Value) -> serde_json::Value {
        use std::io::{BufRead, Write};
        self.stdin
            .write_all((serde_json::to_string(&req).unwrap() + "\n").as_bytes())
            .unwrap();
        self.stdin.flush().unwrap();
        let mut response = String::new();
        self.reader.read_line(&mut response).unwrap();
        serde_json::from_str(&response).unwrap()
    }

    /// Call an MCP tool; returns the parsed `content[0].text` JSON payload.
    pub fn call(&mut self, tool: &str, args: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let v = self.exchange(serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": args}
        }));
        if let Some(err) = v.get("error") {
            panic!("MCP error for {tool}: {err}");
        }
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap_or_else(|_| serde_json::json!({"raw": text}))
    }

    /// Establish session identity (R9): agent_id + roles are injected into
    /// every subsequent tool call — e.g. `["admin"]` reads across owners.
    pub fn session_init(&mut self, agent_id: &str, roles: &[&str]) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        self.exchange(serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "session/init",
            "params": {"agent_id": agent_id, "roles": roles}
        }))
    }
}

impl Drop for McpQueryClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
