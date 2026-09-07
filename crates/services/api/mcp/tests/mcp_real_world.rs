//! Real-World MCP Integration Test — exercises the full product as an AI agent would.
//!
//! This test:
//! 1. Starts the MCP server in stdio mode
//! 2. Sends JSON-RPC requests simulating an agent workflow
//! 3. Verifies every response
//! 4. Tests CRUD → Search → Graph → Programs → Policies → Backup → Audit
//!
//! ponytail: one comprehensive test that validates the entire surface.

use serde_json::{json, Value as J};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use aikoql_ingestion::{EntityCandidate, Evidence, FactCandidate, KnowledgeIr, RelationCandidate};

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

fn tmp_db(suffix: &str) -> String {
    let p = std::env::temp_dir().join(format!("mcp-{suffix}-{}.redb", std::process::id()));
    let _ = std::fs::remove_file(&p);
    TEMP_PATHS.with(|t| t.borrow_mut().paths.push(p.clone()));
    p.to_string_lossy().into_owned()
}

struct McpClient {
    child: Child,
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl McpClient {
    fn start(db_path: &str) -> Self {
        // Find binary relative to workspace root.
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
        // Prefer the freshest build — otherwise a stale release binary runs
        // old code and integration tests silently test the wrong version.
        let newest = |a: &std::path::Path, b: &std::path::Path| -> bool {
            let m = |p: &std::path::Path| {
                p.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH)
            };
            m(a) >= m(b)
        };
        let bin = match (release_bin.exists(), debug_bin.exists()) {
            (true, true) => {
                if newest(&debug_bin, &release_bin) {
                    debug_bin
                } else {
                    release_bin
                }
            }
            (true, false) => release_bin,
            _ => debug_bin,
        };
        eprintln!("Using binary: {}", bin.display());
        let mut child = Command::new(&bin)
            .arg("serve")
            .arg(db_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // crash output lands in CI logs, not /dev/null
            .spawn()
            .expect("start MCP server");

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let reader = BufReader::new(stdout);

        McpClient {
            child,
            stdin,
            reader,
            next_id: 1,
        }
    }

    fn call(&mut self, tool: &str, args: &J) -> J {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": tool,
                "arguments": args
            }
        });
        let line = serde_json::to_string(&req).unwrap() + "\n";
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.flush().unwrap();

        let mut response = String::new();
        self.reader.read_line(&mut response).unwrap();
        let v: J = serde_json::from_str(&response).unwrap();
        if let Some(err) = v.get("error") {
            panic!("MCP error for {}: {:?}", tool, err);
        }
        // Parse the content[0].text as JSON.
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap_or_else(|_| json!({"raw": text}))
    }

    fn call_raw(&mut self, tool: &str, args: &J) -> J {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": args}
        });
        self.stdin
            .write_all((serde_json::to_string(&req).unwrap() + "\n").as_bytes())
            .unwrap();
        self.stdin.flush().unwrap();
        let mut response = String::new();
        self.reader.read_line(&mut response).unwrap();
        serde_json::from_str(&response).unwrap()
    }

    /// Establish session identity (R9): subsequent tool calls inherit the
    /// agent_id, roles, and tenant scope until the next session/init.
    fn session_init(&mut self, agent_id: &str, tenant: &str) -> J {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0", "id": id, "method": "session/init",
            "params": {"agent_id": agent_id, "tenant": tenant}
        });
        self.stdin
            .write_all((serde_json::to_string(&req).unwrap() + "\n").as_bytes())
            .unwrap();
        self.stdin.flush().unwrap();
        let mut response = String::new();
        self.reader.read_line(&mut response).unwrap();
        serde_json::from_str(&response).unwrap()
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        // Wait for the process to fully exit: the child holds the redb
        // exclusive flock, and a respawn on the same db before the OS tears
        // it down fails to open and dies before responding (EOF flake under
        // parallel load).
        let _ = self.child.wait();
    }
}

#[test]
fn real_world_agent_workflow() {
    let db = tmp_db("rw");
    let _ = std::fs::remove_file(&db);
    let mut c = McpClient::start(&db);

    // ── Phase 1: Knowledge CRUD ──────────────────────────────────────────

    // Create employee objects.
    let alice = c.call("remember", &json!({
        "subject": "admin", "type_name": "Employee", "tenant": "acme",
        "properties": {"name": "Alice Chen", "dept": "Engineering", "salary": 165000, "level": "L6"},
        "tags": ["engineering", "senior"]
    }));
    let alice_koid = alice["koid"].as_str().unwrap().to_string();
    assert_eq!(alice["version"], 1);

    let bob = c.call("remember", &json!({
        "subject": "admin", "type_name": "Employee", "tenant": "acme",
        "properties": {"name": "Bob Martinez", "dept": "Design", "salary": 130000, "level": "L5"},
        "tags": ["design"]
    }));
    let bob_koid = bob["koid"].as_str().unwrap().to_string();

    let carol = c.call("remember", &json!({
        "subject": "admin", "type_name": "Employee", "tenant": "beta",
        "properties": {"name": "Carol Wu", "dept": "Engineering", "salary": 175000, "level": "L6"},
        "tags": ["engineering", "lead"]
    }));
    let carol_koid = carol["koid"].as_str().unwrap().to_string();

    let proj = c.call("remember", &json!({
        "subject": "admin", "type_name": "Project", "tenant": "acme",
        "properties": {"title": "Aikoql Core", "status": "active", "priority": 1, "budget": 500000.0}
    }));
    let _proj_koid = proj["koid"].as_str().unwrap().to_string();

    // ── Phase 2: Read + Verify ──────────────────────────────────────────

    let fetched = c.call("get", &json!({"koid": &alice_koid, "subject": "admin"}));
    assert_eq!(fetched["type_name"], "Employee");
    assert_eq!(fetched["properties"]["name"], "Alice Chen");

    // ── Phase 3: Graph Relationships ─────────────────────────────────────

    let rel1 = c.call(
        "relate",
        &json!({
            "subject": "admin", "from": &alice_koid, "to": &bob_koid, "rel_type": "knows"
        }),
    );
    assert!(rel1["koid"].as_str().is_some());

    c.call(
        "relate",
        &json!({
            "subject": "admin", "from": &alice_koid, "to": &carol_koid, "rel_type": "collaborates"
        }),
    );

    // Traverse from Alice.
    let hits = c.call(
        "traverse",
        &json!({
            "subject": "admin", "koid": &alice_koid, "depth": 1, "direction": "outbound"
        }),
    );
    // Should find both Bob and Carol.
    assert!(hits["hits"].as_array().unwrap().len() >= 2);

    // ── Phase 4: Search ──────────────────────────────────────────────────

    let found = c.call(
        "find_similar",
        &json!({
            "subject": "admin", "type_name": "Employee", "text": "engineering lead", "k": 5
        }),
    );
    assert!(!found["results"].as_array().unwrap().is_empty());

    // ── Phase 5: Aikoql Query ────────────────────────────────────────────

    let query = "MATCH Employee WHERE dept == \"Engineering\" RETURN *".to_string();
    let results = c.call("aikoql", &json!({"query": query, "subject": "admin"}));
    assert!(results["results"].as_array().unwrap().len() >= 2);

    // ── Phase 6: Programs-as-KOs ─────────────────────────────────────────

    let prog = c.call(
        "deploy_program",
        &json!({
            "subject": "admin", "name": "FindEngineers",
            "body": "MATCH Employee WHERE dept == \"Engineering\" RETURN *",
            "language": "aikoql"
        }),
    );
    let prog_koid = prog["koid"].as_str().unwrap().to_string();

    let exec = c.call(
        "execute_program",
        &json!({
            "subject": "admin", "roles": ["admin"], "koid": &prog_koid
        }),
    );
    assert!(exec["count"].as_u64().unwrap() >= 2);

    // List programs.
    let programs = c.call("list_programs", &json!({"subject": "admin"}));
    assert!(!programs["programs"].as_array().unwrap().is_empty());

    // ── Phase 7: Policy-as-KO ────────────────────────────────────────────

    c.call(
        "deploy_policy",
        &json!({
            "subject": "admin", "name": "HRReadEmployee", "effect": "Allow",
            "principal": "hr-team", "action": "Read", "resource_type": "Employee"
        }),
    );

    let eval = c.call("evaluate_policies", &json!({
        "subject": "admin", "principal": "hr-team", "action": "Read", "resource_type": "Employee"
    }));
    assert_eq!(eval["allowed"], true);

    let deny_eval = c.call(
        "evaluate_policies",
        &json!({
            "subject": "admin", "principal": "intern", "action": "Read", "resource_type": "Employee"
        }),
    );
    // intern has no policy — should not be allowed.
    assert_eq!(deny_eval["allowed"], false);

    // ── Phase 8: Workflow ────────────────────────────────────────────────

    let wf = c.call(
        "deploy_workflow",
        &json!({
            "subject": "admin", "name": "TeamReport",
            "steps": [{"order": 1, "program": "FindEngineers"}]
        }),
    );
    let wf_koid = wf["koid"].as_str().unwrap().to_string();

    let wf_exec = c.call(
        "execute_workflow",
        &json!({
            "subject": "admin", "koid": &wf_koid
        }),
    );
    assert_eq!(wf_exec["executed"], true);

    // ── Phase 9: Backup + Audit ──────────────────────────────────────────

    let backup = c.call_raw("backup", &json!({"subject": "admin"})).clone();
    // Result may have been successful even if backup dir exists.
    assert!(backup["result"].is_object());

    let audit = c.call("audit_report", &json!({}));
    assert!(audit["total_objects"].as_u64().unwrap() >= 4);
    assert!(!audit["audit_chain"].as_str().unwrap().is_empty());

    // ── Phase 10: ABI Version ────────────────────────────────────────────

    let abi = c.call("abi_version", &json!({}));
    assert_eq!(abi["abi_version"], 1);
    assert_eq!(abi["audit_chain_exportable"], true);

    // ── Phase 11: Metrics ────────────────────────────────────────────────

    let metrics = c.call("metrics", &json!({}));
    assert!(metrics["journal_seq"].as_u64().unwrap() > 0);
    assert!(metrics["total_objects"].as_u64().unwrap() >= 4);

    // ── Phase 12: Multi-Tenancy (R9) ─────────────────────────────────────

    // The SAME principal "admin" owns both notes — only the tenant differs,
    // so any cross-visibility here is a tenant-confinement failure, not an
    // ACL failure. Session identity carries the tenant into every tool call.
    let init = c.session_init("admin", "acme");
    assert_eq!(init["result"]["established"], true);

    let acme_note = c.call(
        "remember",
        &json!({"type_name": "note", "properties": {"body": "acme quarterly report", "memo": "acme"}}),
    );
    let acme_koid = acme_note["koid"].as_str().unwrap().to_string();

    c.session_init("admin", "beta");
    let beta_note = c.call(
        "remember",
        &json!({"type_name": "note", "properties": {"body": "beta launch plan", "memo": "beta"}}),
    );
    let beta_koid = beta_note["koid"].as_str().unwrap().to_string();

    // Scoped to beta: recall sees only beta's note.
    let beta_sim = c.call(
        "find_similar",
        &json!({"type_name": "note", "text": "launch plan", "k": 10}),
    );
    let beta_koids: Vec<&str> = beta_sim["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["koid"].as_str())
        .collect();
    assert!(
        beta_koids.contains(&beta_koid.as_str()),
        "beta's own note must be visible: {beta_koids:?}"
    );
    assert!(
        !beta_koids.contains(&acme_koid.as_str()),
        "acme's note leaked into beta's recall: {beta_koids:?}"
    );

    // Cross-tenant point read denied even though admin owns the object.
    // Tool errors surface as an isError result carrying the message.
    let cross = c.call_raw("get", &json!({"koid": &acme_koid}));
    let cross_text = cross["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        cross["result"]["isError"] == true && cross_text.contains("ACCESS_DENIED"),
        "cross-tenant get must be denied: {cross}"
    );

    // Scoped to acme: recall sees only acme's note.
    c.session_init("admin", "acme");
    let acme_sim = c.call(
        "find_similar",
        &json!({"type_name": "note", "text": "report", "k": 10}),
    );
    let acme_koids: Vec<&str> = acme_sim["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["koid"].as_str())
        .collect();
    assert!(
        acme_koids.contains(&acme_koid.as_str()),
        "acme's own note must be visible: {acme_koids:?}"
    );
    assert!(
        !acme_koids.contains(&beta_koid.as_str()),
        "beta's note leaked into acme's recall: {acme_koids:?}"
    );

    // MATCH (aikoql) rides the same scoped path.
    let acme_match = c.call("aikoql", &json!({"query": "MATCH note RETURN *"}));
    let match_koids: Vec<&str> = acme_match["results"]
        .as_array()
        .map(|a| a.iter().filter_map(|o| o["koid"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        match_koids.contains(&acme_koid.as_str()),
        "MATCH should return acme's note: {match_koids:?}"
    );
    assert!(
        !match_koids.contains(&beta_koid.as_str()),
        "MATCH leaked beta's note: {match_koids:?}"
    );

    let _ = std::fs::remove_file(&db);
}

/// §51 Critical End-to-End Scenario (chatbot suite, certification G5):
/// deterministic scripted replay over the real MCP surface with mechanical
/// judges (PR-R pattern — the script is the "LLM", asserts are the judges).
///
/// Scenario beats: initial conversation → durable memories with provenance
/// and scope → later recall ("AWS") → authoritative org update supersedes
/// the preference ("Azure", with supersession evidence) → "Deploy it." runs
/// the Program-as-KO pipeline (identity → permissions → policy → execute →
/// postconditions → episode).
#[test]
fn critical_e2e_scenario_51_chatbot_memory() {
    let db = tmp_db("s51");
    let _ = std::fs::remove_file(&db);
    let mut c = McpClient::start(&db);
    c.session_init("chatbot-user", "acme");

    // ── §51.1 Initial conversation → three durable memories ───────────────
    // Evidence-backed user statements enter as assertions (evidence is
    // mandatory there and stamped by the kernel); plain identity data uses
    // remember. Both must survive round-trip with provenance intact.
    let style = c.call(
        "assert_knowledge",
        &json!({
            "subject": "chatbot-user", "type_name": "UserPreference", "tenant": "acme",
            "properties": {"topic": "response style", "value": "concise"},
            "authority": "human_approved",
            "evidence": [{"source_artifact": "chat-message-1", "method": "human_provided"}]
        }),
    );
    assert_eq!(style["version"], 1);

    let acct = c.call(
        "remember",
        &json!({
            "subject": "chatbot-user", "type_name": "AccountInfo", "tenant": "acme",
            "properties": {"account": "ACME-123"},
            "origin": "human"
        }),
    );
    assert_eq!(acct["version"], 1);

    let aws = c.call("assert_knowledge", &json!({
        "subject": "chatbot-user", "type_name": "DeploymentPreference", "tenant": "acme",
        "properties": {"account": "ACME-123", "cloud": "AWS"},
        "authority": "human_approved",
        "evidence": [{"source_artifact": "chat-message-3", "method": "human_provided", "confidence": 0.95}]
    }));
    let aws_koid = aws["koid"].as_str().unwrap().to_string();

    // Memory carries provenance + scope to the query boundary.
    let aws_ko = c.call(
        "get",
        &json!({"subject": "chatbot-user", "koid": &aws_koid}),
    );
    assert_eq!(aws_ko["type_name"], "DeploymentPreference");
    assert_eq!(aws_ko["properties"]["cloud"], "AWS");
    assert_eq!(aws_ko["extensions"]["authority"], "human_approved");
    assert_eq!(
        aws_ko["extensions"]["scope"], "session",
        "the kernel stamps an explicit scope for agent-mediated claims: {aws_ko}"
    );
    assert_eq!(aws_ko["extensions"]["epistemic_status"], "asserted");
    assert!(
        aws_ko["extensions"]["evidence"]
            .to_string()
            .contains("chat-message-3"),
        "provenance evidence must survive to the query boundary: {}",
        aws_ko["extensions"]["evidence"]
    );

    // ── §51.2 Later conversation: recall with correct provenance/scope ────
    // "What do you know about my deployment setup?" → the remembered AWS.
    let recall = c.call(
        "aikoql",
        &json!({
            "subject": "chatbot-user",
            "query": "MATCH DeploymentPreference WHERE account == \"ACME-123\" RETURN *"
        }),
    );
    let clouds: Vec<String> = recall["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["properties"]["cloud"].as_str().map(String::from))
        .collect();
    assert!(
        clouds.contains(&"AWS".to_string()),
        "recall must return the remembered deployment preference: {clouds:?}"
    );

    // ── §51.3 Authoritative org update supersedes the preference ──────────
    // Ingest the organization directive as an assertion carrying
    // organization_policy authority, then supersede the user preference
    // with it.
    let directive = c.call("assert_knowledge", &json!({
        "subject": "chatbot-user", "type_name": "DeploymentDirective", "tenant": "acme",
        "properties": {"account": "ACME-123", "cloud": "Azure"},
        "authority": "organization_policy",
        "evidence": [{"source_artifact": "org-policy-v2", "method": "human_provided", "confidence": 1.0}],
        "note": "ACME-123 must now deploy on Azure"
    }));
    let directive_koid = directive["koid"].as_str().unwrap().to_string();
    let directive_ko = c.call(
        "get",
        &json!({"subject": "chatbot-user", "koid": &directive_koid}),
    );
    assert_eq!(
        directive_ko["extensions"]["authority"], "organization_policy",
        "the org directive must carry organization-policy authority: {directive_ko}"
    );

    let sup = c.call("supersede", &json!({
        "subject": "chatbot-user",
        "old": &aws_koid,
        "superseded_by": &directive_koid,
        "reason": "Organization policy supersedes the previous preference: ACME-123 must deploy on Azure",
        "evidence": [{"source_artifact": "org-policy-v2", "method": "human_provided"}]
    }));
    assert_eq!(sup["new"], directive_koid);

    // The old preference is temporally closed, still readable, and links to
    // its successor — the supersession explanation is durable knowledge.
    let aws_after = c.call(
        "get",
        &json!({"subject": "chatbot-user", "koid": &aws_koid}),
    );
    assert_eq!(
        aws_after["properties"]["cloud"], "AWS",
        "superseded knowledge stays readable (temporal)"
    );
    assert_eq!(aws_after["extensions"]["epistemic_status"], "superseded");
    assert!(
        aws_after["relationships"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["target"] == directive_koid),
        "superseded preference must link to its successor: {}",
        aws_after["relationships"]
    );
    assert!(
        aws_after["extensions"]["epistemic_history"]
            .to_string()
            .contains("Organization policy supersedes"),
        "supersession reason must be recorded: {}",
        aws_after["extensions"]["epistemic_history"]
    );
    assert!(
        aws_after["extensions"]["evidence"]
            .to_string()
            .contains("org-policy-v2"),
        "supersession evidence must append to the old claim, never disappear"
    );

    // "Where should I deploy now?" → the org directive, with org authority.
    let now = c.call(
        "aikoql",
        &json!({
            "subject": "chatbot-user",
            "query": "MATCH DeploymentDirective WHERE account == \"ACME-123\" RETURN *"
        }),
    );
    let targets: Vec<String> = now["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["properties"]["cloud"].as_str().map(String::from))
        .collect();
    assert!(
        targets.contains(&"Azure".to_string()),
        "current deployment target must be Azure: {targets:?}"
    );

    // ── §51.4 "Deploy it." — the Program-as-KO action pipeline ────────────
    // Resolve Program-as-KO: the deployment program reads the current
    // directive from knowledge (no hardcoded target).
    let prog = c.call(
        "deploy_program",
        &json!({
            "subject": "chatbot-user",
            "name": "DeployToCloud",
            "body": "MATCH DeploymentDirective WHERE account == \"ACME-123\" RETURN *",
            "language": "aikoql"
        }),
    );
    let prog_koid = prog["koid"].as_str().unwrap().to_string();
    let prog_ko = c.call(
        "get",
        &json!({"subject": "chatbot-user", "koid": &prog_koid}),
    );
    assert_eq!(prog_ko["type_name"], "aikoql:program");

    // Check permissions + policy: Allow for the bot principal, deny for
    // anyone else (the approval gate where a human would be asked).
    c.call(
        "deploy_policy",
        &json!({
            "subject": "chatbot-user", "name": "BotMayDeploy", "effect": "Allow",
            "principal": "chatbot-user", "action": "Write", "resource_type": "DeploymentDirective"
        }),
    );
    let allow = c.call(
        "evaluate_policies",
        &json!({
            "subject": "chatbot-user", "principal": "chatbot-user",
            "action": "Write", "resource_type": "DeploymentDirective"
        }),
    );
    assert_eq!(
        allow["allowed"], true,
        "deploy policy must allow the bot: {allow}"
    );
    let deny = c.call(
        "evaluate_policies",
        &json!({
            "subject": "chatbot-user", "principal": "other-bot",
            "action": "Write", "resource_type": "DeploymentDirective"
        }),
    );
    assert_eq!(
        deny["allowed"], false,
        "non-authorized principal must be denied: {deny}"
    );

    // Execute under the caller's identity.
    let exec = c.call(
        "execute_program",
        &json!({
            "subject": "chatbot-user", "roles": ["chatbot-user"], "koid": &prog_koid
        }),
    );
    assert_eq!(
        exec["count"], 1,
        "program must resolve exactly one deployment target: {exec}"
    );
    assert_eq!(
        exec["results"][0]["properties"]["cloud"], "Azure",
        "postcondition: the executed deployment targets the org-mandated cloud"
    );

    // Record the episode: goal → action → outcome, with preconditions.
    let ep = c.call(
        "record_experience",
        &json!({
            "subject": "chatbot-user",
            "goal": "Deploy ACME-123",
            "action": "execute DeployToCloud",
            "outcome": "success",
            "preconditions": ["policy BotMayDeploy allowed"],
            "lesson": "deployment target resolved from the org directive",
            "evidence": [{"source_artifact": "exec-run-1", "method": "runtime_observation"}]
        }),
    );
    let ep_koid = ep["koid"].as_str().unwrap().to_string();
    let ep_ko = c.call("get", &json!({"subject": "chatbot-user", "koid": &ep_koid}));
    assert_eq!(ep_ko["type_name"], "aikoql:experience");
    assert_eq!(ep_ko["properties"]["actor"], "chatbot-user");
    assert_eq!(ep_ko["properties"]["goal"], "Deploy ACME-123");
    assert_eq!(ep_ko["properties"]["outcome"], "success");
    assert_eq!(
        ep_ko["properties"]["preconditions"][0],
        "policy BotMayDeploy allowed"
    );

    let _ = std::fs::remove_file(&db);
}

/// G6 — Chatbot Memory Certification Scenarios (TP-3b): scripted replay of
/// the chatbot suite's conversation-level scenarios — §8 CHAT-MEM-001..005
/// (same-session, cross-session, restart persistence, explicit remember,
/// ephemeral non-conversion), §9 CLASS-001..005 (fact/preference/episode/
/// procedure/program classification), §11 PERS-001..004 (behavior change,
/// explainability, conflict resolution, scope confinement) — over the real
/// MCP surface with mechanical judges (PR-R pattern).
#[test]
fn chatbot_memory_certification_scenarios() {
    let db = tmp_db("cmem");
    let _ = std::fs::remove_file(&db);
    let mut c = McpClient::start(&db);
    c.session_init("chatbot-user", "acme");

    // ── §8 CHAT-MEM-001: same-session preference recall ────────────────────
    // "I prefer responses in English." → an asserted UserPreference.
    let lang = c.call(
        "assert_knowledge",
        &json!({
            "subject": "chatbot-user", "type_name": "UserPreference", "tenant": "acme",
            "properties": {"topic": "preferred language", "value": "English"},
            "authority": "human_approved",
            "evidence": [{"source_artifact": "chat-msg-lang", "method": "human_provided"}]
        }),
    );
    assert_eq!(lang["version"], 1);
    let recall_lang = c.call(
        "aikoql",
        &json!({
            "subject": "chatbot-user",
            "query": "MATCH UserPreference WHERE topic == \"preferred language\" RETURN *"
        }),
    );
    let lang_values: Vec<String> = recall_lang["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["properties"]["value"].as_str().map(String::from))
        .collect();
    assert!(
        lang_values.contains(&"English".to_string()),
        "CHAT-MEM-001: same-session preference must be recallable: {lang_values:?}"
    );

    // ── CHAT-MEM-002: cross-session recall ─────────────────────────────────
    // "I prefer concise answers." → remembered in conversation 1 …
    c.call(
        "assert_knowledge",
        &json!({
            "subject": "chatbot-user", "type_name": "UserPreference", "tenant": "acme",
            "properties": {"topic": "response style", "value": "concise"},
            "authority": "human_approved",
            "evidence": [{"source_artifact": "chat-msg-style", "method": "human_provided", "confidence": 0.9}]
        }),
    );
    // … available again in conversation 2 (fresh session, same identity).
    c.session_init("chatbot-user", "acme");
    let recall_style = c.call(
        "aikoql",
        &json!({
            "subject": "chatbot-user",
            "query": "MATCH UserPreference WHERE topic == \"response style\" RETURN *"
        }),
    );
    let style_values: Vec<String> = recall_style["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["properties"]["value"].as_str().map(String::from))
        .collect();
    assert!(
        style_values.contains(&"concise".to_string()),
        "CHAT-MEM-002: cross-session recall must find the preference: {style_values:?}"
    );

    // ── CHAT-MEM-003: persistence across server restart ────────────────────
    // Kill the server, reopen the same database, ask again.
    drop(c);
    let mut c = McpClient::start(&db);
    c.session_init("chatbot-user", "acme");
    let after_restart = c.call(
        "aikoql",
        &json!({
            "subject": "chatbot-user",
            "query": "MATCH UserPreference RETURN *"
        }),
    );
    let after_restart_values: Vec<String> = after_restart["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["properties"]["value"].as_str().map(String::from))
        .collect();
    assert!(
        after_restart_values.contains(&"English".to_string())
            && after_restart_values.contains(&"concise".to_string()),
        "CHAT-MEM-003: preferences must survive a full restart: {after_restart_values:?}"
    );

    // ── CHAT-MEM-004: explicit "Remember that …" → durable candidate ───────
    // "Remember that my preferred deployment environment is AWS."
    let aws = c.call("assert_knowledge", &json!({
        "subject": "chatbot-user", "type_name": "DeploymentPreference", "tenant": "acme",
        "properties": {"account": "ACME-123", "cloud": "AWS"},
        "authority": "human_approved",
        "evidence": [{"source_artifact": "chat-msg-4", "method": "human_provided", "confidence": 0.95}]
    }));
    let aws_koid = aws["koid"].as_str().unwrap().to_string();
    let aws_ko = c.call(
        "get",
        &json!({"subject": "chatbot-user", "koid": &aws_koid}),
    );
    assert_eq!(aws_ko["extensions"]["authority"], "human_approved");
    assert_eq!(aws_ko["extensions"]["epistemic_status"], "asserted");
    assert!(
        aws_ko["extensions"]["evidence"]
            .to_string()
            .contains("chat-msg-4"),
        "CHAT-MEM-004: explicit remember must keep its evidence: {}",
        aws_ko["extensions"]["evidence"]
    );

    // ── CHAT-MEM-005: ephemeral statements are NOT auto-converted ──────────
    // "I am currently testing this on AWS." → an observation (status
    // "observed", non-assertive channel) — classification is the chatbot's
    // job; the substrate must not silently promote it to a preference.
    let obs = c.call(
        "observe",
        &json!({
            "subject": "chatbot-user", "type_name": "UserStatement", "tenant": "acme",
            "properties": {"environment": "AWS", "stage": "testing"},
            "evidence": [{"source_artifact": "chat-msg-5", "method": "human_provided"}]
        }),
    );
    let obs_koid = obs["koid"].as_str().unwrap().to_string();
    let obs_ko = c.call(
        "get",
        &json!({"subject": "chatbot-user", "koid": &obs_koid}),
    );
    assert_eq!(
        obs_ko["extensions"]["epistemic_status"], "observed",
        "ephemeral statement must be stamped observed, not asserted: {obs_ko}"
    );
    let prefs_after_ephemeral = c.call(
        "aikoql",
        &json!({"subject": "chatbot-user", "query": "MATCH UserPreference RETURN *"}),
    );
    let pref_blobs: Vec<String> = prefs_after_ephemeral["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["properties"].to_string())
        .collect();
    assert!(
        !pref_blobs.iter().any(|p| p.contains("testing")),
        "CHAT-MEM-005: the ephemeral statement must not become a preference: {pref_blobs:?}"
    );

    // ── §9 CLASS: classification into memory types ─────────────────────────
    // CLASS-001: "My company is ACME." → semantic fact.
    let fact = c.call(
        "assert_knowledge",
        &json!({
            "subject": "chatbot-user", "type_name": "SemanticFact", "tenant": "acme",
            "properties": {"subject": "user company", "predicate": "is", "object": "ACME"},
            "authority": "human_approved",
            "evidence": [{"source_artifact": "chat-msg-6", "method": "human_provided"}]
        }),
    );
    let fact_ko = c.call(
        "get",
        &json!({"subject": "chatbot-user", "koid": fact["koid"].as_str().unwrap()}),
    );
    assert_eq!(fact_ko["type_name"], "SemanticFact");
    assert_eq!(fact_ko["properties"]["object"], "ACME");

    // CLASS-002: preference → UserPreference KO (the concise one, §8).
    let style_ko = c.call(
        "aikoql",
        &json!({"subject": "chatbot-user", "query": "MATCH UserPreference WHERE topic == \"response style\" RETURN *"}),
    );
    let style_koid = style_ko["results"][0]["koid"].as_str().unwrap().to_string();
    let style_ko = c.call(
        "get",
        &json!({"subject": "chatbot-user", "koid": &style_koid}),
    );
    assert_eq!(style_ko["type_name"], "UserPreference");

    // CLASS-003: "Yesterday I deployed ACME-123." → episodic memory.
    let ep = c.call(
        "record_experience",
        &json!({
            "subject": "chatbot-user",
            "goal": "Deploy ACME-123 yesterday",
            "action": "ran the deployment pipeline",
            "outcome": "success",
            "preconditions": [],
            "evidence": [{"source_artifact": "chat-msg-7", "method": "human_provided"}]
        }),
    );
    let ep_ko = c.call(
        "get",
        &json!({"subject": "chatbot-user", "koid": ep["koid"].as_str().unwrap()}),
    );
    assert_eq!(ep_ko["type_name"], "aikoql:experience");

    // CLASS-004: "To reset an account: …" → procedural memory. Procedural
    // knowledge is an experience KO carrying reuse_conditions (there is no
    // separate aikoql:procedure type).
    let proc = c.call(
        "record_experience",
        &json!({
            "subject": "chatbot-user",
            "goal": "Reset an account",
            "action": "verify identity, then reset password",
            "outcome": "account reset",
            "preconditions": ["user verified identity"],
            "lesson": "always verify identity before resetting",
            "reuse_conditions": ["account reset request"],
            "evidence": [{"source_artifact": "chat-msg-8", "method": "human_provided"}]
        }),
    );
    let proc_ko = c.call(
        "get",
        &json!({"subject": "chatbot-user", "koid": proc["koid"].as_str().unwrap()}),
    );
    assert_eq!(proc_ko["type_name"], "aikoql:experience");
    assert!(
        proc_ko["properties"]["reuse_conditions"]
            .to_string()
            .contains("account reset request"),
        "CLASS-004: procedural memory must carry reuse_conditions: {}",
        proc_ko["properties"]["reuse_conditions"]
    );

    // CLASS-005: "Run ResetAccount." → Program-as-KO.
    let prog = c.call(
        "deploy_program",
        &json!({
            "subject": "chatbot-user", "name": "ResetAccount",
            "body": "MATCH AccountInfo WHERE account == \"ACME-123\" RETURN *",
            "language": "aikoql"
        }),
    );
    let prog_ko = c.call(
        "get",
        &json!({"subject": "chatbot-user", "koid": prog["koid"].as_str().unwrap()}),
    );
    assert_eq!(prog_ko["type_name"], "aikoql:program");

    // ── §11 PERS-001/002: behavior + explainability ────────────────────────
    // PERS-001: the preference that changes behavior is durable knowledge.
    // PERS-002: "Why do you answer concisely?" → provenance names the user
    // statement, the confidence, and the evidence chain.
    let prov = c.call(
        "provenance",
        &json!({"subject": "chatbot-user", "koid": &style_koid}),
    );
    let prov_md = prov["provenance"].as_str().unwrap();
    assert!(
        prov_md.contains("chat-msg-style"),
        "PERS-002: provenance must name the source chat message: {prov_md}"
    );
    assert!(
        prov_md.contains("Confidence:"),
        "PERS-002: provenance must carry the confidence: {prov_md}"
    );

    // ── PERS-003: conflict resolution keeps history ────────────────────────
    // "Actually I prefer detailed answers now." → supersede, not overwrite.
    let detailed = c.call("assert_knowledge", &json!({
        "subject": "chatbot-user", "type_name": "UserPreference", "tenant": "acme",
        "properties": {"topic": "response style", "value": "detailed"},
        "authority": "human_approved",
        "evidence": [{"source_artifact": "chat-msg-9", "method": "human_provided", "confidence": 1.0}]
    }));
    let detailed_koid = detailed["koid"].as_str().unwrap().to_string();
    c.call(
        "supersede",
        &json!({
            "subject": "chatbot-user",
            "old": &style_koid,
            "superseded_by": &detailed_koid,
            "reason": "user now prefers detailed answers",
            "evidence": [{"source_artifact": "chat-msg-9", "method": "human_provided"}]
        }),
    );
    // The old preference is closed but readable — history is never lost.
    let old_style = c.call(
        "get",
        &json!({"subject": "chatbot-user", "koid": &style_koid}),
    );
    assert_eq!(old_style["properties"]["value"], "concise");
    assert_eq!(old_style["extensions"]["epistemic_status"], "superseded");
    // Current-truth recall returns only the new preference.
    let current = c.call(
        "aikoql",
        &json!({"subject": "chatbot-user", "query": "MATCH UserPreference WHERE topic == \"response style\" RETURN *"}),
    );
    let current_values: Vec<String> = current["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["properties"]["value"].as_str().map(String::from))
        .collect();
    assert!(
        current_values.contains(&"detailed".to_string())
            && !current_values.contains(&"concise".to_string()),
        "PERS-003: current-truth recall must return only the new preference: {current_values:?}"
    );

    // ── PERS-004: user scope confinement ───────────────────────────────────
    // Another user in the same tenant must see neither the preference nor
    // the point object; a user preference never widens to org scope.
    c.session_init("other-user", "acme");
    let leak = c.call("aikoql", &json!({"query": "MATCH UserPreference RETURN *"}));
    assert_eq!(
        leak["results"].as_array().unwrap().len(),
        0,
        "PERS-004: another user's recall must not leak this user's preferences: {leak}"
    );
    let foreign = c.call_raw("get", &json!({"koid": &style_koid}));
    let foreign_text = foreign["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        foreign["result"]["isError"] == true && foreign_text.contains("ACCESS_DENIED"),
        "PERS-004: another user's point read must be denied: {foreign}"
    );

    let _ = std::fs::remove_file(&db);
}

/// G7 — CTX differential scenarios (TP-3c): the same context-compilation
/// question over the real MCP surface under different permissions (CTX-001),
/// different temporal states (CTX-002), and post-update knowledge (CTX-003).
/// CTX-MIN-001..003 (1000-KO minimization, no irrelevant forwarding, dedup)
/// are pure-compiler tests in aikoql-ingestion's context::tests.
#[test]
fn ctx_differential_scenarios() {
    let db = tmp_db("ctx");
    let _ = std::fs::remove_file(&db);
    let mut c = McpClient::start(&db);
    c.session_init("alice", "acme");

    // Knowledge snapshot v1 — the same ir_json shape ingest-dir produces.
    let v1 = KnowledgeIr {
        entities: vec![
            EntityCandidate {
                name: "PaymentService".into(),
                type_hint: Some("Struct".into()),
                mentions: vec!["processes payments".into()],
                confidence: 0.9,
                evidence: Evidence::default(),
            },
            EntityCandidate {
                name: "Ledger".into(),
                type_hint: Some("Struct".into()),
                mentions: vec!["payment ledger".into()],
                confidence: 0.8,
                evidence: Evidence::default(),
            },
        ],
        facts: vec![FactCandidate {
            snippet: None,
            statement: "payments flow through Stripe".into(),
            entities: vec![],
            confidence: 0.9,
            evidence: Evidence::default(),
        }],
        ..Default::default()
    };
    let doc = c.call(
        "remember",
        &json!({
            "subject": "alice", "type_name": "KnowledgeSnapshot", "tenant": "acme",
            "properties": {"ir_json": serde_json::to_string(&v1).unwrap()},
            "origin": "system"
        }),
    );
    let doc_koid = doc["koid"].as_str().unwrap().to_string();

    // ── CTX-001: same question, two users, different permissions ──────────
    // Alice (owner) compiles the payments context…
    let alice_ctx = c.call(
        "compile_context",
        &json!({"subject": "alice", "koid": &doc_koid, "task": "process payments"}),
    );
    let alice_names: Vec<&str> = alice_ctx["package"]["entities"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(
        alice_names.contains(&"PaymentService"),
        "the owner must get the payment context: {alice_ctx}"
    );
    // §36: a disabled/absent semantic index must be detectable in the
    // response — this harness has no embedding provider wired, so the
    // compile must say so instead of silently degrading.
    assert_eq!(
        alice_ctx["semantic"],
        json!(false),
        "semantic availability must be reported: {alice_ctx}"
    );

    // …Bob (same tenant, no grant) gets no context at all — the context
    // compilation layer is permission-differential, not just content-differential.
    c.session_init("bob", "acme");
    let bob_ctx = c.call_raw(
        "compile_context",
        &json!({"koid": &doc_koid, "task": "process payments"}),
    );
    let bob_text = bob_ctx["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        bob_ctx["result"]["isError"] == true && bob_text.contains("ACCESS_DENIED"),
        "CTX-001: a user without permission must get no context: {bob_ctx}"
    );

    // ── CTX-002: same question at two times — temporal state ──────────────
    // A fresh run experience (1s TTL) enters the context for the refund task…
    c.session_init("alice", "acme");
    c.call(
        "record_experience",
        &json!({
            "subject": "alice",
            "goal": "process payments refund",
            "action": "refund via payment service",
            "outcome": "success",
            "preconditions": [],
            "ttl_seconds": 1,
            "evidence": [{"source_artifact": "exec-run-2", "method": "runtime_observation"}]
        }),
    );
    let ctx_t0 = c.call(
        "compile_context",
        &json!({"subject": "alice", "koid": &doc_koid, "task": "process payments refund"}),
    );
    assert!(
        !ctx_t0["experiences"].as_array().unwrap().is_empty(),
        "t0: the fresh experience must be in the context: {ctx_t0}"
    );
    // …and drops out once its temporal window closes. Same question, same
    // knowledge — only time has passed, so only the temporal state differs.
    std::thread::sleep(std::time::Duration::from_millis(2200));
    let ctx_t1 = c.call(
        "compile_context",
        &json!({"subject": "alice", "koid": &doc_koid, "task": "process payments refund"}),
    );
    assert!(
        ctx_t1["experiences"].as_array().unwrap().is_empty(),
        "CTX-002: the expired experience must drop out of the context: {ctx_t1}"
    );

    // ── CTX-003: same question after a knowledge update ───────────────────
    // The snapshot moves to v2: internal ledger replaces Stripe.
    let v2 = KnowledgeIr {
        entities: vec![
            EntityCandidate {
                name: "PaymentService".into(),
                type_hint: Some("Struct".into()),
                mentions: vec!["processes payments".into()],
                confidence: 0.9,
                evidence: Evidence::default(),
            },
            EntityCandidate {
                name: "InternalLedger".into(),
                type_hint: Some("Struct".into()),
                mentions: vec!["internal payment ledger".into()],
                confidence: 0.8,
                evidence: Evidence::default(),
            },
        ],
        facts: vec![FactCandidate {
            snippet: None,
            statement: "payments flow through the internal ledger".into(),
            entities: vec![],
            confidence: 0.9,
            evidence: Evidence::default(),
        }],
        relations: vec![RelationCandidate {
            subject: "PaymentService".into(),
            predicate: "depends_on".into(),
            object: "InternalLedger".into(),
            confidence: 0.8,
            evidence: Evidence::default(),
        }],
        ..Default::default()
    };
    let upd = c.call(
        "remember",
        &json!({
            "subject": "alice", "koid": &doc_koid, "expected_version": 1,
            "type_name": "KnowledgeSnapshot", "tenant": "acme",
            "properties": {"ir_json": serde_json::to_string(&v2).unwrap()},
            "origin": "system"
        }),
    );
    assert_eq!(upd["version"], 2);

    let after = c.call(
        "compile_context",
        &json!({"subject": "alice", "koid": &doc_koid, "task": "process payments"}),
    );
    let fact_strs: Vec<&str> = after["package"]["facts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["statement"].as_str())
        .collect();
    assert!(
        fact_strs.iter().any(|s| s.contains("internal ledger")),
        "CTX-003: the updated context must carry the new fact: {after}"
    );
    assert!(
        !fact_strs.iter().any(|s| s.contains("Stripe")),
        "CTX-003: the replaced fact must not linger in the context: {fact_strs:?}"
    );
    let after_names: Vec<&str> = after["package"]["entities"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(
        after_names.contains(&"InternalLedger"),
        "CTX-003: the new entity must enter the context: {after_names:?}"
    );

    let _ = std::fs::remove_file(&db);
}

#[test]
fn mcp_ping_and_tools_list() {
    let db = tmp_db("ping");
    let _ = std::fs::remove_file(&db);
    let mut c = McpClient::start(&db);

    // Ping
    let mut req = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
    c.stdin
        .write_all((serde_json::to_string(&req).unwrap() + "\n").as_bytes())
        .unwrap();
    c.stdin.flush().unwrap();
    let mut resp = String::new();
    c.reader.read_line(&mut resp).unwrap();
    let v: J = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"], json!({}));

    // Tools list
    req = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
    c.stdin
        .write_all((serde_json::to_string(&req).unwrap() + "\n").as_bytes())
        .unwrap();
    c.stdin.flush().unwrap();
    resp.clear();
    c.reader.read_line(&mut resp).unwrap();
    let v: J = serde_json::from_str(&resp).unwrap();
    let tools = v["result"]["tools"].as_array().unwrap();
    assert!(
        tools.len() >= 30,
        "Expected >=30 tools, got {}",
        tools.len()
    );

    let _ = std::fs::remove_file(&db);
}

#[test]
fn mcp_idempotency_guarantee() {
    let db = tmp_db("idem");
    let _ = std::fs::remove_file(&db);
    let mut c = McpClient::start(&db);

    // Create with idempotency key.
    let r1 = c.call(
        "remember",
        &json!({
            "subject": "admin", "type_name": "Note",
            "properties": {"body": "idempotent test"},
            "idempotency_key": "agent-retry-001"
        }),
    );
    let koid1 = r1["koid"].as_str().unwrap().to_string();

    // Repeat with same idempotency key — must return same KOID, not create a new one.
    let r2 = c.call(
        "remember",
        &json!({
            "subject": "admin", "type_name": "Note",
            "properties": {"body": "idempotent test"},
            "idempotency_key": "agent-retry-001"
        }),
    );
    assert_eq!(r2["koid"].as_str().unwrap(), koid1);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn mvp_rec_002_backup_destroy_restore_round_trip() {
    // MVP-QA-001 MVP-REC-002: backup → destroy → restore yields equivalent
    // knowledge — same KOID resolvable with the same content, and the
    // backup is listable.
    let db = tmp_db("recv");
    let _ = std::fs::remove_file(&db);

    // Phase 1: build knowledge.
    let mut c = McpClient::start(&db);
    let note = c.call(
        "remember",
        &json!({
            "subject": "admin", "type_name": "note", "tenant": "acme",
            "properties": {"body": "quarterly revenue reached 42M", "memo": "rec002"}
        }),
    );
    let koid = note["koid"].as_str().unwrap().to_string();

    // MVP-QA-001 REC-002 equivalence legs (2026-08-25): relations,
    // provenance (evidence + assertion instant) and temporal state
    // (supersession) must all survive backup → destroy → restore.
    let asserted = c.call(
        "assert_knowledge",
        &json!({
            "subject": "admin", "type_name": "Policy",
            "properties": {"text": "retention is 30 days"},
            "authority": "architecture_decision",
            "evidence": [{"source_artifact": "runbook.md", "method": "doc_extraction"}],
            "valid_from": 1000
        }),
    );
    let asserted_koid = asserted["koid"].as_str().unwrap().to_string();

    let rel = c.call(
        "relate",
        &json!({
            "subject": "admin", "from": &koid, "to": &asserted_koid,
            "rel_type": "derived_from"
        }),
    );
    assert!(rel["koid"].as_str().is_some());

    let successor = c.call(
        "remember",
        &json!({
            "subject": "admin", "type_name": "note", "tenant": "acme",
            "properties": {"body": "quarterly revenue reached 45M", "memo": "rec002-v2"}
        }),
    );
    let successor_koid = successor["koid"].as_str().unwrap().to_string();
    let sup = c.call(
        "supersede",
        &json!({
            "subject": "admin",
            "old": &koid,
            "superseded_by": &successor_koid,
            "reason": "correction: 45M",
            "evidence": [{"source_artifact": "finance.md", "method": "human_provided"}]
        }),
    );
    assert_eq!(sup["new"], successor_koid);

    // Phase 2: verified backup + it must be listable.
    let backup = c.call("backup", &json!({"subject": "admin"}));
    assert_eq!(backup["verified"], true, "backup must verify: {backup}");
    let backup_dir = backup["backup"].as_str().unwrap().to_string();

    let list = c.call("list_backups", &json!({"subject": "admin"}));
    let names: Vec<&str> = list["backups"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| b["name"].as_str())
        .collect();
    assert!(
        names.iter().any(|n| backup_dir.ends_with(n)),
        "backup must appear in list_backups, got {names:?}"
    );

    // Phase 3: destroy — kill the server, delete the database (a redb file
    // pre-flip, an aikoql-v2 directory now — the 2026-09-07 default; give
    // the killed process a moment to release the handle on Windows).
    drop(c);
    let mut removed = false;
    for _ in 0..20 {
        if std::fs::remove_file(&db).is_ok() || std::fs::remove_dir_all(&db).is_ok() {
            removed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    assert!(removed, "destroy: database file must be removable");

    // Phase 4: fresh server on the same path (empty DB), then restore.
    let mut c = McpClient::start(&db);
    let restored = c.call(
        "restore",
        &json!({"subject": "admin", "backup": &backup_dir}),
    );
    assert_eq!(
        restored["restored"], true,
        "restore must succeed: {restored}"
    );

    // Phase 5: the restored file lands on reopen — restart the server.
    drop(c);
    let mut c = McpClient::start(&db);

    // Phase 6: equivalent knowledge — same KOID, same content.
    let fetched = c.call("get", &json!({"koid": &koid, "subject": "admin"}));
    assert_eq!(fetched["type_name"], "note");
    assert_eq!(
        fetched["properties"]["body"],
        "quarterly revenue reached 42M"
    );

    // Relations survive: note → Policy derived_from.
    let rels = fetched["relationships"].as_array().unwrap();
    assert!(
        rels.iter().any(|r| r["target"] == asserted_koid),
        "relation to asserted policy must survive restore: {fetched}"
    );

    // Temporal state survives: the supersession mark + successor link.
    assert_eq!(fetched["extensions"]["epistemic_status"], "superseded");
    assert!(
        rels.iter().any(|r| r["target"] == successor_koid),
        "supersession link must survive restore"
    );

    // Provenance survives: evidence list + assertion instant on the asserted KO.
    let restored_asserted = c.call("get", &json!({"koid": &asserted_koid, "subject": "admin"}));
    let evidence = restored_asserted["extensions"]["evidence"]
        .as_array()
        .unwrap();
    assert!(!evidence.is_empty(), "evidence must survive restore");
    assert_eq!(restored_asserted["extensions"]["valid_from"], 1000);

    let _ = std::fs::remove_file(&db);
}
