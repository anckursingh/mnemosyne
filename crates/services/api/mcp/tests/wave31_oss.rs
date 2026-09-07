//! Wave 3.1 (MVP-QA-003A) — W31-OSS-001 OSS time-to-value.
//!
//! The spec's contract: a fresh developer receives ONLY the README, the
//! quickstart, and the examples, then walks seven tasks:
//!
//! ```text
//! install · start · ingest · query · add second source ·
//! create knowledge-backed agent · debug failure
//! ```
//!
//! Measured: time, completion rate, documentation failures, support
//! interventions. Targets must come from baseline observations, never be
//! invented — so the honest scope is declared here BEFORE measurement:
//!
//! - `w31_oss_001` is the mechanical leg of each task, driven over the
//!   real MCP binary exactly as the quickstart's tool table describes
//!   (remember → find_similar → session_init → explain/trace). Times
//!   are printed, never thresholded (debug-build wall-clock — the
//!   methodology's latency law). Completion is asserted: 7/7.
//! - "install" is the released-binary path the quickstart's 5-second
//!   start assumes (binary present). The from-source build time is a
//!   toolchain variable, not measured here — recorded as a doc note,
//!   not a number.
//! - `w31_oss_002` pins the onboarding-artifact laws: the three
//!   mandated artifacts exist at the mandated paths, and the quickstart
//!   literally covers each of the seven tasks. A missing artifact or an
//!   uncovered task is a real documentation failure (RED), fixed by
//!   shipping the artifact — the spec's own TDD loop.
//! - Support interventions are the steps a fresh developer could not
//!   derive from the three artifacts alone; each one found during this
//!   measurement is a doc gap to close. The tally lives in
//!   docs/benchmarks/oss-time-to-value.md with the baseline table.

use serde_json::{json, Value as J};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Instant;

// ponytail: per-file client mirrors mcp_stdio's (its McpClient is
// private and connectors/mod.rs drags the live-connector stack in);
// extract a shared common module if a third file needs one.
struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    pending: Vec<J>,
}

impl McpClient {
    fn start(db: &PathBuf) -> Self {
        let mut exe = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        exe.push("../../../../target/debug/aikoql-mcp");
        #[cfg(windows)]
        exe.set_extension("exe");
        assert!(exe.exists(), "aikoql-mcp not built at {:?}", exe);
        let mut child = Command::new(&exe)
            .arg(db)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn aikoql-mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        McpClient {
            child,
            stdin,
            stdout,
            next_id: 0,
            pending: Vec::new(),
        }
    }

    fn request(&mut self, method: &str, params: J) -> J {
        self.next_id += 1;
        let id = self.next_id;
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{}", frame).unwrap();
        self.stdin.flush().unwrap();
        loop {
            if let Some(pos) = self
                .pending
                .iter()
                .position(|f| f.get("id").and_then(|i| i.as_u64()) == Some(id))
            {
                let resp = self.pending.remove(pos);
                if let Some(err) = resp.get("error") {
                    panic!("json-rpc error: {}", err);
                }
                return resp.get("result").cloned().unwrap_or(J::Null);
            }
            let mut line = String::new();
            self.stdout.read_line(&mut line).expect("server response");
            let frame: J = serde_json::from_str(line.trim()).expect("valid json-rpc frame");
            if frame.get("id").and_then(|i| i.as_u64()) == Some(id) {
                if let Some(err) = frame.get("error") {
                    panic!("json-rpc error: {}", err);
                }
                return frame.get("result").cloned().unwrap_or(J::Null);
            }
            self.pending.push(frame);
        }
    }

    // Notifications get no response — a request() here would block
    // forever waiting for a reply the server never sends.
    fn notify(&mut self, method: &str) {
        let frame = json!({"jsonrpc": "2.0", "method": method, "params": {}});
        writeln!(self.stdin, "{}", frame).unwrap();
        self.stdin.flush().unwrap();
    }

    fn call_tool(&mut self, name: &str, args: J) -> J {
        let res = self.request("tools/call", json!({"name": name, "arguments": args}));
        assert_eq!(
            res.get("isError").and_then(|b| b.as_bool()),
            Some(false),
            "tool error: {}",
            res
        );
        let text = res["content"][0]["text"].as_str().unwrap().to_string();
        serde_json::from_str(&text).expect("tool payload is json")
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// Temp db paths written by THIS test thread, swept when the thread exits
// (the main thread's destructor runs at process exit — statics are NOT
// dropped on Windows MSVC, TLS is).
thread_local! {
    static TEMP_PATHS: std::cell::RefCell<TempSweeper> =
        const { std::cell::RefCell::new(TempSweeper { paths: Vec::new() }) };
}

struct TempSweeper {
    paths: Vec<PathBuf>,
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

fn tmp_db(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aikoql_w31oss_{}_{}.redb",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    TEMP_PATHS.with(|t| t.borrow_mut().paths.push(p.clone()));
    p
}

#[test]
fn w31_oss_001_time_to_value_flow() {
    let mut report = String::from("task                    | done | µs\n");
    report.push_str("------------------------|------|----------\n");
    let mut completed = 0usize;
    let mut leg = |report: &mut String, name: &str, done: bool, t: u128| {
        if done {
            completed += 1;
        }
        report.push_str(&format!(
            "{name:<23} | {:<4} | {t}\n",
            if done { "yes" } else { "NO" }
        ));
    };

    // 1. install — the released-binary path the quickstart's 5-second
    // start assumes. (From-source build time: toolchain variable, not
    // measured — doc note.)
    let t0 = Instant::now();
    let mut exe = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    exe.push("../../../../target/debug/aikoql-mcp");
    #[cfg(windows)]
    exe.set_extension("exe");
    leg(
        &mut report,
        "install (binary present)",
        exe.exists(),
        t0.elapsed().as_micros(),
    );

    // 2. start — spawn the server, MCP initialize handshake.
    let t0 = Instant::now();
    let db = tmp_db("t2v");
    let mut c = McpClient::start(&db);
    c.request(
        "initialize",
        json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "w31-oss", "version": "0"}}),
    );
    c.notify("notifications/initialized");
    leg(&mut report, "start", true, t0.elapsed().as_micros());

    // 3. ingest — the quickstart's TypeScript hello: a note.
    let t0 = Instant::now();
    let n1 = c.call_tool(
        "remember",
        json!({
            "subject": "fresh-dev",
            "type_name": "note",
            "properties": {"body": "Hello from the aikoql quickstart."},
            "origin": "fresh-dev"
        }),
    );
    let koid1 = n1["koid"].as_str().unwrap().to_string();
    let ok = n1["version"].as_u64() == Some(1);
    leg(
        &mut report,
        "ingest (remember)",
        ok,
        t0.elapsed().as_micros(),
    );

    // 4. query — recall finds it.
    let t0 = Instant::now();
    let q1 = c.call_tool(
        "find_similar",
        json!({"subject": "fresh-dev", "text": "quickstart", "k": 5}),
    );
    let hits1: Vec<&str> = q1["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["koid"].as_str().unwrap())
        .collect();
    leg(
        &mut report,
        "query (find_similar)",
        hits1.contains(&koid1.as_str()),
        t0.elapsed().as_micros(),
    );

    // 5. add second source — ingest another doc, both recall.
    let t0 = Instant::now();
    let n2 = c.call_tool(
        "remember",
        json!({
            "subject": "fresh-dev",
            "type_name": "note",
            "properties": {"body": "Second source: ingestion extracts knowledge IR from documents."},
            "origin": "fresh-dev"
        }),
    );
    let koid2 = n2["koid"].as_str().unwrap().to_string();
    let q2 = c.call_tool(
        "find_similar",
        json!({"subject": "fresh-dev", "text": "pipeline extracts", "k": 5}),
    );
    let hits2: Vec<&str> = q2["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["koid"].as_str().unwrap())
        .collect();
    let q1b = c.call_tool(
        "find_similar",
        json!({"subject": "fresh-dev", "text": "quickstart", "k": 5}),
    );
    let hits1b: Vec<&str> = q1b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["koid"].as_str().unwrap())
        .collect();
    leg(
        &mut report,
        "add second source",
        hits2.contains(&koid2.as_str()) && hits1b.contains(&koid1.as_str()),
        t0.elapsed().as_micros(),
    );

    // 6. knowledge-backed agent — session + agent commits + recalls its
    // own knowledge.
    let t0 = Instant::now();
    let sess = c.call_tool(
        "session_init",
        json!({"agent_id": "hello-agent", "run_id": "oss-1", "roles": ["developer"]}),
    );
    let n3 = c.call_tool(
        "remember",
        json!({
            "subject": "hello-agent",
            "type_name": "claim",
            "properties": {"body": "agent believes the quickstart works"},
            "origin": "hello-agent"
        }),
    );
    let koid3 = n3["koid"].as_str().unwrap().to_string();
    let q3 = c.call_tool(
        "find_similar",
        json!({"subject": "hello-agent", "text": "agent believes", "k": 5}),
    );
    let hits3: Vec<&str> = q3["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["koid"].as_str().unwrap())
        .collect();
    leg(
        &mut report,
        "knowledge-backed agent",
        sess["session"]["agent_id"].as_str() == Some("hello-agent")
            && hits3.contains(&koid3.as_str()),
        t0.elapsed().as_micros(),
    );

    // 7. debug failure — the quickstart's audit tools: why does this KO
    // say what it says, and what is its lineage?
    let t0 = Instant::now();
    let ex = c.call_tool("explain", json!({"subject": "fresh-dev", "koid": koid1}));
    let tr = c.call_tool("trace", json!({"subject": "fresh-dev", "koid": koid1}));
    let ok = ex["event_refs"]
        .as_array()
        .map(|a| !a.is_empty())
        .unwrap_or(false)
        && tr["versions"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false);
    leg(
        &mut report,
        "debug (explain + trace)",
        ok,
        t0.elapsed().as_micros(),
    );

    println!(
        "\n[W31-OSS-001] fresh-developer seven-task flow:\n{}",
        report
    );
    assert_eq!(
        completed, 7,
        "time-to-value completion rate {}/7 — a mandated task failed",
        completed
    );
}

#[test]
fn w31_oss_002_onboarding_artifact_laws() {
    // The three artifacts the spec hands a fresh developer, at the
    // mandated paths.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../..");
    let readme = root.join("README.md");
    let quickstart = root.join("QUICKSTART.md");
    let examples = root.join("examples");
    assert!(
        readme.exists(),
        "README.md missing — mandated onboarding artifact"
    );
    assert!(
        quickstart.exists(),
        "QUICKSTART.md missing — mandated onboarding artifact"
    );
    assert!(
        examples.is_dir(),
        "examples/ missing — mandated onboarding artifact"
    );
    let example_files: Vec<_> = std::fs::read_dir(&examples)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .collect();
    assert!(!example_files.is_empty(), "examples/ ships no example file");

    // README must lead to the other two artifacts — it is the entry
    // point, not a dead end.
    let readme_text = std::fs::read_to_string(&readme).unwrap().to_lowercase();
    assert!(
        readme_text.contains("quickstart.md"),
        "README never points at the quickstart"
    );
    assert!(
        readme_text.contains("examples"),
        "README never points at the examples"
    );

    // The quickstart must literally cover each of the seven mandated
    // tasks — a task a fresh developer cannot map to a quickstart
    // section is a documentation failure, not their mistake.
    let qs = std::fs::read_to_string(&quickstart).unwrap().to_lowercase();
    let coverage: [(&str, &[&str]); 7] = [
        ("install", &["npm i -g", "cargo build"]),
        ("start", &["aikoql-mcp", "stdio"]),
        ("ingest", &["remember", "document_ingest"]),
        ("query", &["find_similar", "aikoql"]),
        ("add second source", &["multi-source"]),
        ("agent", &["agent runtime", "session_init"]),
        ("debug", &["trace", "explain", "eval_"]),
    ];
    let mut failures = Vec::new();
    for (task, markers) in coverage {
        if !markers.iter().any(|m| qs.contains(m)) {
            failures.push(task);
        }
    }
    assert!(
        failures.is_empty(),
        "QUICKSTART.md does not cover mandated task(s): {failures:?}"
    );

    // The example must exercise the real tool registry — an example
    // that names tools the server does not have teaches a fresh
    // developer a lie.
    let hello = examples.join("hello-agent.ts");
    let example_text = std::fs::read_to_string(&hello).unwrap().to_lowercase();
    for tool in ["remember", "findsimilar", "explain", "trace"] {
        assert!(
            example_text.contains(tool),
            "hello-agent.ts never uses the '{tool}' tool — not the documented flow"
        );
    }
}
