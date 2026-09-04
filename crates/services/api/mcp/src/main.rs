#![recursion_limit = "512"]
#![allow(clippy::too_many_arguments)]
//! aikoql-mcp — MCP server for the Knowledge Kernel.
//!
//! Exposes the MRFC-0011 Class A syscalls as MCP tools over the stdio
//! transport (newline-delimited JSON-RPC 2.0). `notify` is intentionally not
//! exposed (streaming; lands with durable CDC in Phase 2).
//!
//! Protocol surface: initialize, ping, tools/list, tools/call.
//! Logs go to stderr; stdout carries protocol frames only.
//! Structured tracing via `tracing` with env-filter (`RUST_LOG`).

mod admin;
mod api_rest;
mod audit;
mod authz;
mod cli;
mod config;
mod dispatcher;
mod engine;
mod error_codes;
mod graph_ui;
mod helpers;
mod http;
mod imports;
mod ingest;
mod knowledge_runtime;
mod model;
mod protocol;
mod rate_limiter;
mod session;
mod shell;
mod studio;
mod tool_registry;
mod tools;
mod transport;

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

pub(crate) struct HttpSession {
    pub username: String,
    pub roles: Vec<String>,
    pub created: Instant,
}

// pub(crate) re-exports: this block is the crate prelude. Extracted modules
// (R7) import from it explicitly — every module carries its own
// `use crate::{...}` list (prelude cleanup, 2026-08-29); api_rest and
// knowledge_runtime use `use super::*` — both work because these are
// re-exports, not private uses.
pub(crate) use aikoql_graph::*;
pub(crate) use aikoql_kernel::ir::*;
pub(crate) use aikoql_kernel::knowledge::ontology::{
    discover_ontology, OntologyDef, OntologyRegistry, ONTOLOGY_TYPE,
};
pub(crate) use aikoql_kernel::lifecycle::schema::SchemaRegistry;
pub(crate) use aikoql_kernel::*;
pub(crate) use aikoql_scheduler::Scheduler;
#[cfg(feature = "embedding-openai")]
pub(crate) use aikoql_semantic::provider::OpenAiEmbeddingProvider;
pub(crate) use aikoql_semantic::{EmbeddingEnricher, SemanticEngine};
pub(crate) use serde_json::{json, Value as J};
pub(crate) use std::collections::{HashMap, HashSet};
pub(crate) use std::io::{BufRead, BufReader, Read, Write};
pub(crate) use std::net::{TcpListener, TcpStream, ToSocketAddrs};
pub(crate) use std::sync::atomic::{AtomicU64, Ordering};
pub(crate) use std::sync::{Arc, Mutex, OnceLock};
pub(crate) use std::thread;
pub(crate) use std::time::Instant;
pub(crate) use tracing::{error, info, info_span, warn};

pub(crate) static SERVER_START: OnceLock<Instant> = OnceLock::new();
pub(crate) static MEMORY_DIR: OnceLock<String> = OnceLock::new();

/// PRR-3: semantic readiness — the enrichment worker thread updates this,
/// tool_health and /health surface it.
#[derive(Clone, Debug)]
pub(crate) struct SemanticStatus {
    pub(crate) state: &'static str, // "initializing" | "ready" | "unavailable"
    pub(crate) detail: String,
}

pub(crate) static SEMANTIC_STATUS: OnceLock<Mutex<SemanticStatus>> = OnceLock::new();

pub(crate) fn set_semantic_status(state: &'static str, detail: impl Into<String>) {
    let cell = SEMANTIC_STATUS.get_or_init(|| {
        Mutex::new(SemanticStatus {
            state: "initializing",
            detail: String::new(),
        })
    });
    let mut s = cell.lock().unwrap(); // justified: Mutex poison is unrecoverable
    s.state = state;
    s.detail = detail.into();
}

pub(crate) fn semantic_status_snapshot() -> SemanticStatus {
    SEMANTIC_STATUS
        .get()
        .map(|m| m.lock().unwrap().clone()) // justified: Mutex poison is unrecoverable
        .unwrap_or(SemanticStatus {
            state: "initializing",
            detail: String::new(),
        })
}

/// PRR-3: local model store — `--model-dir` wins, else `~/.aikoql/models`.
pub(crate) fn model_store_dir(flag: Option<&str>) -> std::path::PathBuf {
    match flag {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let home = std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            home.join(".aikoql").join("models")
        }
    }
}

pub(crate) const PROTOCOL_VERSION: &str = "2024-11-05";

// main() is the orchestrator: CLI dispatch + server bootstrap. Everything
// else lives in the modules above (R7).
use crate::cli::*;
use crate::http::*;
use crate::session::TcpAuthTable;
use crate::transport::*;

#[allow(unused_assignments)]
fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Handle --version and --help before anything else.
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("aikoql-mcp {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return;
    }

    // Subcommand routing: find first positional arg, then use args after it.
    let subcmd_idx = args.iter().skip(1).position(|a| !a.starts_with('-'));
    let subcmd = subcmd_idx.map(|i| args[i + 1].as_str());
    if dispatch(&args, subcmd, subcmd_idx) {
        return;
    }

    // PRR-4: defaults → aikoql.toml → env → CLI, validated in one place.
    let cfg = match config::load(&args, subcmd, subcmd_idx) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&cfg.log_level)),
        )
        .with_writer(std::io::stderr);
    if cfg.log_format == "json" {
        subscriber.json().init();
    } else {
        subscriber.init();
    }
    if let Some(path) = &cfg.config_path {
        info!(config = %path, "configuration loaded");
    }
    let listen_addr = cfg.listen_addr;
    let metrics_addr = cfg.metrics_addr;
    let tcp_tokens = cfg.tcp_tokens;
    let db_path = cfg.db_path;
    let memory_dir = cfg.memory_dir;
    // PRR-4: [rate_limit] config — per-connection on MCP tools/call, per-token
    // on the REST surface (shared limiter below).
    let rate_enabled = cfg.rate_enabled;
    let rate_max_calls_per_minute = cfg.rate_max_calls_per_minute;
    let rest_rate_limit = Arc::new(Mutex::new(crate::rate_limiter::RateLimiter::new(
        rate_enabled,
        rate_max_calls_per_minute,
    )));
    // R5 (review round 3): ONE MCP limiter shared across all stdio/TCP
    // sessions — the dispatcher keys it by principal. (R9 deleted the
    // second, hidden limiter in authz.rs.)
    let mcp_rate_limit = Arc::new(Mutex::new(crate::rate_limiter::RateLimiter::new(
        rate_enabled,
        rate_max_calls_per_minute,
    )));
    let embedding_provider = cfg.embedding_provider;
    #[allow(unused_assignments, unused_variables)]
    let embedding_base_url = cfg.embedding_base_url;
    let embedding_model = cfg.embedding_model;
    #[allow(unused_assignments, unused_variables)]
    let embedding_api_key = cfg.embedding_api_key;
    // PRR-3: local model store override (default ~/.aikoql/models).
    let model_dir_flag = cfg.model_dir;
    MEMORY_DIR.set(memory_dir).ok();

    let kernel = match engine::open_kernel(&db_path, &cfg.encryption, cfg.backend) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("open kernel: {}", e);
            std::process::exit(1);
        }
    };

    #[cfg(feature = "embedding-openai")]
    let url = if embedding_base_url.is_empty() {
        "http://localhost:11434".to_string()
    } else {
        embedding_base_url.clone()
    };
    let model = if embedding_model.is_empty() {
        "nomic-embed-text".to_string()
    } else {
        embedding_model.clone()
    };

    // Build provider Arc first so we can share it with the enrichment engine.
    // PRR-3: the runtime NEVER downloads — candle loads from the local model
    // store only; a missing install degrades to lexical-only recall with a
    // clear remediation in tool_health / /health.
    let emb_provider: Option<Arc<dyn EmbeddingProvider>> = match embedding_provider.as_deref() {
        Some("openai") => {
            #[cfg(feature = "embedding-openai")]
            {
                let p = OpenAiEmbeddingProvider::new(&url, &model, embedding_api_key.as_deref());
                set_semantic_status(
                    "initializing",
                    format!("openai-compatible endpoint {url} (model {model})"),
                );
                Some(Arc::new(p))
            }
            #[cfg(not(feature = "embedding-openai"))]
            {
                set_semantic_status(
                    "unavailable",
                    "openai embedding requested but binary not compiled with embedding-openai feature",
                );
                None
            }
        }
        _ => {
            #[cfg(feature = "embedding-candle")]
            {
                let candle_dir = model_store_dir(model_dir_flag.as_deref()).join(
                    aikoql_semantic::provider::model_slug(
                        aikoql_semantic::provider::DEFAULT_MODEL_ID,
                    ),
                );
                // Model identity is explicit: a non-default --embedding-model
                // names a candle model that isn't installed, so we reject it
                // instead of silently swapping in all-MiniLM-L6-v2.
                if !embedding_model.is_empty() && embedding_model != "all-MiniLM-L6-v2" {
                    set_semantic_status(
                        "unavailable",
                        format!(
                            "model '{embedding_model}' is not installed — run `aikoql model install {embedding_model}`, or omit --embedding-model for the bundled all-MiniLM-L6-v2"
                        ),
                    );
                    None
                } else {
                    match aikoql_semantic::provider::CandleEmbedding::from_local(&candle_dir) {
                        Ok(p) => {
                            set_semantic_status(
                                "initializing",
                                "local model loaded; background enrichment running",
                            );
                            Some(Arc::new(p))
                        }
                        Err(e) => {
                            set_semantic_status(
                                "unavailable",
                                format!(
                                    "{e} — run `aikoql model install` to install all-MiniLM-L6-v2 into {}",
                                    candle_dir.display()
                                ),
                            );
                            None
                        }
                    }
                }
            }
            #[cfg(not(feature = "embedding-candle"))]
            {
                set_semantic_status(
                    "unavailable",
                    "no embedding provider compiled in — activate embedding-candle or embedding-openai feature",
                );
                None
            }
        }
    };

    let kernel = if let Some(ref p) = emb_provider {
        kernel.with_embedding_provider(p.clone())
    } else {
        kernel
    };
    let kernel = Arc::new(kernel);

    // PRR-3: enrichment runs on a worker thread — serve comes up immediately
    // and /health reports semantic readiness while the scan runs.
    if let Some(enrichment_provider) = emb_provider {
        // Record the real model: candle always loads all-MiniLM-L6-v2; the
        // --embedding-model flag only names the OpenAI-compatible endpoint.
        let enrichment_model = if embedding_provider.as_deref() == Some("openai") {
            model.clone()
        } else {
            "all-MiniLM-L6-v2".to_string()
        };
        let kernel_work = kernel.clone();
        thread::spawn(move || {
            let enricher = EmbeddingEnricher::new(enrichment_provider, &enrichment_model);
            let engine = Arc::new(SemanticEngine::new(Arc::new(enricher)));
            let sched = Scheduler::new();
            sched.register(engine);
            match sched.start_all(&kernel_work) {
                Ok(()) => set_semantic_status(
                    "ready",
                    format!("embeddings live (model {enrichment_model})"),
                ),
                Err(e) => set_semantic_status("unavailable", format!("enrichment failed: {e}")),
            }
        });
    }

    let db_path = Arc::new(db_path);
    SERVER_START.set(Instant::now()).ok();

    // Load ontology from stored Ontology KOs (MRFC-0041).
    // Prefer manually-curated ontologies over auto-discovered ones.
    // If multiple exist, pick the latest non-discovered, fall back to discovered.
    let ontology: Arc<OntologyRegistry> = {
        let subj = Subject::with_roles("system", &["admin"]);
        match kernel.scan_by_type(&subj, ONTOLOGY_TYPE) {
            Ok(kos) if !kos.is_empty() => {
                let manual: Vec<_> = kos
                    .iter()
                    .filter(|ko| !ko.metadata.tags.contains(&"auto-discovered".to_string()))
                    .collect();
                let candidate = if !manual.is_empty() {
                    // Prefer the latest manually-created ontology.
                    manual
                        .into_iter()
                        .max_by_key(|ko| ko.commit_ts)
                        .expect("manual is non-empty") // justified: guarded by !manual.is_empty() above
                } else {
                    // Fall back to latest auto-discovered.
                    kos.iter()
                        .max_by_key(|ko| ko.commit_ts)
                        .expect("kos is non-empty") // justified: guarded by the outer Ok(kos) if !kos.is_empty() arm
                };
                match OntologyDef::from_ko(candidate) {
                    Ok(def) => {
                        let source = if candidate
                            .metadata
                            .tags
                            .contains(&"auto-discovered".to_string())
                        {
                            "auto-discovered"
                        } else {
                            "manual"
                        };
                        info!(namespace = %def.namespace, version = %def.version,
                              classes = def.classes.len(),
                              mappings = def.mappings.len(),
                              source = source,
                              "ontology loaded");
                        match OntologyRegistry::new(def) {
                            Ok(r) => Arc::new(r),
                            Err(e) => {
                                info!(error = %e, "ontology registry failed to initialize — using empty registry");
                                Arc::new(OntologyRegistry::empty())
                            }
                        }
                    }
                    Err(e) => {
                        info!(error = %e, "ontology KO found but failed to decode — using empty registry");
                        Arc::new(OntologyRegistry::empty())
                    }
                }
            }
            _ => {
                info!("no ontology KO found — using empty registry");
                Arc::new(OntologyRegistry::empty())
            }
        }
    };

    // Optional HTTP metrics endpoint for Prometheus + Kubernetes probes.
    if let Some(ref addr) = metrics_addr {
        spawn_metrics(
            kernel.clone(),
            ontology.clone(),
            addr.clone(),
            db_path.clone(),
            rest_rate_limit.clone(),
        );
    }

    if let Some(addr) = listen_addr {
        // PRR-2: TCP requires token auth (fail-closed). Stdio keeps the
        // process-boundary trust model and needs no token.
        if tcp_tokens.is_empty() {
            eprintln!(
                "TCP mode requires at least one --tcp-token TOKEN[:TENANT[:ROLE1,ROLE2]] — refusing to serve without authentication (stdio mode needs no token)"
            );
            std::process::exit(2);
        }
        let auth = match TcpAuthTable::parse(&tcp_tokens) {
            Ok(t) => Arc::new(t),
            Err(e) => {
                eprintln!("invalid --tcp-token: {e}");
                std::process::exit(2);
            }
        };
        // R1 (review round 3): loopback-only plaintext TCP — a non-loopback
        // bind would put the bearer token on the wire unencrypted, so it is
        // rejected fail-closed. TLS arrives post-MVP (terminate TLS at a
        // reverse proxy in front of the loopback listener).
        let addr = match validate_listen(&addr) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        };
        let listener = match TcpListener::bind(&addr) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("bind TCP listener {}: {}", addr, e);
                std::process::exit(1);
            }
        };
        run_tcp_listener(kernel, listener, auth, db_path, mcp_rate_limit);
    } else {
        run_stdio(&kernel, &db_path, mcp_rate_limit);
    }
}

/// PRR-2 + R1 (review round 3): `--listen :9090` (empty host) binds
/// loopback only. A non-loopback bind is REJECTED fail-closed — without TLS
/// the bearer token would travel in plaintext, so plaintext TCP is
/// loopback-only in MVP. Hostnames resolve; every resolved address must be
/// loopback.
fn validate_listen(addr: &str) -> Result<String, String> {
    let expanded = match addr.rsplit_once(':') {
        Some(("", port)) => format!("127.0.0.1:{port}"),
        _ => addr.to_string(),
    };
    let resolved: Vec<std::net::SocketAddr> = expanded
        .to_socket_addrs()
        .map_err(|e| format!("invalid --listen address {expanded}: {e}"))?
        .collect();
    if resolved.is_empty() {
        return Err(format!(
            "--listen {expanded} did not resolve to any address"
        ));
    }
    if !resolved.iter().all(|a| a.ip().is_loopback()) {
        return Err(format!(
            "--listen {expanded} would accept plaintext TCP on a non-loopback interface; \
             bearer tokens must not travel unencrypted — bind 127.0.0.1 (or ::1) and \
             terminate TLS at a reverse proxy, or use stdio mode"
        ));
    }
    Ok(expanded)
}

#[cfg(test)]
mod tests;
