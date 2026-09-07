//! Extracted verbatim from server.rs (PRR-7). No behavior changes.

use crate::session::*;
use crate::{
    error, info, thread, warn, Arc, AtomicU64, BufRead, BufReader, HashSet, Kernel, Mutex,
    Ordering, TcpListener, TcpStream, J, PROTOCOL_VERSION,
};
// Test-only (the stdio client below) — unused in the bin target.
#[cfg(test)]
use crate::{json, RedbEngine, SystemClock};

use crate::dispatcher::*;
use crate::protocol::*;

pub(crate) static ACTIVE_CONNECTIONS: AtomicU64 = AtomicU64::new(0);
pub(crate) static STREAM_ID: AtomicU64 = AtomicU64::new(0);
pub(crate) fn handle_tcp_client(
    kernel: &Arc<Kernel>,
    stream: TcpStream,
    db_path: Arc<String>,
    auth: &Arc<TcpAuthTable>,
    rate_limit: Arc<Mutex<crate::rate_limiter::RateLimiter>>,
) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        // justified: log-only cosmetic — unknown peer on failure
        .unwrap_or_default();
    ACTIVE_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
    info!(%peer, "client connected");
    let Ok(clone) = stream.try_clone() else {
        eprintln!("clone stream failed — dropping connection");
        return;
    };
    let reader = BufReader::new(clone);
    let writer = Arc::new(Mutex::new(stream));
    let mut sub_ids: HashSet<String> = HashSet::new();
    // R5 (review round 3): the limiter is process-shared and keyed by
    // principal in the dispatcher — not created per connection here.
    // PRR-2: TCP identity comes exclusively from a verified --tcp-token.
    let mut session = McpSession {
        trust_mode: TrustMode::Tcp,
        ..Default::default()
    };
    let mut authenticated = false;
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: J = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(e) => {
                let mut out = writer.lock().unwrap(); // justified: Mutex poison is unrecoverable
                write_frame(
                    &mut *out,
                    err_frame(&J::Null, -32700, &format!("parse error: {}", e)),
                );
                continue;
            }
        };
        // PRR-2 auth gate: only initialize (token check) and ping are allowed
        // before authentication; everything else is rejected and the
        // connection dropped (fail-closed).
        if !authenticated {
            let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
            if method == "initialize" {
                let params = msg.get("params").cloned().unwrap_or(J::Null);
                let token = params.get("token").and_then(|t| t.as_str()).unwrap_or("");
                match auth.lookup(token) {
                    Some(ident) => {
                        session.agent_id = "tcp-agent".into();
                        session.tenant = ident.tenant.clone();
                        session.roles = ident.roles.clone();
                        authenticated = true;
                        info!(%peer, roles = %session.roles.join(","), "TCP client authenticated");
                    }
                    None => {
                        let mut out = writer.lock().unwrap(); // justified: Mutex poison is unrecoverable
                        if let Some(id) = msg.get("id").cloned() {
                            write_frame(
                                &mut *out,
                                err_frame(&id, -32001, "invalid or missing token — pass a --tcp-token value as params.token to initialize"),
                            );
                        }
                        warn!(%peer, "TCP client rejected: invalid token");
                        break;
                    }
                }
            } else if method != "ping" {
                let mut out = writer.lock().unwrap(); // justified: Mutex poison is unrecoverable
                if let Some(id) = msg.get("id").cloned() {
                    write_frame(
                        &mut *out,
                        err_frame(
                            &id,
                            -32001,
                            "authentication required — call initialize with a valid token first",
                        ),
                    );
                }
                warn!(%peer, method = %method, "TCP client rejected: unauthenticated");
                break;
            }
        }
        handle_message(
            kernel,
            &mut sub_ids,
            &writer,
            &rate_limit,
            &db_path,
            &mut session,
            msg,
        );
    }
    ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
    info!(%peer, "client disconnected");
}
pub(crate) fn run_tcp_listener(
    kernel: Arc<Kernel>,
    listener: TcpListener,
    auth: Arc<TcpAuthTable>,
    db_path: Arc<String>,
    rate_limit: Arc<Mutex<crate::rate_limiter::RateLimiter>>,
) {
    info!(
        addr = %listener.local_addr().map(|a| a.to_string()).unwrap_or_default(),
        db = %db_path,
        "aikoql-mcp TCP server ready (token auth required)"
    );
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let k = kernel.clone();
                let db = db_path.clone();
                let auth = auth.clone();
                let rl = rate_limit.clone();
                thread::spawn(move || handle_tcp_client(&k, stream, db, &auth, rl));
            }
            Err(e) => error!("accept error: {}", e),
        }
    }
}
pub(crate) fn run_stdio(
    kernel: &Arc<Kernel>,
    db_path: &Arc<String>,
    rate_limit: Arc<Mutex<crate::rate_limiter::RateLimiter>>,
) {
    info!(db = %db_path, protocol = PROTOCOL_VERSION, "aikoql-mcp ready");
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let mut sub_ids: HashSet<String> = HashSet::new();
    let mut session = McpSession::default();
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: J = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(e) => {
                let mut out = stdout.lock().unwrap(); // justified: Mutex poison is unrecoverable
                write_frame(
                    &mut *out,
                    err_frame(&J::Null, -32700, &format!("parse error: {}", e)),
                );
                continue;
            }
        };
        handle_message(
            kernel,
            &mut sub_ids,
            &stdout,
            &rate_limit,
            db_path,
            &mut session,
            msg,
        );
    }
}

#[cfg(test)]
#[cfg(test)]
mod tcp_auth_tests {
    // PRR-2 acceptance matrix: unauthenticated → reject; wrong token → reject;
    // user token → server identity, no escalation; admin → privileged tools;
    // tenant A cannot read tenant B; per-call roles never elevate.
    use super::*;
    use crate::session::TcpAuthTable;
    use std::io::{BufRead, BufReader, Write};

    static DB_SEQ: AtomicU64 = AtomicU64::new(0);

    fn spawn_server(token_specs: &[&str]) -> std::net::SocketAddr {
        spawn_server_with_limit(token_specs, 1000)
    }

    fn spawn_server_with_limit(token_specs: &[&str], max_per_minute: u64) -> std::net::SocketAddr {
        // ponytail: this db stays open in the detached listener thread for
        // the process lifetime, so no sweeper can remove it (Windows locks
        // the file) — a ~1.5MB pid-unique file per spawn is the accepted leak.
        let db = std::env::temp_dir().join(format!(
            "mcp-tcp-auth-{}-{}.redb",
            std::process::id(),
            DB_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&db);
        let engine = RedbEngine::open(db.to_str().unwrap()).expect("open engine");
        let kernel =
            Kernel::open(Arc::new(engine), Arc::new(SystemClock), 0xA9C9).expect("open kernel");
        let specs: Vec<String> = token_specs.iter().map(|s| s.to_string()).collect();
        let auth = Arc::new(TcpAuthTable::parse(&specs).expect("valid token specs"));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let db_path = Arc::new(db.to_str().unwrap().to_string());
        let rate_limit = Arc::new(Mutex::new(crate::rate_limiter::RateLimiter::new(
            true,
            max_per_minute,
        )));
        thread::spawn(move || {
            run_tcp_listener(Arc::new(kernel), listener, auth, db_path, rate_limit)
        });
        addr
    }

    struct TcpClient {
        stream: TcpStream,
        reader: BufReader<TcpStream>,
        next_id: u64,
    }

    impl TcpClient {
        fn connect(addr: std::net::SocketAddr) -> Self {
            let stream = TcpStream::connect(addr).expect("connect");
            let reader = BufReader::new(stream.try_clone().unwrap());
            TcpClient {
                stream,
                reader,
                next_id: 1,
            }
        }

        /// Send one request, read one frame. Panics if the connection closes.
        fn req(&mut self, method: &str, params: J) -> J {
            let id = self.next_id;
            self.next_id += 1;
            let line =
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string() + "\n";
            self.stream.write_all(line.as_bytes()).unwrap();
            self.stream.flush().unwrap();
            let mut resp = String::new();
            let n = self.reader.read_line(&mut resp).unwrap();
            if n == 0 {
                panic!("server closed connection before responding to {method}");
            }
            serde_json::from_str(&resp).unwrap()
        }

        fn init(&mut self, token: &str) -> J {
            self.req("initialize", json!({"token": token}))
        }

        fn call(&mut self, tool: &str, args: J) -> J {
            self.req("tools/call", json!({"name": tool, "arguments": args}))
        }

        /// Returns 0 on EOF (server dropped the connection).
        fn read_line_or_eof(&mut self) -> usize {
            let mut buf = String::new();
            self.reader.read_line(&mut buf).unwrap()
        }
    }

    #[test]
    fn tcp_rate_limit_rejects_excess_tool_calls() {
        let addr = spawn_server_with_limit(&["user1:acme:viewer"], 3);
        let mut c = TcpClient::connect(addr);
        let init = c.init("user1");
        assert!(init.get("error").is_none(), "expected auth ok, got {init}");
        for _ in 0..3 {
            let r = c.call("metrics", json!({}));
            assert!(
                r.get("error").is_none(),
                "call under limit must pass, got {r}"
            );
        }
        let r = c.call("metrics", json!({}));
        let msg = r
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or_default();
        assert!(msg.contains("rate limit exceeded"), "got {r}");
        // Error frame, not a drop — the next call is also rejected, not EOF.
        assert!(c.call("metrics", json!({})).get("error").is_some());
    }

    #[test]
    fn tcp_rate_limit_is_shared_across_connections() {
        // R5 (review round 3): the budget is per PRINCIPAL (agent_id:tenant),
        // not per connection — one principal on two sockets still gets one
        // budget, so a second connection cannot double the allowance.
        let addr = spawn_server_with_limit(&["user1:acme:viewer"], 3);
        let mut a = TcpClient::connect(addr);
        let mut b = TcpClient::connect(addr);
        for c in [&mut a, &mut b] {
            let init = c.init("user1");
            assert!(init.get("error").is_none(), "expected auth ok, got {init}");
        }
        for _ in 0..2 {
            assert!(
                a.call("metrics", json!({})).get("error").is_none(),
                "conn A under limit"
            );
        }
        // Conn B shares the same principal budget — one call fills it.
        assert!(
            b.call("metrics", json!({})).get("error").is_none(),
            "conn B shares the remaining budget"
        );
        // Budget exhausted: BOTH connections are now rejected.
        let msg = b
            .call("metrics", json!({}))
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        assert!(msg.contains("rate limit exceeded"), "got {msg}");
        assert!(a.call("metrics", json!({})).get("error").is_some());
    }

    #[test]
    fn tcp_unauthenticated_rejected_and_dropped() {
        let addr = spawn_server(&["s3cret:acme:viewer"]);
        let mut c = TcpClient::connect(addr);
        let resp = c.req("tools/list", J::Null);
        assert!(
            resp.get("error").is_some(),
            "expected auth error, got {resp}"
        );
        assert_eq!(
            c.read_line_or_eof(),
            0,
            "connection must be dropped after rejection"
        );
    }

    #[test]
    fn tcp_wrong_token_rejected_and_dropped() {
        let addr = spawn_server(&["s3cret:acme:viewer"]);
        let mut c = TcpClient::connect(addr);
        let resp = c.init("wrong-token");
        assert!(
            resp.get("error").is_some(),
            "expected auth error, got {resp}"
        );
        assert_eq!(c.read_line_or_eof(), 0, "connection must be dropped");
    }

    #[test]
    fn tcp_user_token_gets_server_identity_and_cannot_elevate() {
        let addr = spawn_server(&["user1:acme:viewer"]);
        let mut c = TcpClient::connect(addr);
        // Correct token → initialize succeeds.
        let resp = c.init("user1");
        assert!(resp.get("result").is_some(), "expected success, got {resp}");
        // Ping still works after auth.
        assert!(c.req("ping", J::Null).get("result").is_some());
        // session/init cannot set tenant/roles (method path, raw params).
        let r = c.req(
            "session/init",
            json!({"agent_id": "mallory", "tenant": "other", "roles": ["admin"]}),
        );
        assert!(
            r.get("error").is_some(),
            "session/init must reject identity fields: {r}"
        );
        // Privileged tool (deploy_program → developer) denied for viewer.
        let r = c.call("deploy_program", json!({"name": "p", "body": "x"}));
        assert!(
            r.get("error").is_some(),
            "viewer must be denied deploy_program: {r}"
        );
        // Per-call roles:["admin"] in arguments must not elevate.
        let r = c.call(
            "deploy_program",
            json!({"name": "p", "body": "x", "roles": ["admin"]}),
        );
        assert!(
            r.get("error").is_some(),
            "per-call admin roles must not elevate: {r}"
        );
        // Non-privileged tool works.
        let r = c.call("metrics", json!({}));
        assert!(r.get("result").is_some(), "metrics should succeed: {r}");
    }

    #[test]
    fn tcp_admin_token_allows_privileged_tools() {
        let addr = spawn_server(&["boss::admin", "user1:acme:viewer"]);
        let mut c = TcpClient::connect(addr);
        let resp = c.init("boss");
        assert!(resp.get("result").is_some(), "expected success, got {resp}");
        // Admin (tenant-less token) may deploy programs.
        let r = c.call("deploy_program", json!({"name": "p", "body": "RETURN 1"}));
        assert!(
            r.get("result").is_some(),
            "admin deploy should succeed: {r}"
        );
        // session/init with only run_id is allowed in TCP mode.
        let r = c.req("session/init", json!({"run_id": "r42"}));
        assert!(
            r.get("result").is_some(),
            "run_id-only session/init should succeed: {r}"
        );
        assert_eq!(r["result"]["session"]["agent_id"], "tcp-agent");
        assert_eq!(r["result"]["session"]["roles"], json!(["admin"]));
    }

    #[test]
    fn tcp_tenant_isolation_across_tokens() {
        let addr = spawn_server(&["userA:tenantA:viewer", "userB:tenantB:viewer"]);
        // Tenant A creates a KO.
        let mut a = TcpClient::connect(addr);
        let resp = a.init("userA");
        assert!(resp.get("result").is_some());
        let r = a.call(
            "remember",
            json!({"type_name": "Note", "properties": {"body": "secret-a"}}),
        );
        assert!(r.get("result").is_some(), "remember should succeed: {r}");
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        let koid: J = serde_json::from_str(text).unwrap();
        let koid = koid["koid"]
            .as_str()
            .unwrap_or_else(|| panic!("remember payload has no koid — call failed: {koid}"))
            .to_string();
        // Tenant A can read it back.
        let r = a.call("get", json!({"koid": koid}));
        assert!(
            r.get("result").is_some(),
            "tenant A must read its own KO: {r}"
        );
        drop(a);
        // Tenant B cannot read it.
        let mut b = TcpClient::connect(addr);
        let resp = b.init("userB");
        assert!(resp.get("result").is_some());
        let r = b.call("get", json!({"koid": koid}));
        assert!(
            r.get("error").is_some() || r["result"]["isError"] == true,
            "tenant B must not read tenant A's KO: {r}"
        );
    }
}
