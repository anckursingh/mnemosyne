//! MCP stdio integration suite — drives the real server binary over pipes.
//!
//! Includes the flagship acceptance scenario (VISION-AND-STRATEGY Phase 1):
//! an agent commits a claim through MCP, then asks "why do you believe this?"
//! and receives source + confidence + verification + evidence in one call.

use serde_json::{json, Value as J};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

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
        assert!(
            exe.exists(),
            "aikoql-mcp not built at {:?}; run cargo build first",
            exe
        );
        // PRR-4: the default rate limit (120 calls/min) would throttle the
        // load-heavy scenarios (m11 creates 150 objects) — raise it through
        // the config pipeline the tests exercise.
        let cfg = db.with_file_name("aikoql-rate.toml");
        std::fs::write(&cfg, "[rate_limit]\nmax_calls_per_minute = 100000\n").unwrap();
        let mut child = Command::new(&exe)
            .arg("--config")
            .arg(&cfg)
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
            self.stdout.read_line(&mut line).unwrap();
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

    fn take_notifications(&mut self) -> Vec<J> {
        let mut out = Vec::new();
        self.pending.retain(|f| {
            if f.get("method").and_then(|m| m.as_str()) == Some("notifications/notify") {
                out.push(f.clone());
                false
            } else {
                true
            }
        });
        out
    }

    /// Wait for at least `n` notifications and return them. Issues ping
    /// requests to drain stdout. Times out after ~2s.
    fn wait_for_notifications(&mut self, n: usize) -> Vec<J> {
        for _ in 0..200 {
            let notes = self.take_notifications();
            if notes.len() >= n {
                return notes;
            }
            let _ = self.request("ping", json!({}));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Vec::new()
    }

    fn notify(&mut self, method: &str) {
        let frame = json!({"jsonrpc": "2.0", "method": method});
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
    p.push(format!("aikoql_mcp_{}_{}.redb", name, std::process::id()));
    let _ = std::fs::remove_file(&p);
    TEMP_PATHS.with(|t| t.borrow_mut().paths.push(p.clone()));
    p
}

#[test]
fn m01_initialize_and_tools_list() {
    let db = tmp_db("handshake");
    let mut c = McpClient::start(&db);
    let init = c.request(
        "initialize",
        json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
    );
    assert_eq!(init["protocolVersion"], "2024-11-05");
    assert_eq!(init["serverInfo"]["name"], "aikoql-mcp");
    c.notify("notifications/initialized");

    let list = c.request("tools/list", json!({}));
    let names: Vec<&str> = list["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "remember",
        "forget",
        "evolve",
        "verify",
        "get",
        "find_similar",
        "trace",
        "explain",
        "prove",
        "relate",
        "traverse",
        "eval_recall",
        "eval_staleness",
        "eval_contradictions",
        "aikoql",
        "backup",
        "restore",
        "list_backups",
        "metrics",
    ] {
        assert!(names.contains(&expected), "missing tool: {}", expected);
    }
    // Review P0-1: the generic epistemic transition is NOT part of the
    // protocol surface — epistemic change goes through the semantic ops only.
    assert!(
        !names.contains(&"transition_epistemic"),
        "transition_epistemic must not be exposed as an MCP tool"
    );
}

#[test]
fn m02_flagship_why_did_the_agent_know_this() {
    let db = tmp_db("flagship");
    let mut c = McpClient::start(&db);
    c.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "flagship", "version": "0"}}));
    c.notify("notifications/initialized");

    // 1. an agent commits a claim with provenance
    let evidence = c.call_tool(
        "remember",
        json!({
            "subject": "agent-researcher",
            "type_name": "evidence",
            "properties": {"title": "SEC 10-K FY2025"},
            "origin": "agent-researcher"
        }),
    );
    let claim = c.call_tool(
        "remember",
        json!({
            "subject": "agent-researcher",
            "type_name": "claim",
            "properties": {"revenue": "$4.2B", "period": "FY2025"},
            "semantic": {"source": "sec-10k-filing", "confidence": 0.99, "embedding_model": "bge-m3", "embedding": [0.5, 0.5]},
            "origin": "agent-researcher",
            "note": "extracted from filing"
        }),
    );
    let claim_koid = claim["koid"].as_str().unwrap().to_string();
    assert_eq!(claim["version"], 1);

    // 2. claim is verified through its lifecycle
    c.call_tool(
        "evolve",
        json!({"subject": "agent-researcher", "koid": claim_koid, "to": "active"}),
    );
    let v = c.call_tool(
        "evolve",
        json!({"subject": "agent-researcher", "koid": claim_koid, "to": "verified"}),
    );
    assert_eq!(v["state"], "verified");

    // 3. THE FLAGSHIP QUESTION: why do you believe this?
    let ex = c.call_tool(
        "explain",
        json!({"subject": "agent-researcher", "koid": claim_koid}),
    );
    assert_eq!(ex["source"], "sec-10k-filing");
    // confidence is stored f32; compare through JSON with tolerance
    let conf = ex["confidence"].as_f64().expect("confidence present");
    assert!((conf - 0.99).abs() < 1e-6, "confidence {} != 0.99", conf);
    assert_eq!(ex["verified"], true);
    assert!(
        ex["event_refs"].as_array().unwrap().len() >= 3,
        "must carry commit lineage"
    );

    // 4. the audit chain verifies
    let proof = c.call_tool(
        "prove",
        json!({"subject": "agent-researcher", "koid": claim_koid}),
    );
    assert_eq!(proof["chain_valid"], true);
    assert!(proof["events"].as_u64().unwrap() >= 4);

    // 5. recall finds it; lineage is complete
    let recall = c.call_tool(
        "find_similar",
        json!({"subject": "agent-researcher", "text": "revenue", "vector": [0.5, 0.5], "k": 5}),
    );
    let found: Vec<&str> = recall["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["koid"].as_str().unwrap())
        .collect();
    assert!(
        found.contains(&claim_koid.as_str()),
        "recall must find the claim"
    );

    let tr = c.call_tool(
        "trace",
        json!({"subject": "agent-researcher", "koid": claim_koid}),
    );
    assert_eq!(tr["versions"].as_array().unwrap().len(), 3);
    assert_eq!(tr["events"].as_array().unwrap().len(), 3);

    // 6. and it survives a server restart (durability through MCP)
    drop(c);
    let mut c2 = McpClient::start(&db);
    c2.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "flagship", "version": "0"}}));
    let got = c2.call_tool(
        "get",
        json!({"subject": "agent-researcher", "koid": claim_koid}),
    );
    assert_eq!(got["state"], "verified");
    assert_eq!(got["semantic"]["source"], "sec-10k-filing");
    let proof2 = c2.call_tool(
        "prove",
        json!({"subject": "agent-researcher", "koid": claim_koid}),
    );
    assert_eq!(proof2["chain_valid"], true);
    assert_eq!(evidence["version"], 1);
    let _ = std::fs::remove_file(&db);
}

#[test]
fn m03_acl_enforced_through_mcp() {
    let db = tmp_db("acl");
    let mut c = McpClient::start(&db);
    c.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}));
    let r = c.call_tool(
        "remember",
        json!({"subject": "alice", "type_name": "secret"}),
    );
    let koid = r["koid"].as_str().unwrap();

    // bob may not read alice's object
    let res = c.request(
        "tools/call",
        json!({"name": "get", "arguments": {"subject": "bob", "koid": koid}}),
    );
    assert_eq!(res["isError"], true);
    let msg = res["content"][0]["text"].as_str().unwrap();
    assert!(
        msg.contains("ACCESS_DENIED"),
        "expected ACCESS_DENIED, got: {}",
        msg
    );
    let _ = std::fs::remove_file(&db);
}

#[test]
fn m04_durable_notification_and_replay() {
    let db = tmp_db("cdc");
    let mut c = McpClient::start(&db);
    c.request(
        "initialize",
        json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "cdc", "version": "0"}}),
    );
    c.notify("notifications/initialized");

    let sub = c.request("notifications/subscribe", json!({"id": "s1"}));
    assert_eq!(sub["subscribed"], true);
    assert_eq!(sub["replayed"], 0);
    assert!(c.take_notifications().is_empty());

    let r = c.call_tool(
        "remember",
        json!({"subject": "alice", "type_name": "fact", "properties": {"x": 1}}),
    );
    let koid1 = r["koid"].as_str().unwrap().to_string();
    let notes = c.wait_for_notifications(1);
    assert!(!notes.is_empty(), "expected at least one live notification");
    let ev1 = &notes[0]["params"]["event"];
    assert_eq!(ev1["koid"], koid1);
    assert_eq!(ev1["kind"], "Created");
    let seq1 = ev1["seq"].as_u64().unwrap();

    c.request("notifications/ack", json!({"id": "s1", "seq": seq1}));

    let r2 = c.call_tool(
        "remember",
        json!({"subject": "alice", "type_name": "fact", "properties": {"x": 2}}),
    );
    let koid2 = r2["koid"].as_str().unwrap().to_string();
    let notes = c.wait_for_notifications(1);
    assert!(!notes.is_empty());
    let ev2 = &notes[0]["params"]["event"];
    assert_eq!(ev2["koid"], koid2);
    let seq2 = ev2["seq"].as_u64().unwrap();
    assert!(seq2 > seq1);

    // reconnect without acking seq2: the persisted subscription replays it
    drop(c);
    let mut c2 = McpClient::start(&db);
    c2.request(
        "initialize",
        json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "cdc", "version": "0"}}),
    );
    let sub2 = c2.request("notifications/subscribe", json!({"id": "s1"}));
    assert_eq!(sub2["subscribed"], true);
    assert_eq!(sub2["replayed"], 1);
    let notes = c2.take_notifications();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["params"]["event"]["seq"], seq2);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn m05_cross_agent_acl_policy_and_role_inheritance() {
    let db = tmp_db("xacl");
    let mut c = McpClient::start(&db);
    c.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "xacl", "version": "0"}}));
    c.notify("notifications/initialized");

    // admin bootstraps role hierarchy and a cross-agent read policy
    c.call_tool(
        "remember",
        json!({
            "subject": "admin",
            "roles": ["admin"],
            "type_name": "aikoql:role",
            "properties": {"name": "senior", "parents": []}
        }),
    );
    c.call_tool(
        "remember",
        json!({
            "subject": "admin",
            "roles": ["admin"],
            "type_name": "aikoql:role",
            "properties": {"name": "junior", "parents": ["senior"]}
        }),
    );
    c.call_tool(
        "remember",
        json!({
            "subject": "admin",
            "roles": ["admin"],
            "type_name": "aikoql:policy",
            "properties": {
                "target_type": "shared_note",
                "rules": [{"principal": "senior", "action": "read", "effect": "allow"}]
            }
        }),
    );

    // alice (junior) writes a shared note; bob (junior, inheriting senior) reads it
    let note = c.call_tool(
        "remember",
        json!({
            "subject": "alice",
            "roles": ["junior"],
            "type_name": "shared_note",
            "properties": {"body": "hello team"}
        }),
    );
    let koid = note["koid"].as_str().unwrap();

    let got = c.call_tool(
        "get",
        json!({"subject": "bob", "roles": ["junior"], "koid": koid}),
    );
    assert_eq!(got["type_name"], "shared_note");
    assert_eq!(got["properties"]["body"], "hello team");

    // carol has no role and is not the owner, so she is denied
    let res = c.request(
        "tools/call",
        json!({"name": "get", "arguments": {"subject": "carol", "koid": koid}}),
    );
    assert_eq!(res["isError"], true);
    let msg = res["content"][0]["text"].as_str().unwrap();
    assert!(
        msg.contains("ACCESS_DENIED"),
        "expected ACCESS_DENIED, got: {}",
        msg
    );

    let _ = std::fs::remove_file(&db);
}

#[test]
fn m06_memory_evals_over_mcp() {
    let db = tmp_db("evals");
    let mut c = McpClient::start(&db);
    c.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "evals", "version": "0"}}));
    c.notify("notifications/initialized");

    let a = c.call_tool(
        "remember",
        json!({"subject": "eval", "type_name": "fact", "properties": {"body": "alpha"}, "semantic": {"embedding": [1.0, 0.0]}}),
    );
    let _b = c.call_tool(
        "remember",
        json!({"subject": "eval", "type_name": "fact", "properties": {"body": "beta"}, "semantic": {"embedding": [0.0, 1.0]}}),
    );
    let c_oid = c.call_tool(
        "remember",
        json!({"subject": "eval", "type_name": "fact", "properties": {"body": "alpha2"}, "semantic": {"embedding": [0.99, 0.01]}}),
    );

    let recall = c.call_tool(
        "eval_recall",
        json!({"subject": "eval", "type_name": "fact", "text": "alpha", "k": 5, "fusion": "text", "expected": [a["koid"].as_str().unwrap(), c_oid["koid"].as_str().unwrap()]}),
    );
    assert_eq!(recall["hits"].as_u64().unwrap(), 2);
    assert!((recall["recall"].as_f64().unwrap() - 1.0).abs() < 1e-6);

    let staleness = c.call_tool(
        "eval_staleness",
        json!({"subject": "eval", "type_name": "fact", "text": "alpha", "k": 5, "fusion": "text"}),
    );
    assert!(
        staleness["max_lag_ms"].as_u64().unwrap() >= staleness["mean_lag_ms"].as_u64().unwrap()
    );

    let yes = c.call_tool(
        "remember",
        json!({"subject": "eval", "type_name": "claim", "properties": {"claim": "AGI is possible", "answer": true}, "semantic": {"embedding": [1.0, 0.0]}}),
    );
    let no = c.call_tool(
        "remember",
        json!({"subject": "eval", "type_name": "claim", "properties": {"claim": "AGI is impossible", "answer": false}, "semantic": {"embedding": [0.99, 0.01]}}),
    );
    let contradictions = c.call_tool(
        "eval_contradictions",
        json!({"subject": "eval", "type_name": "claim", "property": "answer", "threshold": 0.9, "max_results": 10}),
    );
    let hits = contradictions["contradictions"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    let pair = &hits[0];
    let left = pair["left"].as_str().unwrap();
    let right = pair["right"].as_str().unwrap();
    assert!(
        (left == yes["koid"].as_str().unwrap() && right == no["koid"].as_str().unwrap())
            || (left == no["koid"].as_str().unwrap() && right == yes["koid"].as_str().unwrap())
    );

    let _ = std::fs::remove_file(&db);
}

#[test]
fn m07_graph_relate_and_traverse_over_mcp() {
    let db = tmp_db("graph");
    let mut c = McpClient::start(&db);
    c.request(
        "initialize",
        json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "graph", "version": "0"}}),
    );
    c.notify("notifications/initialized");

    let a = c.call_tool(
        "remember",
        json!({"subject": "alice", "type_name": "note", "properties": {"body": "A"}}),
    );
    let b = c.call_tool(
        "remember",
        json!({"subject": "alice", "type_name": "note", "properties": {"body": "B"}}),
    );
    let c_oid = c.call_tool(
        "remember",
        json!({"subject": "alice", "type_name": "note", "properties": {"body": "C"}}),
    );

    let a_koid = a["koid"].as_str().unwrap();
    let b_koid = b["koid"].as_str().unwrap();
    let c_koid = c_oid["koid"].as_str().unwrap();

    let rel = c.call_tool(
        "relate",
        json!({"subject": "alice", "from": a_koid, "to": b_koid, "rel_type": "references"}),
    );
    assert_eq!(rel["koid"], a_koid);
    assert_eq!(rel["version"], 2);

    let rel_c = c.call_tool(
        "relate",
        json!({"subject": "alice", "from": a_koid, "to": c_koid, "rel_type": "cites"}),
    );

    // idempotent: second identical relate returns the current head version without a new edge
    let rel2 = c.call_tool(
        "relate",
        json!({"subject": "alice", "from": a_koid, "to": b_koid, "rel_type": "references"}),
    );
    assert_eq!(rel2["version"], rel_c["version"]);

    let all = c.call_tool(
        "traverse",
        json!({"subject": "alice", "koid": a_koid, "depth": 1}),
    );
    let hits = all["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2);

    let filtered = c.call_tool(
        "traverse",
        json!({"subject": "alice", "koid": a_koid, "depth": 1, "rel_type": "references"}),
    );
    let fhits = filtered["hits"].as_array().unwrap();
    assert_eq!(fhits.len(), 1);
    assert_eq!(fhits[0]["koid"], b_koid);
    assert_eq!(fhits[0]["direction"], "outbound");

    let _ = std::fs::remove_file(&db);
}

#[test]
fn m_split_entity_over_mcp() {
    let db = tmp_db("split");
    let mut c = McpClient::start(&db);
    c.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "split", "version": "0"}}));
    c.notify("notifications/initialized");

    // A merged entity holding two facts, with a caller-owned edge on it.
    let m = c.call_tool(
        "remember",
        json!({"subject": "alice", "type_name": "entity", "properties": {"name": "Apple", "sector": "banking", "family": "Rosaceae"}}),
    );
    let m_koid = m["koid"].as_str().unwrap();
    let t = c.call_tool(
        "remember",
        json!({"subject": "alice", "type_name": "entity", "properties": {"name": "Subsidiary"}}),
    );
    let t_koid = t["koid"].as_str().unwrap();
    let rel = c.call_tool(
        "relate",
        json!({"subject": "alice", "from": m_koid, "to": t_koid, "rel_type": "supplies"}),
    );

    // QA2-KNOW-006: split the fruit side off, moving its fact and its edge.
    let split = c.call_tool(
        "split_entity",
        json!({
            "subject": "alice",
            "koid": m_koid,
            "expected_version": rel["version"],
            "properties": {"name": "Apple", "family": "Rosaceae"},
            "relationships": [{"rel_type": "supplies", "target": t_koid}],
            "reason": "the bank and the fruit are distinct entities"
        }),
    );
    assert_eq!(split["original_koid"], m_koid);
    let b_koid = split["new_entity_koid"].as_str().unwrap();

    // Side B: the moved fact, the moved edge, and derivation lineage back to A.
    let b = c.call_tool("get", json!({"subject": "alice", "koid": b_koid}));
    assert_eq!(b["type_name"], "entity");
    assert_eq!(b["properties"]["family"], "Rosaceae");
    assert!(b["properties"].get("sector").is_none());
    let b_rels = b["relationships"].as_array().unwrap();
    assert!(b_rels
        .iter()
        .any(|r| r["rel_type"] == "supplies" && r["target"] == t_koid));
    assert!(b_rels
        .iter()
        .any(|r| r["rel_type"] == "derived_from" && r["target"] == m_koid));
    assert_eq!(b["extensions"]["derivation"]["operation"], "split");

    // Side A: same KOID, bumped version, only the unmoved parts survive.
    let a = c.call_tool("get", json!({"subject": "alice", "koid": m_koid}));
    assert_eq!(a["version"], rel["version"].as_u64().unwrap() + 1);
    assert_eq!(a["properties"]["sector"], "banking");
    assert!(a["properties"].get("family").is_none());
    let a_rels = a["relationships"].as_array().unwrap();
    assert!(!a_rels.iter().any(|r| r["rel_type"] == "supplies"));

    let _ = std::fs::remove_file(&db);
}

#[test]
fn m08_aikoql_query_over_mcp() {
    let db = tmp_db("aikoql");
    let mut c = McpClient::start(&db);
    c.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "aikoql", "version": "0"}}));
    c.notify("notifications/initialized");

    c.call_tool("remember", json!({"subject": "alice", "type_name": "Person", "properties": {"name": "Alice", "city": "Amsterdam"}}));
    c.call_tool("remember", json!({"subject": "alice", "type_name": "Person", "properties": {"name": "Bob", "city": "London"}}));

    let all = c.call_tool(
        "aikoql",
        json!({"subject": "alice", "query": "MATCH Person RETURN *"}),
    );
    assert_eq!(all["results"].as_array().unwrap().len(), 2);

    let filtered = c.call_tool(
        "aikoql",
        json!({"subject": "alice", "query": "MATCH Person WHERE name == \"Alice\" RETURN *"}),
    );
    assert_eq!(filtered["results"].as_array().unwrap().len(), 1);

    let _ = std::fs::remove_file(&db);
}

// --- MRFC-0040 Agent Experience tests ---

#[test]
fn m09_session_identity_persistence() {
    // Verify session/init establishes identity that subsequent calls inherit.
    let db = tmp_db("session");
    let mut c = McpClient::start(&db);
    c.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "session-test", "version": "0"}}));
    c.notify("notifications/initialized");

    // Establish session identity via session/init method.
    let sess = c.request(
        "session/init",
        json!({
            "agent_id": "pm-agent-7",
            "run_id": "run-42",
            "roles": ["admin", "reviewer"]
        }),
    );
    assert_eq!(sess["session"]["agent_id"], "pm-agent-7");
    assert_eq!(sess["session"]["run_id"], "run-42");
    assert!(sess["established"].as_bool().unwrap());

    // Create a KO without passing "subject" — session identity should be used.
    let r = c.call_tool(
        "remember",
        json!({
            "type_name": "Task",
            "properties": {"title": "Fix login bug", "priority": 1}
        }),
    );
    let koid = r["koid"].as_str().unwrap().to_string();
    assert!(!koid.is_empty());

    // Verify the KO was created and is retrievable (session identity has access).
    let ko = c.call_tool("get", json!({"koid": koid}));
    assert_eq!(ko["properties"]["title"], "Fix login bug");
    assert_eq!(ko["type_name"], "Task");

    // Verify session_init tool also works (backward compat).
    let sess2 = c.call_tool(
        "session_init",
        json!({
            "agent_id": "qa-agent-3",
            "run_id": "run-99",
            "roles": ["tester"]
        }),
    );
    assert_eq!(sess2["session"]["agent_id"], "qa-agent-3");
    assert_eq!(sess2["session"]["roles"][0], "tester");

    // Now creates should use the new identity.
    let r2 = c.call_tool(
        "remember",
        json!({
            "type_name": "Task",
            "properties": {"title": "Verify login fix"}
        }),
    );
    let koid2 = r2["koid"].as_str().unwrap();
    let ko2 = c.call_tool("get", json!({"koid": koid2}));
    assert_eq!(ko2["properties"]["title"], "Verify login fix");

    let _ = std::fs::remove_file(&db);
}

#[test]
fn m10_session_roles_merged_with_call_roles() {
    // Verify session roles are merged with per-call roles.
    let db = tmp_db("session_roles");
    let mut c = McpClient::start(&db);
    c.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "roles-test", "version": "0"}}));
    c.notify("notifications/initialized");

    c.request(
        "session/init",
        json!({
            "agent_id": "pm-agent-7",
            "roles": ["admin"]
        }),
    );

    // Create a KO — session roles should be applied.
    let r = c.call_tool(
        "remember",
        json!({
            "type_name": "Task",
            "properties": {"title": "Test role merge"}
        }),
    );
    let koid = r["koid"].as_str().unwrap();

    // The KO is accessible (session identity with admin role was used).
    let ko = c.call_tool("get", json!({"koid": koid}));
    assert_eq!(ko["properties"]["title"], "Test role merge");

    let _ = std::fs::remove_file(&db);
}

// --- MRFC-0040 Streaming tests ---

#[test]
fn m11_aikoql_stream_over_mcp() {
    // Verify aikoql/stream delivers results in chunks via notifications.
    let db = tmp_db("stream");
    let mut c = McpClient::start(&db);
    c.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "stream-test", "version": "0"}}));
    c.notify("notifications/initialized");

    // Create 150 objects to ensure 2+ chunks (chunk_size=100).
    for i in 0..150 {
        c.call_tool(
            "remember",
            json!({
                "subject": "alice",
                "type_name": "Item",
                "properties": {"idx": i, "label": format!("item-{}", i)}
            }),
        );
    }

    // Stream query: request returns first chunk, remaining come as notifications.
    let first = c.request(
        "aikoql/stream",
        json!({
            "query": "MATCH Item RETURN *",
            "subject": "alice"
        }),
    );
    assert_eq!(first["chunk"], 0);
    assert!(
        first["total_chunks"].as_u64().unwrap() >= 2,
        "expected 2+ chunks for 150 items"
    );
    let stream_id = first["stream_id"].as_str().unwrap().to_string();
    let first_results = first["results"].as_array().unwrap();
    assert!(!first_results.is_empty());

    // Collect remaining chunks from notification frames.
    let mut all_results: Vec<J> = first_results.clone();
    let remaining_chunks = first["total_chunks"].as_u64().unwrap() as usize - 1;
    let notes = c.wait_for_notifications(remaining_chunks);
    for note in &notes {
        let params = &note["params"];
        assert_eq!(params["stream_id"].as_str().unwrap(), stream_id);
        if let Some(results) = params["results"].as_array() {
            for r in results {
                all_results.push(r.clone());
            }
        }
        if params
            .get("done")
            .and_then(|d| d.as_bool())
            .unwrap_or(false)
        {
            break;
        }
    }

    assert_eq!(all_results.len(), 150);
    // Verify unique KOIDs.
    let koids: std::collections::HashSet<String> = all_results
        .iter()
        .map(|r| r["koid"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(koids.len(), 150);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn m12_agent_runtime_execute_agent_with_skills() {
    // Deploy a Program KO, then an Agent KO referencing it, and execute the
    // agent. Verifies Agent Runtime resolves skills → programs and runs them.
    let db = tmp_db("agent_runtime");
    let mut c = McpClient::start(&db);
    c.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "agent-test", "version": "0"}}));
    c.notify("notifications/initialized");
    c.call_tool(
        "session_init",
        json!({"agent_id": "tester", "roles": ["admin"]}),
    );

    // Create test data.
    c.call_tool(
        "remember",
        json!({
            "subject": "tester", "type_name": "Person",
            "properties": {"name": "Ada", "dept": "Eng"}
        }),
    );
    c.call_tool(
        "remember",
        json!({
            "subject": "tester", "type_name": "Person",
            "properties": {"name": "Bob", "dept": "HR"}
        }),
    );

    // Deploy a Program KO that filters by department.
    c.call_tool(
        "deploy_program",
        json!({
            "name": "FindEngPeople",
            "body": "MATCH Person WHERE dept == \"Eng\" RETURN name",
            "language": "aikoql",
            "subject": "tester"
        }),
    );

    // Deploy an Agent KO with the program as a skill.
    let agent = c.call_tool(
        "deploy_agent",
        json!({
            "name": "HRAssistant",
            "prompt": "You help find people in the org.",
            "skills": ["FindEngPeople"],
            "tools": [],
            "policies": [],
            "subject": "tester"
        }),
    );
    let agent_koid = agent["koid"].as_str().unwrap();

    // Execute the agent.
    let result = c.call_tool(
        "execute_agent",
        json!({"koid": agent_koid, "subject": "tester"}),
    );
    let log: Vec<String> = result["execution_log"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let log_text = log.join("\n");

    assert!(
        log_text.contains("HRAssistant"),
        "log should mention agent name, got: {}",
        log_text
    );
    assert!(
        log_text.contains("FindEngPeople"),
        "log should mention skill name, got: {}",
        log_text
    );
    assert!(
        log_text.contains("OK:"),
        "log should show successful execution, got: {}",
        log_text
    );

    let _ = std::fs::remove_file(&db);
}

// --- MRFC-0050 Document Ingestion tests ---

#[test]
fn m13_document_ingest_and_extract_text() {
    // D1 acceptance: ingest a text file, verify extraction populates page_count and char_count.
    let db = tmp_db("doc_d1");
    let mut c = McpClient::start(&db);
    c.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "doc-test", "version": "0"}}));
    c.notify("notifications/initialized");
    c.call_tool(
        "session_init",
        json!({"agent_id": "tester", "roles": ["admin"]}),
    );

    // Base64-encode a simple text document.
    let b64 = "SGVsbG8gZnJvbSBNbmVtb3N5bmUgRDEuClRoaXMgaXMgYSB0ZXN0IGRvY3VtZW50LgpJdCBoYXMgdGhyZWUgbGluZXMu";

    let ingested = c.call_tool(
        "document_ingest",
        json!({
            "filename": "test-d1.txt",
            "content_base64": b64,
            "mime_type": "text/plain"
        }),
    );
    let koid = ingested["koid"].as_str().unwrap();
    assert!(!koid.is_empty(), "ingest must return a koid");
    assert_eq!(
        ingested["status"], "extracted",
        "text/plain must be extracted"
    );
    assert_eq!(ingested["page_count"], 1, "plain text = 1 page");
    let char_count = ingested["char_count"].as_i64().unwrap();
    assert!(char_count > 0, "extracted text must have characters");

    // Verify via document_status.
    let status = c.call_tool("document_status", json!({"koid": koid}));
    assert_eq!(status["koid"], koid);
    assert_eq!(status["status"], "extracted");
    assert_eq!(status["page_count"], 1);
    assert_eq!(status["char_count"].as_i64().unwrap(), char_count);

    // Verify via document_list.
    let list = c.call_tool("document_list", json!({"subject": "tester"}));
    let docs = list["documents"].as_array().unwrap();
    assert!(!docs.is_empty(), "document list must contain ingested doc");
    let found = docs.iter().find(|d| d["koid"] == koid).unwrap();
    assert_eq!(found["filename"], "test-d1.txt");
    assert_eq!(found["status"], "extracted");

    // Verify dedup: same content returns existing document.
    let dedup = c.call_tool(
        "document_ingest",
        json!({
            "filename": "test-d1-dup.txt",
            "content_base64": b64,
            "mime_type": "text/plain"
        }),
    );
    assert_eq!(dedup["status"], "duplicate");
    assert_eq!(dedup["koid"], koid);

    // Verify unsupported format still ingests (with status "ingested").
    let binary_b64 = "AAECAwQ=";
    let unsupported = c.call_tool(
        "document_ingest",
        json!({
            "filename": "unknown.bin",
            "content_base64": binary_b64,
            "mime_type": "application/octet-stream"
        }),
    );
    assert_eq!(
        unsupported["status"], "ingested",
        "unsupported format still ingested"
    );
    assert_eq!(unsupported["page_count"], 0);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn m14_document_ocr_detection_and_source_tagging() {
    // D2 acceptance: verify pages are tagged with source="native" for native text,
    // and OCR tools are detected/absent gracefully.
    let dir = std::env::temp_dir().join("aikoql-d2-test");
    std::fs::create_dir_all(&dir).unwrap();

    // Write a text file and verify source tagging.
    let txt_path = dir.join("source-test.txt");
    std::fs::write(&txt_path, "Hello from D2 test.\nThis has two lines.\n").unwrap();
    let doc = aikoql_ingestion::extract_document(&txt_path.to_string_lossy(), "text/plain", None)
        .unwrap();
    assert_eq!(doc.page_count, 1);
    assert_eq!(doc.pages[0].source, "native");
    assert!(doc.pages[0].text.contains("Hello from D2 test"));

    // Verify OCR decision heuristic is wired (empty page needs OCR).
    assert!(aikoql_ingestion::page_needs_ocr("", 10));
    assert!(!aikoql_ingestion::page_needs_ocr(
        "This is a full page of text.",
        10
    ));

    // Verify tool_available returns false for garbage, true for a real command.
    assert!(!aikoql_ingestion::tool_available(
        "nonexistent-tool-xyzzy-12345"
    ));
    // cmd.exe or sh must exist.
    assert!(aikoql_ingestion::tool_available("cmd") || aikoql_ingestion::tool_available("sh"));

    std::fs::remove_dir_all(&dir).ok();
}

// --- MRFC-0050 Document Compilation test ---

#[test]
fn m15_document_compile_pipeline() {
    // D9 acceptance: ingest a document, compile it, verify all pipeline phases
    // produce non-empty output.
    let db = tmp_db("doc_compile");
    let mut c = McpClient::start(&db);
    c.request(
        "initialize",
        json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "compile-test", "version": "0"}}),
    );
    c.notify("notifications/initialized");
    c.call_tool(
        "session_init",
        json!({"agent_id": "tester", "roles": ["admin"]}),
    );

    // Ingest a document with structured business content.
    let content = "Om Building Materials\n\
                   GSTIN: 10CQAPS3890L1ZM\n\
                   Shop No. 12, Gandhi Nagar, Patna, Bihar\n\n\
                   Achintya Industries Pvt. Ltd.\n\
                   GSTIN: 09AADCA1234C1Z5\n\
                   Plot 45, Industrial Area, Kanpur, UP\n\n\
                   TAX INVOICE\n\
                   Invoice No: INV-2024-001\n\
                   Date: 2024-07-15\n\n\
                   Grey Cement, HSN 2523291, 220 Bags, Rs.590/bag\n\
                   Fe 500 TMT Bar, HSN 7214200, 10 MT, Rs.58500/MT\n\
                   Taxable: Rs.714800, IGST: Rs.141644, Total: Rs.856444";
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());

    let ingested = c.call_tool(
        "document_ingest",
        json!({
            "filename": "invoice-test.txt",
            "content_base64": b64,
            "mime_type": "text/plain"
        }),
    );
    let koid = ingested["koid"].as_str().unwrap();
    assert!(!koid.is_empty());

    // Compile the document.
    let result = c.call_tool("document_compile", json!({"koid": koid}));

    // Verify IR: entities discovered.
    let ir = &result["ir"];
    let entities = ir["entities"].as_array().unwrap();
    assert!(
        !entities.is_empty(),
        "IR should discover entities from invoice text"
    );

    // Verify entities contain invoice-related names.
    let entity_names: Vec<&str> = entities
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(
        entity_names
            .iter()
            .any(|n| n.contains("Om") || n.contains("Building")),
        "should find 'Om Building Materials' in entities: {:?}",
        entity_names
    );

    // Verify ontology proposals.
    let ontology = &result["ontology"];
    let classes = ontology["classes"].as_array().unwrap();
    let _props = ontology["properties"].as_array().unwrap();
    let _rels = ontology["relationships"].as_array().unwrap();
    assert!(!classes.is_empty(), "ontology should propose classes");

    // Verify resolution stats.
    let res = &result["resolution"];
    let res_stats = &res["stats"];
    assert!(res_stats["total_entities"].as_u64().unwrap() > 0);
    assert_eq!(
        res_stats["total_entities"].as_u64().unwrap(),
        res_stats["matched_count"].as_u64().unwrap()
            + res_stats["ambiguous_count"].as_u64().unwrap()
            + res_stats["unmatched_count"].as_u64().unwrap()
    );

    // Verify commit plan has actions.
    let plan = &result["commit_plan"];
    let actions = plan["actions"].as_array().unwrap();
    assert!(
        !actions.is_empty(),
        "commit plan must have at least one action"
    );
    let plan_stats = &plan["stats"];
    assert!(plan_stats["total_actions"].as_u64().unwrap() > 0);

    // Verify embedded chunks.
    let chunks = result["embedded_chunks"].as_array().unwrap();
    assert!(
        !chunks.is_empty(),
        "should produce at least one embedded chunk"
    );
    // Each chunk must have an embedding vector.
    for chunk in chunks {
        let emb = chunk["embedding"].as_array().unwrap();
        assert!(!emb.is_empty(), "each chunk must have an embedding");
    }

    // Verify evidence trail covers all phases.
    let trail = &result["evidence_trail"];
    let nodes = trail["nodes"].as_array().unwrap();
    assert!(!nodes.is_empty(), "evidence trail must have nodes");
    let phases: std::collections::HashSet<&str> =
        nodes.iter().map(|n| n["phase"].as_str().unwrap()).collect();
    assert!(phases.contains("D4-semantic-ir"));
    assert!(phases.contains("D5-ontology"));
    assert!(phases.contains("D6-resolution"));
    assert!(phases.contains("D7-reconcile"));

    // Verify stats: 8 phases (D3-ast .. D8-projection + D8-visual-index;
    // D4 splits into the boundary stream and the semantic leg).
    let stats = &result["stats"];
    let phases_arr = stats["phases"].as_array().unwrap();
    assert_eq!(
        phases_arr.len(),
        8,
        "pipeline must have 8 phases (D3-D8 + visual index)"
    );
    assert!(
        phases_arr
            .iter()
            .any(|p| p["phase"].as_str() == Some("D4-fragments")),
        "the boundary stream phase must be reported"
    );
    assert!(stats["total_us"].as_u64().unwrap() > 0);

    let _ = std::fs::remove_file(&db);
}

// ---- v0.3 K1 acceptance -----------------------------------------------------

#[test]
fn k1_epistemic_and_evidence_end_to_end() {
    let db = tmp_db("k1");
    let mut c = McpClient::start(&db);
    c.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "k1", "version": "0"}}));
    c.notify("notifications/initialized");

    // 1. An agent commits knowledge with an evidence trail declared at the
    // protocol boundary — via the semantic assert op (review P0-1:
    // remember() rejects kernel-managed extensions like evidence/authority).
    let created = c.call_tool(
        "assert_knowledge",
        json!({
            "subject": "agent-researcher",
            "type_name": "claim",
            "properties": {"revenue": "$4.2B"},
            "authority": "documentation",
            "evidence": [{
                "source_artifact": "sec-10k-filing.pdf",
                "method": "doc_extraction",
                "location": "page 42",
                "confidence": 0.95
            }]
        }),
    );
    let koid = created["koid"].as_str().unwrap().to_string();
    assert_eq!(created["version"], 1);

    // 1b. The bypass is closed at the protocol boundary too: remember with
    // a kernel-managed extension key is a tool error, not a silent stamp.
    let res = c.request(
        "tools/call",
        json!({
            "name": "remember",
            "arguments": {
                "subject": "agent-researcher",
                "type_name": "claim",
                "properties": {"revenue": "$4.2B"},
                "extensions": {"authority": "human_approved"}
            }
        }),
    );
    assert_eq!(res.get("isError").and_then(|b| b.as_bool()), Some(true));

    // 2. Epistemic baseline stamped on the write; explicit authority wins.
    // Scope is origin-stamped by the kernel (agent assertion → session).
    let ko = c.call_tool("get", json!({"subject": "agent-researcher", "koid": koid}));
    assert_eq!(ko["extensions"]["epistemic_status"], "asserted");
    assert_eq!(ko["extensions"]["authority"], "documentation");
    assert_eq!(ko["extensions"]["scope"], "session");

    // 3. Evidence survives ingestion -> commit -> storage -> query with
    // every detail intact (no silent epistemic metadata drop).
    let ev = &ko["extensions"]["evidence"][0];
    assert_eq!(ev["source_artifact"], "sec-10k-filing.pdf");
    assert_eq!(ev["method"], "doc_extraction");
    assert_eq!(ev["location"], "page 42");
    let conf = ev["confidence"].as_f64().expect("confidence present");
    assert!((conf - 0.95).abs() < 1e-6, "confidence {} != 0.95", conf);

    // 4. Epistemic transitions through the protocol via the semantic ops:
    // verify_knowledge is the only route to `verified` (review P0-1 — the
    // generic transition is not on the protocol surface), and the
    // append-only history lands.
    let t = c.call_tool(
        "verify_knowledge",
        json!({
            "subject": "agent-researcher",
            "koid": koid,
            "evidence": [{"source_artifact": "review-notes.md", "method": "human_provided", "confidence": 0.9}],
            "note": "human review"
        }),
    );
    assert_eq!(t["status"], "verified");
    assert_eq!(t["confirmations"], 1);
    let ko = c.call_tool("get", json!({"subject": "agent-researcher", "koid": koid}));
    assert_eq!(ko["extensions"]["epistemic_status"], "verified");
    let history = ko["extensions"]["epistemic_history"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["by"], "agent-researcher");
    assert_eq!(history[0]["reason"], "human review");

    // 5. The generic transition is NOT a protocol tool: the rawCall surfaces
    // as a tool error, and illegal epistemic moves fail at the semantic ops
    // (table enforced in production, not just in the kernel).
    let res = c.request(
        "tools/call",
        json!({
            "name": "transition_epistemic",
            "arguments": {"subject": "agent-researcher", "koid": koid, "to": "observed"}
        }),
    );
    assert_eq!(res.get("isError").and_then(|b| b.as_bool()), Some(true));

    // 6. Lifecycle transitions create evidence too.
    c.call_tool(
        "evolve",
        json!({"subject": "agent-researcher", "koid": koid, "to": "active"}),
    );
    let ko = c.call_tool("get", json!({"subject": "agent-researcher", "koid": koid}));
    let lh = ko["extensions"]["lifecycle_history"].as_array().unwrap();
    assert_eq!(lh.len(), 1);
    assert_eq!(lh[0]["from"], "draft");
    assert_eq!(lh[0]["to"], "active");

    let _ = std::fs::remove_file(&db);
}

// ---- v0.3 K2 acceptance ------------------------------------------------------

#[test]
fn k2_temporal_operators_end_to_end() {
    let db = tmp_db("k2");
    let mut c = McpClient::start(&db);
    c.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "k2", "version": "0"}}));
    c.notify("notifications/initialized");

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    // 1. Two generations of a fact. The old one is valid since the epoch
    // (timeless upper bound); the new one is valid since Nov 2023.
    let old = c.call_tool(
        "remember",
        json!({
            "subject": "alice",
            "type_name": "claim",
            "properties": {"text": "we use kafka"},
            "extensions": {"valid_from": 0}
        }),
    );
    let old_koid = old["koid"].as_str().unwrap().to_string();
    let new = c.call_tool(
        "remember",
        json!({
            "subject": "alice",
            "type_name": "claim",
            "properties": {"text": "we use rabbitmq"},
            "extensions": {"valid_from": 1_700_000_000_000u64}
        }),
    );
    let new_koid = new["koid"].as_str().unwrap().to_string();

    let ql = |c: &mut McpClient, query: &str| {
        c.call_tool("aikoql", json!({"subject": "alice", "query": query}))["results"]
            .as_array()
            .unwrap()
            .clone()
    };

    // 2. Default MATCH answers with current truth: both generations are
    // valid now.
    assert_eq!(ql(&mut c, "MATCH claim RETURN *").len(), 2);

    // 3. BETWEEN narrows to valid-time overlap: only the old generation was
    // valid during [1000, 2000).
    let between = ql(&mut c, "MATCH claim BETWEEN 1000 AND 2000 RETURN *");
    assert_eq!(between.len(), 1);
    assert_eq!(between[0]["properties"]["text"], "we use kafka");

    // 4. AS_OF is transaction-time reconstruction: nothing existed at epoch 0.
    assert_eq!(ql(&mut c, "MATCH claim AS_OF 0 RETURN *").len(), 0);
    let as_of_now = ql(
        &mut c,
        &format!("MATCH claim AS_OF {} RETURN *", now_ms + 60_000),
    );
    assert_eq!(as_of_now.len(), 2);

    // 5. Supersession through the protocol via the semantic op: validity
    // ends now and the SUPERSEDES edge old -> new is wired. The successor
    // already exists, so supersede() with superseded_by supersedes without
    // creating a new generation (review P0-1).
    let t = c.call_tool(
        "supersede",
        json!({
            "subject": "alice",
            "old": old_koid,
            "superseded_by": new_koid,
            "evidence": [{"source_artifact": "migration-runbook.md", "method": "runtime_observation", "confidence": 0.95}],
            "reason": "migrated to rabbitmq"
        }),
    );
    assert_eq!(t["old"], old_koid.as_str());
    assert_eq!(t["new"], new_koid.as_str());
    let ko = c.call_tool("get", json!({"subject": "alice", "koid": old_koid}));
    assert_eq!(ko["extensions"]["epistemic_status"], "superseded");
    let valid_to = ko["extensions"]["valid_to"].as_i64().unwrap();
    assert!(
        (valid_to as u64) >= now_ms - 60_000,
        "supersession must end validity at ~now, got {}",
        valid_to
    );
    // The supersession evidence is stamped on the old claim — never dropped.
    assert!(
        ko["extensions"]["evidence"]
            .as_array()
            .map(|e| !e.is_empty())
            .unwrap_or(false),
        "supersession evidence must be stamped on the superseded claim"
    );
    let hits = c.call_tool(
        "traverse",
        json!({"subject": "alice", "koid": old_koid, "rel_type": "supersedes"}),
    );
    assert_eq!(hits["hits"].as_array().unwrap().len(), 1);
    assert_eq!(hits["hits"][0]["koid"], new_koid);

    // 6. Current truth excludes the superseded generation — no application
    // code reconstructs this; the runtime enforces validity.
    let current = ql(&mut c, "MATCH claim RETURN *");
    assert_eq!(current.len(), 1);
    assert_eq!(current[0]["properties"]["text"], "we use rabbitmq");

    // 7. HISTORICAL reconstructs every committed version: old appears three
    // times (created + superseded + evidence stamp), new once.
    let hist = ql(&mut c, "MATCH claim HISTORICAL RETURN *");
    assert_eq!(hist.len(), 4);
    let old_versions: Vec<u64> = hist
        .iter()
        .filter(|r| r["koid"] == old_koid.as_str())
        .map(|r| r["version"].as_u64().unwrap())
        .collect();
    assert_eq!(old_versions, vec![1, 2, 3], "ascending commit order");

    // 8. K1 leftover closed: protocol-level epistemic filter. The successor
    // passes human review (semantic verification, review P0-1); EPISTEMIC
    // verified returns only it.
    c.call_tool(
        "verify_knowledge",
        json!({
            "subject": "alice",
            "koid": new_koid,
            "evidence": [{"source_artifact": "ops-review.md", "method": "human_provided", "confidence": 0.9}],
            "note": "ops review"
        }),
    );
    let verified = ql(&mut c, "MATCH claim EPISTEMIC verified RETURN *");
    assert_eq!(verified.len(), 1);
    assert_eq!(verified[0]["koid"], new_koid.as_str());

    let _ = std::fs::remove_file(&db);
}

#[test]
fn k3_derivation_and_lineage_end_to_end() {
    let db = tmp_db("k3");
    let mut c = McpClient::start(&db);
    c.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "k3", "version": "0"}}));
    c.notify("notifications/initialized");

    // Two premise claims, each carrying structured evidence.
    let p1 = c.call_tool(
        "remember",
        json!({
            "subject": "alice",
            "type_name": "observation",
            "properties": {"env": "prod", "cpu": 41}
        }),
    );
    let p1_koid = p1["koid"].as_str().unwrap().to_string();
    let p2 = c.call_tool(
        "remember",
        json!({
            "subject": "alice",
            "type_name": "observation",
            "properties": {"env": "prod", "cpu": 43}
        }),
    );
    let p2_koid = p2["koid"].as_str().unwrap().to_string();
    // Confidence is kernel-managed (review P0-1): seed it via the semantic
    // verify op, not a remember extension.
    c.call_tool(
        "verify_knowledge",
        json!({
            "subject": "alice",
            "koid": p1_koid,
            "confidence": 0.8,
            "evidence": [{"source_artifact": "monitoring/grafana", "method": "runtime_observation"}]
        }),
    );

    // 1. Derive a conclusion through the protocol — first-class operation.
    let d = c.call_tool(
        "derive",
        json!({
            "subject": "alice",
            "type_name": "conclusion",
            "properties": {"env": "prod", "cpu_is_high": true},
            "sources": [p1_koid, p2_koid],
            "operation": "inference",
            "actor": "agent-7",
            "model": "claude-sonnet-5",
            "reason": "two independent observations agree cpu is elevated",
            "evidence": [{"source_artifact": "monitoring/grafana", "method": "runtime_observation", "location": "prod cluster", "confidence": 0.9}]
        }),
    );
    let d_koid = d["koid"].as_str().unwrap().to_string();

    // 2. The derived KO carries the full derivation record at the query
    // boundary — all six questions answerable from one trace call.
    let ko = c.call_tool("get", json!({"subject": "alice", "koid": d_koid}));
    let ext = ko["extensions"].clone();
    assert_eq!(
        ext["epistemic_status"], "inferred",
        "Origin::Reason => Inferred"
    );
    let deriv = ext["derivation"].clone();
    assert_eq!(deriv["operation"], "inference"); // DERIVED HOW
                                                 // Review P1-9 (Test 5): the caller-supplied "actor": "agent-7" arg is
                                                 // IGNORED — the tool binds the actor to the session identity ("alice"),
                                                 // so provenance can never be spoofed through the protocol boundary.
    assert_eq!(deriv["actor"], "alice"); // BY WHOM
    assert_eq!(deriv["model"], "claude-sonnet-5");
    assert_eq!(
        deriv["reason"],
        "two independent observations agree cpu is elevated"
    ); // WHY
    let sources = deriv["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 2);
    assert!(sources.contains(&json!(p1_koid)) && sources.contains(&json!(p2_koid))); // FROM WHAT
    assert!(deriv["timestamp"].as_u64().is_some()); // WHEN
                                                    // Baseline confidence: one source had 0.8, the other none -> 0.8, 1 confirmation.
    let conf = ext["confidence"].clone();
    assert!((conf["score"].as_f64().unwrap() - 0.8).abs() < 0.001);
    assert_eq!(conf["confirmations"], 1);

    // 3. DERIVED_FROM edges are traversable from either premise — the
    // invalidation input for K4.
    for p in [&p1_koid, &p2_koid] {
        let hits = c.call_tool(
            "traverse",
            json!({"subject": "alice", "koid": p, "rel_type": "derived_from"}),
        );
        let hits = hits["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["koid"], d_koid.as_str());
    }

    // 4. trace answers all six questions in one call.
    let t = c.call_tool("trace", json!({"subject": "alice", "koid": d_koid}));
    let tr = t["derivation"].clone();
    assert_eq!(tr["operation"], "inference");
    assert_eq!(tr["actor"], "alice"); // session identity, not the forged arg (P1-9)
    assert_eq!(tr["model"], "claude-sonnet-5");
    assert_eq!(
        tr["reason"],
        "two independent observations agree cpu is elevated"
    );
    assert_eq!(tr["sources"].as_array().unwrap().len(), 2);
    assert_eq!(tr["sources"][0]["type_name"], "observation");
    let te = t["evidence"].as_array().unwrap();
    assert_eq!(te.len(), 1);
    assert_eq!(te[0]["source_artifact"], "monitoring/grafana");
    assert_eq!(te[0]["method"], "runtime_observation");
    assert!(
        (t["confidence"]["score"].as_f64().unwrap() - 0.8).abs() < 0.001,
        "f32 scores serialize with f64 rounding"
    );

    // 5. A bare pointer is not enough: deriving from a missing KO fails —
    // the operation validates premises, it does not cosplay a property write.
    // (Raw request: call_tool would panic on a tool error, which is the
    // behavior under test here.)
    let bad = c.request(
        "tools/call",
        json!({
            "name": "derive",
            "arguments": {
                "subject": "alice",
                "type_name": "conclusion",
                "sources": ["ffffffffffffffffffffffffffffffff"]
            }
        }),
    );
    assert_eq!(bad.get("isError").and_then(|b| b.as_bool()), Some(true));

    let _ = std::fs::remove_file(&db);
}

#[test]
fn k4_knowledge_transactions_end_to_end() {
    let db = tmp_db("k4");
    let mut c = McpClient::start(&db);
    c.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "k4", "version": "0"}}));
    c.notify("notifications/initialized");

    // 1. assert_knowledge: authority + evidence mandatory, stamped on the KO.
    let a = c.call_tool(
        "assert_knowledge",
        json!({
            "subject": "alice",
            "type_name": "claim",
            "properties": {"env": "prod", "cpu": 41},
            "authority": "source_code",
            "evidence": [{"source_artifact": "src/main.rs", "method": "ast_extraction"}]
        }),
    );
    let a_koid = a["koid"].as_str().unwrap().to_string();
    let ko = c.call_tool("get", json!({"subject": "alice", "koid": a_koid}));
    assert_eq!(ko["extensions"]["epistemic_status"], "asserted");
    assert_eq!(ko["extensions"]["authority"], "source_code");

    // 2. observe + verify_knowledge: verification is not a status flip — it
    // bumps the confidence context.
    let o = c.call_tool(
        "observe",
        json!({
            "subject": "alice",
            "type_name": "sighting",
            "properties": {"temp": 21},
            "evidence": [{"source_artifact": "thermometer-1", "method": "runtime_observation"}]
        }),
    );
    let o_koid = o["koid"].as_str().unwrap().to_string();
    let v = c.call_tool(
        "verify_knowledge",
        json!({
            "subject": "alice",
            "koid": o_koid,
            "evidence": [{"source_artifact": "ci-run-1", "method": "ci_observation"}]
        }),
    );
    assert_eq!(v["status"], "verified");
    assert_eq!(v["confirmations"], 1);
    let oko = c.call_tool("get", json!({"subject": "alice", "koid": o_koid}));
    assert_eq!(oko["extensions"]["epistemic_status"], "verified");

    // 3. Derive a dependent from the claim (the invalidation input).
    let d = c.call_tool(
        "derive",
        json!({
            "subject": "alice",
            "type_name": "conclusion",
            "properties": {"cpu_is_high": true},
            "sources": [a_koid],
            "operation": "inference",
            "reason": "elevated cpu"
        }),
    );
    let d_koid = d["koid"].as_str().unwrap().to_string();

    // 4. contradict: counter + persisted Conflict KO; original untouched.
    let cc = c.call_tool(
        "contradict",
        json!({
            "subject": "alice",
            "claim": a_koid,
            "properties": {"env": "prod", "cpu": 87},
            "authority": "documentation",
            "evidence": [{"source_artifact": "ops-runbook", "method": "doc_extraction"}]
        }),
    );
    let counter_koid = cc["counter"].as_str().unwrap().to_string();
    let conflict_koid = cc["conflict"].as_str().unwrap().to_string();
    let ako = c.call_tool("get", json!({"subject": "alice", "koid": a_koid}));
    assert_eq!(ako["extensions"]["epistemic_status"], "asserted");
    let cko = c.call_tool("get", json!({"subject": "alice", "koid": conflict_koid}));
    assert_eq!(cko["type_name"], "aikoql:conflict");
    assert_eq!(cko["extensions"]["resolution"], "unresolved");
    assert_eq!(cko["properties"]["claim_a"], a_koid.as_str());
    assert_eq!(cko["properties"]["claim_b"], counter_koid.as_str());
    // Per-assertion snapshots carry each side's authority + evidence.
    assert_eq!(
        cko["extensions"]["assertions"]["a"]["authority"],
        "source_code"
    );
    assert_eq!(
        cko["extensions"]["assertions"]["b"]["authority"],
        "documentation"
    );

    // 5. resolve_conflict_by_authority: source_code (7) beats documentation
    // (3) — the kernel ranks, the losing claim becomes Contradicted.
    let res = c.call_tool(
        "resolve_conflict_by_authority",
        json!({
            "subject": "alice",
            "koid": conflict_koid,
            "rationale": "code is ground truth"
        }),
    );
    assert_eq!(res["decision"], "resolved_a_preferred");
    assert_eq!(res["effects"].as_array().unwrap().len(), 1);
    assert_eq!(res["effects"][0]["koid"], counter_koid.as_str());
    assert_eq!(res["effects"][0]["status"], "contradicted");
    let rko = c.call_tool("get", json!({"subject": "alice", "koid": conflict_koid}));
    assert_eq!(rko["extensions"]["resolution"], "resolved_a_preferred");
    assert_eq!(
        rko["extensions"]["resolution_rationale"],
        "code is ground truth"
    );

    // 6. supersede: old preserved + Superseded, dependent swept for staleness.
    let s = c.call_tool(
        "supersede",
        json!({
            "subject": "alice",
            "old": a_koid,
            "type_name": "claim",
            "properties": {"env": "prod", "cpu": 55},
            "evidence": [{"source_artifact": "re-measure", "method": "runtime_observation"}],
            "reason": "new measurement"
        }),
    );
    assert_eq!(s["old"], a_koid.as_str());
    let new_koid = s["new"].as_str().unwrap().to_string();
    assert_eq!(s["invalidated_dependents"].as_array().unwrap().len(), 1);
    assert_eq!(s["invalidated_dependents"][0], d_koid.as_str());
    let ako = c.call_tool("get", json!({"subject": "alice", "koid": a_koid}));
    assert_eq!(ako["extensions"]["epistemic_status"], "superseded");
    assert!(ako["extensions"]["valid_to"].as_u64().is_some());
    let dko = c.call_tool("get", json!({"subject": "alice", "koid": d_koid}));
    // Dependent: stamped invalidated, epistemic status untouched.
    assert_eq!(dko["extensions"]["epistemic_status"], "inferred");
    assert!(dko["extensions"]["invalidation"].is_object());

    // 7. trace answers INVALIDATED WHEN / BY WHOM / WHY for the dependent.
    let t = c.call_tool("trace", json!({"subject": "alice", "koid": d_koid}));
    assert_eq!(t["invalidation"]["actor"], "alice");
    assert!(t["invalidation"]["at"].as_u64().is_some());
    assert!(!t["invalidation"]["reason"].as_str().unwrap().is_empty());

    // 8. merge: first-class derivation with operation "merge".
    let x = c.call_tool(
        "assert_knowledge",
        json!({
            "subject": "alice",
            "type_name": "claim",
            "properties": {"region": "us"},
            "authority": "ci_verified",
            "evidence": [{"source_artifact": "ci-log", "method": "ci_observation"}]
        }),
    );
    let x_koid = x["koid"].as_str().unwrap().to_string();
    let m = c.call_tool(
        "merge",
        json!({
            "subject": "alice",
            "type_name": "merged",
            "sources": [new_koid, x_koid],
            "strategy": "newest_wins",
            "evidence": [{"source_artifact": "merge-run", "method": "agent_analysis"}]
        }),
    );
    let m_koid = m["koid"].as_str().unwrap().to_string();
    let mko = c.call_tool("get", json!({"subject": "alice", "koid": m_koid}));
    assert_eq!(mko["extensions"]["derivation"]["operation"], "merge");
    assert_eq!(mko["properties"]["env"], "prod");
    assert_eq!(mko["properties"]["region"], "us");

    // 9. invalidate: target Contradicted + chain sweep in BFS order.
    let y = c.call_tool(
        "derive",
        json!({
            "subject": "alice",
            "type_name": "conclusion",
            "properties": {"region_is": "us"},
            "sources": [x_koid],
            "operation": "inference"
        }),
    );
    let y_koid = y["koid"].as_str().unwrap().to_string();
    let inv = c.call_tool(
        "invalidate",
        json!({
            "subject": "alice",
            "koid": x_koid,
            "evidence": [{"source_artifact": "refuting-observation", "method": "runtime_observation"}],
            "reason": "premise refuted"
        }),
    );
    // x has TWO derived dependents: y (step 9) and the merged KO m (step 8,
    // which folds x as a source) — both must be swept, plus the target.
    assert_eq!(inv["invalidated"].as_array().unwrap().len(), 3);
    assert_eq!(inv["invalidated"][0], x_koid.as_str());
    let swept: Vec<&str> = inv["invalidated"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(swept.contains(&y_koid.as_str()) && swept.contains(&m_koid.as_str()));
    let xko = c.call_tool("get", json!({"subject": "alice", "koid": x_koid}));
    assert_eq!(xko["extensions"]["epistemic_status"], "contradicted");
    assert_eq!(
        xko["extensions"]["invalidation"]["reason"],
        "premise refuted"
    );
    let yko = c.call_tool("get", json!({"subject": "alice", "koid": y_koid}));
    assert_eq!(yko["extensions"]["epistemic_status"], "inferred");
    assert!(yko["extensions"]["invalidation"].is_object());
    let mko2 = c.call_tool("get", json!({"subject": "alice", "koid": m_koid}));
    assert!(mko2["extensions"]["invalidation"].is_object());

    // 10. Anti-CRUD-cosplay at the protocol boundary: unbacked operations
    // fail (raw request — call_tool would panic on tool errors).
    for (name, arguments) in [
        (
            "observe",
            json!({"subject": "alice", "type_name": "sighting", "properties": {"temp": 1}}),
        ),
        (
            "assert_knowledge",
            json!({"subject": "alice", "type_name": "claim", "properties": {"x": 1}, "authority": "source_code"}),
        ),
        (
            "verify_knowledge",
            json!({"subject": "alice", "koid": o_koid}),
        ),
        ("invalidate", json!({"subject": "alice", "koid": m_koid})),
    ] {
        let bad = c.request("tools/call", json!({"name": name, "arguments": arguments}));
        assert_eq!(
            bad.get("isError").and_then(|b| b.as_bool()),
            Some(true),
            "{} without evidence must fail",
            name
        );
    }

    // 11. Authority tie: an explicit decision is required — never a silent
    // pick (raw request; resolve_conflict_by_authority must fail).
    let t1 = c.call_tool(
        "assert_knowledge",
        json!({
            "subject": "alice",
            "type_name": "claim",
            "properties": {"p": 1},
            "authority": "documentation",
            "evidence": [{"source_artifact": "doc-a", "method": "doc_extraction"}]
        }),
    );
    let t1_koid = t1["koid"].as_str().unwrap().to_string();
    let tc = c.call_tool(
        "contradict",
        json!({
            "subject": "alice",
            "claim": t1_koid,
            "properties": {"p": 2},
            "authority": "documentation",
            "evidence": [{"source_artifact": "doc-b", "method": "doc_extraction"}]
        }),
    );
    let tie_conflict = tc["conflict"].as_str().unwrap().to_string();
    let tie = c.request(
        "tools/call",
        json!({
            "name": "resolve_conflict_by_authority",
            "arguments": {
                "subject": "alice",
                "koid": tie_conflict,
                "rationale": "rank"
            }
        }),
    );
    assert_eq!(tie.get("isError").and_then(|b| b.as_bool()), Some(true));

    let _ = std::fs::remove_file(&db);
}

// --- v0.3 K5: Agent Experience ---

#[test]
fn k5_experience_reuse_end_to_end() {
    let db = tmp_db("k5");
    let mut c = McpClient::start(&db);
    c.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "k5", "version": "0"}}));
    c.notify("notifications/initialized");

    // 1. record_experience: evidence mandatory at the protocol boundary.
    let bad = c.request(
        "tools/call",
        json!({
            "name": "record_experience",
            "arguments": {
                "subject": "alice",
                "goal": "refactor the rust parser",
                "action": "split the lexer",
                "outcome": "tests green"
            }
        }),
    );
    assert_eq!(bad.get("isError").and_then(|b| b.as_bool()), Some(true));

    let r = c.call_tool(
        "record_experience",
        json!({
            "subject": "alice",
            "goal": "refactor the rust parser",
            "action": "split the lexer",
            "outcome": "tests green",
            "lesson": "smaller functions first",
            "reuse_conditions": ["rust", "parser"],
            "evidence": [{"source_artifact": "run-log", "method": "agent_analysis"}],
            "shared_with": ["bob"]
        }),
    );
    let e_koid = r["koid"].as_str().unwrap().to_string();
    let eko = c.call_tool("get", json!({"subject": "alice", "koid": e_koid}));
    assert_eq!(eko["type_name"], "aikoql:experience");
    assert_eq!(eko["extensions"]["epistemic_status"], "asserted");
    assert_eq!(eko["extensions"]["authority"], "agent_derived");
    assert!(eko["extensions"]["valid_to"].as_u64().is_some());
    assert_eq!(eko["extensions"]["confidence"]["score"], 0.5);

    // 2. Cross-agent reuse: bob matches only when ALL condition tokens
    // appear; a stranger with no ACL grant sees nothing.
    let m = c.call_tool(
        "find_experiences",
        json!({"subject": "bob", "task": "please refactor the rust parser again"}),
    );
    assert_eq!(m["matches"].as_array().unwrap().len(), 1);
    assert_eq!(m["matches"][0]["koid"], e_koid.as_str());
    assert_eq!(m["matches"][0]["actor"], "alice");
    let none = c.call_tool(
        "find_experiences",
        json!({"subject": "bob", "task": "refactor something else entirely"}),
    );
    assert_eq!(none["matches"].as_array().unwrap().len(), 0);
    let stranger = c.call_tool(
        "find_experiences",
        json!({"subject": "carol", "task": "please refactor the rust parser again"}),
    );
    assert_eq!(stranger["matches"].as_array().unwrap().len(), 0);

    // 3. compile_context injects the experiences section for a matching task.
    let kb = c.call_tool(
        "remember",
        json!({
            "subject": "bob",
            "type_name": "knowledge_doc",
            "properties": {"ir_json": "{\"entities\":[],\"relations\":[],\"facts\":[],\"events\":[],\"temporal\":[],\"document_id\":null,\"page_count\":0,\"extractor\":\"\"}"}
        }),
    );
    let kb_koid = kb["koid"].as_str().unwrap().to_string();
    let ctx_pkg = c.call_tool(
        "compile_context",
        json!({"subject": "bob", "koid": kb_koid, "task": "refactor the rust parser"}),
    );
    assert!(ctx_pkg["context_markdown"]
        .as_str()
        .unwrap()
        .contains("Previous Agent Experience"));
    assert_eq!(ctx_pkg["experiences"].as_array().unwrap().len(), 1);
    assert_eq!(ctx_pkg["experiences"][0]["koid"], e_koid.as_str());
    let ctx_none = c.call_tool(
        "compile_context",
        json!({"subject": "bob", "koid": kb_koid, "task": "paint the bikeshed"}),
    );
    assert_eq!(ctx_none["experiences"].as_array().unwrap().len(), 0);

    // 4. agent_memory TTL enforcement: ttl=0 is dropped at read.
    c.call_tool(
        "agent_memory",
        json!({"subject": "alice", "agent_id": "alice", "key": "gone", "value": "expired", "ttl": 0}),
    );
    c.call_tool(
        "agent_memory",
        json!({"subject": "alice", "agent_id": "alice", "key": "live", "value": "alive", "ttl": 3600}),
    );
    let mem = c.call_tool(
        "agent_memory",
        json!({"subject": "alice", "agent_id": "alice"}),
    );
    assert_eq!(mem["count"], 1);
    assert_eq!(mem["expired_dropped"], 1);
    assert_eq!(mem["memories"][0]["key"], "live");

    // 5. execute_agent captures the run as an experience (non-fatal hook).
    c.call_tool(
        "deploy_program",
        json!({
            "name": "FindEngPeople",
            "body": "MATCH Person WHERE dept == \"Eng\" RETURN name",
            "language": "aikoql",
            "subject": "tester"
        }),
    );
    let agent = c.call_tool(
        "deploy_agent",
        json!({
            "name": "HRAssistant",
            "prompt": "You help find people in the org.",
            "skills": ["FindEngPeople"],
            "tools": [],
            "policies": [],
            "subject": "tester"
        }),
    );
    let agent_koid = agent["koid"].as_str().unwrap();
    let result = c.call_tool(
        "execute_agent",
        json!({"koid": agent_koid, "subject": "tester"}),
    );
    let log = result["execution_log"].as_array().unwrap();
    let log_text = log
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        log_text.contains("experience captured:"),
        "run outcome should be captured, got: {}",
        log_text
    );
    // The capture is visible to the executor as a reusable experience.
    let own = c.call_tool(
        "find_experiences",
        json!({"subject": "tester", "task": "find people in the org"}),
    );
    assert_eq!(own["matches"].as_array().unwrap().len(), 1);
    assert_eq!(own["matches"][0]["actor"], "tester");

    let _ = std::fs::remove_file(&db);
}

#[test]
fn m_ret1_remember_tool_retention_expiry() {
    // RET-CHAT-001 acceptance: create temporary memory with short retention;
    // expected automatic expiry according to policy.
    let db = tmp_db("retention");
    let mut c = McpClient::start(&db);

    // temporary: retention_ms 0 => zero-duration interval, expired on arrival
    let temp = c.call_tool(
        "remember",
        json!({
            "subject": "alice",
            "type_name": "chat_note",
            "properties": {"text": "the wifi password is hunter2"},
            "retention_ms": 0
        }),
    );
    let temp_koid = temp["koid"].as_str().unwrap().to_string();
    // control: no retention declaration => permanent
    c.call_tool(
        "remember",
        json!({
            "subject": "alice",
            "type_name": "chat_note",
            "properties": {"text": "alice prefers tea"}
        }),
    );

    // default-time query answers with current truth: only the permanent note
    let q = c.call_tool(
        "aikoql",
        json!({"subject": "alice", "query": "MATCH chat_note RETURN *"}),
    );
    let rows = q["results"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["properties"]["text"], "alice prefers tea");

    // expired memory is not erased: still fetchable via get (audit/lineage)
    let got = c.call_tool("get", json!({"subject": "alice", "koid": temp_koid}));
    assert_eq!(got["koid"], temp_koid);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn m_evp1_evidence_pack_bundles_compliance_evidence() {
    // MRFC-0020 Phase 4 acceptance (IMPLEMENTATION-PLAN "Next
    // implementation"): one auditor export bundles the audit chain, the
    // object inventory, the PII-filtering config, the retention records,
    // and the encryption compliance report. Golden dataset: the m_ret1
    // retention shapes — an expired-on-arrival window (retention_ms 0,
    // valid_to == its own write instant, expired under the half-open
    // interval), a live one-day window, and a permanent control.
    let db = tmp_db("evidence_pack");
    let mut c = McpClient::start(&db);

    c.call_tool(
        "remember",
        json!({
            "subject": "alice",
            "type_name": "chat_note",
            "properties": {"text": "the wifi password is hunter2"},
            "retention_ms": 0
        }),
    );
    c.call_tool(
        "remember",
        json!({
            "subject": "alice",
            "type_name": "chat_note",
            "properties": {"text": "alice prefers tea"},
            "retention_ms": 86_400_000
        }),
    );
    c.call_tool(
        "remember",
        json!({"subject": "alice", "type_name": "chat_note", "properties": {"text": "permanent"}}),
    );

    for framework in ["gdpr", "hipaa"] {
        let pack = c.call_tool("evidence_pack", json!({"framework": framework}));
        assert_eq!(pack["framework"], framework);
        // Audit chain + inventory (audit_report substrate).
        assert!(!pack["audit_chain"].as_str().unwrap().is_empty());
        assert!(pack["journal_seq"].as_u64().unwrap() >= 3);
        assert!(pack["object_inventory"]["total"].as_u64().unwrap() >= 3);
        // Fresh remembers land as Draft (kernel default lifecycle).
        assert!(
            pack["object_inventory"]["by_state"]["draft"]
                .as_u64()
                .unwrap()
                >= 3
        );
        // PII filtering config (MRFC-0070 A7 substrate): the detector
        // capability statement plus the known-limits honesty note.
        assert_eq!(pack["pii_filtering"]["active"], true);
        let kinds = pack["pii_filtering"]["detector_kinds"].as_array().unwrap();
        assert!(kinds.iter().any(|k| k == "API_KEY"));
        assert!(kinds.iter().any(|k| k == "EMAIL"));
        assert!(kinds.iter().any(|k| k == "CREDIT_CARD"));
        assert!(!pack["pii_filtering"]["known_limits"]
            .as_str()
            .unwrap()
            .is_empty());
        // Retention records: 2 stamped windows — one live, one
        // expired-on-arrival; purge coverage stated honestly (counted,
        // deletion is caller-side — no kernel purge op exists).
        assert_eq!(pack["retention"]["retained_objects"].as_u64().unwrap(), 2);
        assert_eq!(pack["retention"]["live_windows"].as_u64().unwrap(), 1);
        assert_eq!(pack["retention"]["expired"].as_u64().unwrap(), 1);
        assert!(!pack["retention"]["purge_coverage"]
            .as_str()
            .unwrap()
            .is_empty());
        // Encryption substrate: fresh store, no field crypto → grade C.
        assert_eq!(pack["encryption"]["compliance_grade"], "C");
    }

    // Unknown framework is refused, not silently relabelled.
    let err = c.request(
        "tools/call",
        json!({"name": "evidence_pack", "arguments": {"framework": "pci"}}),
    );
    assert_eq!(err.get("isError").and_then(|b| b.as_bool()), Some(true));
    assert!(err["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("framework"));

    let _ = std::fs::remove_file(&db);
}

#[test]
fn m_sum1_summarize_conversation_tool() {
    // §38–39 acceptance: a conversation is summarized into the seven
    // buckets; every item traces to speaker + message range + timestamp.
    let db = tmp_db("summarize");
    let mut c = McpClient::start(&db);

    let out = c.call_tool(
        "summarize_conversation",
        json!({
            "subject": "alice",
            "conversation_id": "conv-9",
            "messages": [
                {"speaker": "alice", "ts_ms": 1000, "text": "We decided to launch on Tuesday."},
                {"speaker": "bob", "ts_ms": 2000, "text": "The rollout must include backups."},
                {"speaker": "alice", "ts_ms": 3000, "text": "The smoke test passed."}
            ],
            "evidence": [{"source_artifact": "chat-export.json", "method": "doc_extraction"}]
        }),
    );
    assert_eq!(out["type_name"], "aikoql:conversation_summary");
    assert_eq!(out["message_count"], 3);
    let koid = out["koid"].as_str().unwrap().to_string();

    let ko = c.call_tool("get", json!({"subject": "alice", "koid": koid}));
    let p = &ko["properties"];
    assert_eq!(p["conversation_id"], "conv-9");
    assert_eq!(p["message_count"], 3);
    assert_eq!(p["decisions"].as_array().unwrap().len(), 1);
    assert_eq!(p["constraints"].as_array().unwrap().len(), 1);
    assert_eq!(p["outcomes"].as_array().unwrap().len(), 1);

    // §39: provenance per item — speaker, message range, timestamp
    let d = &p["decisions"][0];
    assert_eq!(d["speaker"], "alice");
    assert_eq!(d["msg_range"], json!([0, 0]));
    assert_eq!(d["ts_ms"], 1000);
    let con = &p["constraints"][0];
    assert_eq!(con["speaker"], "bob");
    assert_eq!(con["msg_range"], json!([1, 1]));
    assert_eq!(con["ts_ms"], 2000);

    let _ = std::fs::remove_file(&db);
}
