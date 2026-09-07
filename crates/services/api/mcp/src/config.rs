//! PRR-4: configuration pipeline — `defaults → aikoql.toml → env → CLI`.
//! One entry point (`load`) for serve-mode startup; subcommands keep their
//! own minimal tail parsing. Unknown TOML keys and invalid values are
//! rejected, not silently ignored.

use serde::Deserialize;

/// Merged runtime configuration after the full precedence pipeline.
#[derive(Clone, Debug)]
pub(crate) struct RuntimeConfig {
    pub db_path: String,
    pub listen_addr: Option<String>,
    pub metrics_addr: Option<String>,
    pub tcp_tokens: Vec<String>,
    pub memory_dir: String,
    /// None = default (candle); Some("openai") = OpenAI-compatible HTTP endpoint.
    /// Canonical config names: "candle" | "http" | "ollama" (alias of http).
    /// Legacy "openai" is accepted with a deprecation warning; internally it
    /// is the same http-adapter sentinel. See normalize_provider.
    pub embedding_provider: Option<String>,
    pub embedding_base_url: String,
    pub embedding_model: String,
    pub embedding_api_key: Option<String>,
    /// None = default store (~/.aikoql/models).
    pub model_dir: Option<String>,
    pub log_level: String,
    pub log_format: String,
    /// [rate_limit] — enforced as a 60s window: per connection on MCP
    /// tools/call (TCP + stdio) and per token on the REST surface.
    pub rate_enabled: bool,
    pub rate_max_calls_per_minute: u64,
    /// [encryption] — encryption-at-rest (MRFC-0020), wired in serve + all
    /// store-opening subcommands via engine::open_kernel.
    pub encryption: RuntimeEncryption,
    /// Backend selection (PR#2 review SE-02): `None` = auto-detect at open
    /// (redb for a fresh/redb path, the native engines for their own
    /// formats — see engine::detect_backend). Any of TOML / env / CLI
    /// naming a backend resolves to `Some` here, so the detection never
    /// overrides an explicit choice.
    pub backend: Option<StorageBackend>,
    /// The TOML path that took effect (for diagnostics).
    pub config_path: Option<String>,
}

/// The storage backends the server can open (docs/STORAGE-BACKENDS.md).
/// Redb is the default for new paths — the stable compatibility engine;
/// aikoql and aikoql-v2 are explicit opt-ins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StorageBackend {
    Redb,
    Aikoql,
    AikoqlV2,
}

impl StorageBackend {
    fn parse(v: &str) -> Result<StorageBackend, String> {
        match v {
            "redb" => Ok(StorageBackend::Redb),
            "aikoql" => Ok(StorageBackend::Aikoql),
            "aikoql-v2" => Ok(StorageBackend::AikoqlV2),
            other => Err(format!(
                "unknown storage backend {other:?}: use \"redb\", \"aikoql\" or \"aikoql-v2\""
            )),
        }
    }
}

/// Merged encryption settings (MRFC-0020). Disabled by default.
#[derive(Clone, Debug, Default)]
pub(crate) struct RuntimeEncryption {
    pub enabled: bool,
    /// Path to the LocalKms key file (v2 envelope, auto-created on first use).
    pub key_path: String,
    /// None = no passphrase configured (fail-closed at open when enabled).
    pub passphrase: Option<String>,
    /// Field-level policies: type_name → fields encrypted on remember.
    pub policies: std::collections::HashMap<String, Vec<String>>,
}

// ---------------------------------------------------------------------------
// TOML schema — every section mirrors aikoql.toml. All fields Option so we
// can layer sections over defaults.
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TomlConfig {
    storage: Option<TomlStorage>,
    database: Option<TomlDatabase>,
    server: Option<TomlServer>,
    encryption: Option<TomlEncryption>,
    rate_limit: Option<TomlRateLimit>,
    embedding: Option<TomlEmbedding>,
    logging: Option<TomlLogging>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TomlStorage {
    backend: Option<String>,
    path: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TomlDatabase {
    path: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TomlServer {
    listen: Option<String>,
    metrics_addr: Option<String>,
    tcp_tokens: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TomlEncryption {
    enabled: Option<bool>,
    key_path: Option<String>,
    passphrase: Option<String>,
    policies: Option<std::collections::HashMap<String, Vec<String>>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TomlRateLimit {
    enabled: Option<bool>,
    max_calls_per_minute: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TomlEmbedding {
    provider: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TomlLogging {
    level: Option<String>,
    format: Option<String>,
}

const LOG_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Canonical user-facing provider names (MVP-008): "candle" (local, default),
/// "http" (any OpenAI-compatible HTTP endpoint, incl. Ollama), "ollama" (alias
/// of http). Legacy "openai" is accepted with a deprecation warning. Internal
/// representation: None = candle, Some("openai") = the http adapter sentinel.
fn normalize_provider(p: &str) -> Result<Option<String>, String> {
    match p {
        "candle" => Ok(None),
        "http" | "ollama" => Ok(Some("openai".to_string())),
        "openai" => {
            eprintln!(
                "warning: embedding provider \"openai\" is deprecated — use \"http\" \
                 (OpenAI-compatible endpoint) or \"ollama\""
            );
            Ok(Some("openai".to_string()))
        }
        other => Err(format!(
            "embedding provider \"{other}\" unknown — use \"candle\", \"http\", or \"ollama\""
        )),
    }
}

/// Find the config file: `--config PATH` flag → `./aikoql.toml` (if present)
/// → `/etc/aikoql/aikoql.toml` (Docker). None if nothing exists.
fn find_toml(args: &[String]) -> Result<Option<String>, String> {
    for i in 0..args.len().saturating_sub(1) {
        if args[i] == "--config" {
            let Some(v) = args.get(i + 1) else {
                return Err("--config requires a path".into());
            };
            if v.starts_with("--") {
                return Err(format!("--config requires a path, got flag {v}"));
            }
            return Ok(Some(v.clone()));
        }
    }
    for p in ["./aikoql.toml", "/etc/aikoql/aikoql.toml"] {
        if std::path::Path::new(p).is_file() {
            return Ok(Some(p.to_string()));
        }
    }
    Ok(None)
}

/// Apply a TOML [encryption] section onto merged settings (shared by `load`
/// and subcommand discovery).
fn apply_toml_encryption(enc: &mut RuntimeEncryption, e: TomlEncryption) {
    enc.enabled = e.enabled.unwrap_or(enc.enabled);
    if let Some(kp) = e.key_path {
        enc.key_path = kp;
    }
    if let Some(p) = e.passphrase {
        enc.passphrase = Some(p);
    }
    if let Some(p) = e.policies {
        enc.policies = p;
    }
}

/// Layering: defaults → TOML → env → CLI. `subcmd == Some("serve")` skips
/// the `serve` token when scanning flags/positionals (mirrors the old main.rs
/// loop); bare `aikoql-mcp [DB]` still works.
pub(crate) fn load(
    args: &[String],
    subcmd: Option<&str>,
    subcmd_idx: Option<usize>,
) -> Result<RuntimeConfig, String> {
    let mut cfg = RuntimeConfig {
        db_path: "./aikoql.redb".into(),
        listen_addr: None,
        metrics_addr: None,
        tcp_tokens: Vec::new(),
        memory_dir: "./memory".into(),
        embedding_provider: None,
        embedding_base_url: "http://localhost:11434".into(),
        embedding_model: String::new(),
        embedding_api_key: None,
        model_dir: None,
        log_level: "info".into(),
        log_format: "text".into(),
        rate_enabled: true,
        rate_max_calls_per_minute: 120,
        encryption: RuntimeEncryption {
            key_path: "./aikoql.key".into(),
            ..Default::default()
        },
        backend: None,
        config_path: None,
    };

    // Layer 2: TOML.
    let toml_path = find_toml(args)?;
    if let Some(path) = &toml_path {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("config {path}: read: {e}"))?;
        let t: TomlConfig = toml::from_str(&raw).map_err(|e| format!("config {path}: {e}"))?;
        cfg.config_path = Some(path.clone());

        if let Some(s) = t.storage {
            if let Some(backend) = s.backend {
                // PR#2 review SE-02: [storage].backend selects the engine
                // through the one pipeline (was rejected unless "redb" —
                // a dead config path).
                cfg.backend = Some(
                    StorageBackend::parse(&backend)
                        .map_err(|m| format!("config {path}: storage.backend {m}"))?,
                );
            }
            if let Some(p) = s.path {
                cfg.db_path = p;
            }
        }
        if let Some(d) = t.database {
            if let Some(p) = d.path {
                cfg.db_path = p; // [database] wins over [storage] (legacy order)
            }
        }
        if let Some(s) = t.server {
            if let Some(v) = s.listen {
                cfg.listen_addr = Some(v);
            }
            if let Some(v) = s.metrics_addr {
                cfg.metrics_addr = Some(v);
            }
            // R2 (review round 3): replacement, not union — each layer
            // overrides the one below it, so revoking a token at a higher
            // layer stops it working.
            if let Some(v) = s.tcp_tokens {
                cfg.tcp_tokens = v;
            }
        }
        if let Some(e) = t.encryption {
            apply_toml_encryption(&mut cfg.encryption, e);
        }
        if let Some(r) = t.rate_limit {
            cfg.rate_enabled = r.enabled.unwrap_or(cfg.rate_enabled);
            cfg.rate_max_calls_per_minute = r
                .max_calls_per_minute
                .unwrap_or(cfg.rate_max_calls_per_minute);
        }
        if let Some(e) = t.embedding {
            if let Some(p) = e.provider {
                cfg.embedding_provider =
                    normalize_provider(&p).map_err(|msg| format!("config {path}: {msg}"))?;
            }
            if let Some(v) = e.base_url {
                cfg.embedding_base_url = v;
            }
            if let Some(v) = e.model {
                cfg.embedding_model = v;
            }
            cfg.embedding_api_key = e.api_key;
        }
        if let Some(l) = t.logging {
            if let Some(v) = l.level {
                if !LOG_LEVELS.contains(&v.as_str()) {
                    return Err(format!(
                        "config {path}: logging.level \"{v}\" unknown — use one of {LOG_LEVELS:?}"
                    ));
                }
                cfg.log_level = v;
            }
            if let Some(v) = l.format {
                if v != "text" && v != "json" {
                    return Err(format!(
                        "config {path}: logging.format \"{v}\" unknown — use \"text\" or \"json\""
                    ));
                }
                cfg.log_format = v;
            }
        }
    }

    // Layer 3: env.
    if let Some(v) = env_opt("AIKOQL_DB") {
        cfg.db_path = v;
    }
    if let Some(v) = env_opt("AIKOQL_BACKEND") {
        cfg.backend = Some(StorageBackend::parse(&v).map_err(|m| format!("AIKOQL_BACKEND: {m}"))?);
    }
    if let Some(v) = env_opt("AIKOQL_LISTEN") {
        cfg.listen_addr = Some(v);
    }
    if let Some(v) = env_opt("AIKOQL_METRICS_ADDR") {
        cfg.metrics_addr = Some(v);
    }
    // R2 (review round 3): an env credential REPLACES the TOML list (one
    // token per env var — role lists use commas, so multi-token env strings
    // would be ambiguous). R4: AIKOQL_TCP_TOKEN_FILE is the production
    // form (docker secrets etc. — keeps the token out of `ps`/inspections)
    // and wins over the inline AIKOQL_TCP_TOKEN var.
    if let Some(path) = env_opt("AIKOQL_TCP_TOKEN_FILE") {
        match std::fs::read_to_string(&path) {
            Ok(v) => {
                let v = v.trim().to_string();
                if v.is_empty() {
                    return Err(format!("AIKOQL_TCP_TOKEN_FILE {path}: file is empty"));
                }
                cfg.tcp_tokens = vec![v];
            }
            Err(e) => return Err(format!("AIKOQL_TCP_TOKEN_FILE {path}: {e}")),
        }
    } else if let Some(v) = env_opt("AIKOQL_TCP_TOKEN") {
        cfg.tcp_tokens = vec![v];
    }
    if let Some(v) = env_opt("AIKOQL_MEMORY_DIR") {
        cfg.memory_dir = v;
    }
    if let Some(v) = env_opt("AIKOQL_EMBEDDING_PROVIDER") {
        cfg.embedding_provider =
            normalize_provider(&v).map_err(|msg| format!("AIKOQL_EMBEDDING_PROVIDER: {msg}"))?;
    }
    if let Some(v) = env_opt("AIKOQL_EMBEDDING_BASE_URL") {
        cfg.embedding_base_url = v;
    }
    if let Some(v) = env_opt("AIKOQL_EMBEDDING_MODEL") {
        cfg.embedding_model = v;
    }
    if let Some(v) = env_opt("AIKOQL_EMBEDDING_API_KEY") {
        cfg.embedding_api_key = Some(v);
    }
    if let Some(v) = env_opt("AIKOQL_MODEL_DIR") {
        cfg.model_dir = Some(v);
    }
    if let Some(v) = env_opt("AIKOQL_PASSPHRASE") {
        cfg.encryption.passphrase = Some(v);
    }

    // Layer 4: CLI (highest precedence). Same semantics as the pre-PRR-4 loop.
    // R2 (review round 3): repeated --tcp-token flags accumulate WITHIN this
    // layer, then replace every lower layer (replacement, not union).
    let mut cli_tcp_tokens: Vec<String> = Vec::new();
    let mut i = if subcmd == Some("serve") {
        let Some(idx) = subcmd_idx else {
            return Err("Usage: aikoql-mcp serve [OPTIONS] [DB]".into());
        };
        idx + 2
    } else {
        1
    };
    while i < args.len() {
        match args[i].as_str() {
            "--listen" => {
                cfg.listen_addr = Some(
                    args.get(i + 1)
                        .cloned()
                        .unwrap_or_else(|| "127.0.0.1:9090".into()),
                );
                i += 2;
            }
            "--tcp-token" => match args.get(i + 1) {
                Some(v) => {
                    cli_tcp_tokens.push(v.clone());
                    i += 2;
                }
                None => {
                    return Err("--tcp-token requires a value: TOKEN[:TENANT[:ROLE1,ROLE2]]".into())
                }
            },
            "--metrics-addr" => {
                cfg.metrics_addr = Some(
                    args.get(i + 1)
                        .cloned()
                        .unwrap_or_else(|| "127.0.0.1:9091".into()),
                );
                i += 2;
            }
            "--memory-dir" => {
                cfg.memory_dir = args
                    .get(i + 1)
                    .cloned()
                    .unwrap_or_else(|| "./memory".into());
                i += 2;
            }
            "--embedding-provider" => {
                let v = args.get(i + 1).cloned().unwrap_or_else(|| "candle".into());
                cfg.embedding_provider =
                    normalize_provider(&v).map_err(|msg| format!("--embedding-provider: {msg}"))?;
                i += 2;
            }
            "--embedding-base-url" => {
                cfg.embedding_base_url = args
                    .get(i + 1)
                    .cloned()
                    .unwrap_or_else(|| "http://localhost:11434".into());
                i += 2;
            }
            "--embedding-model" => {
                cfg.embedding_model = args
                    .get(i + 1)
                    .cloned()
                    .unwrap_or_else(|| "nomic-embed-text".into());
                i += 2;
            }
            "--embedding-api-key" => {
                // justified: missing flag value → empty (no API key)
                cfg.embedding_api_key = Some(args.get(i + 1).cloned().unwrap_or_default());
                i += 2;
            }
            "--model-dir" => {
                cfg.model_dir = args.get(i + 1).cloned();
                if cfg.model_dir.is_none() {
                    return Err("--model-dir requires a value".into());
                }
                i += 2;
            }
            "--backend" => match args.get(i + 1) {
                Some(v) => {
                    cfg.backend =
                        Some(StorageBackend::parse(v).map_err(|m| format!("--backend: {m}"))?);
                    i += 2;
                }
                None => return Err("--backend requires a value: redb|aikoql|aikoql-v2".into()),
            },
            "--config" => {
                // Consumed by find_toml above; skip its value here.
                i += 2;
            }
            _ if args[i].starts_with("--") => {
                return Err(format!(
                    "Unknown option: {} (run `aikoql-mcp help`)",
                    args[i]
                ));
            }
            _ => {
                cfg.db_path = args[i].clone();
                i += 1;
            }
        }
    }

    // R2: any CLI token flags replace every lower layer.
    if !cli_tcp_tokens.is_empty() {
        cfg.tcp_tokens = cli_tcp_tokens;
    }

    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn load_bare(args: &[String]) -> Result<RuntimeConfig, String> {
        load_t(args, None, None)
    }

    /// load() reads process env — serialize every test that goes through it.
    fn load_t(
        args: &[String],
        subcmd: Option<&str>,
        subcmd_idx: Option<usize>,
    ) -> Result<RuntimeConfig, String> {
        let _guard = ENV_LOCK.lock().unwrap(); // justified: Mutex poison is unrecoverable
        load(args, subcmd, subcmd_idx)
    }

    /// Holds ENV_LOCK across the whole set→load→remove window so readers
    /// (via load_t) can't observe a half-applied env mutation. Calls plain
    /// `load` — load_t would deadlock on the held guard.
    fn load_with_env<F: FnOnce() -> Result<RuntimeConfig, String>>(
        f: F,
    ) -> Result<RuntimeConfig, String> {
        let _guard = ENV_LOCK.lock().unwrap(); // justified: Mutex poison is unrecoverable
        f()
    }

    /// Serializes env-mutating tests (process-global state).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    static TOML_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    // Temp toml files written by THIS test thread, swept when the thread
    // exits (the main thread's destructor runs at process exit — statics
    // are NOT dropped on Windows MSVC, TLS is). The files are tiny but used
    // to accumulate in the OS temp dir (3,934 leftovers at last count).
    thread_local! {
        static TEMP_FILES: std::cell::RefCell<TempSweeper> =
            const { std::cell::RefCell::new(TempSweeper { paths: Vec::new() }) };
    }

    struct TempSweeper {
        paths: Vec<std::path::PathBuf>,
    }
    impl Drop for TempSweeper {
        fn drop(&mut self) {
            for p in &self.paths {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    fn tmp_toml(content: &str) -> String {
        let n = TOML_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "aikoql-config-test-{}-{n}.toml",
            std::process::id()
        ));
        std::fs::write(&p, content).unwrap();
        TEMP_FILES.with(|t| t.borrow_mut().paths.push(p.clone()));
        p.to_string_lossy().into_owned()
    }

    /// PR#2 review SE-02 — the backend flows through the one pipeline with
    /// the same precedence as every other knob: default(auto) → TOML → env
    /// → CLI. The review's chain: TOML aikoql, env aikoql-v2, CLI redb ⇒
    /// redb.
    #[test]
    fn backend_precedence_toml_env_cli() {
        // load() directly: load_bare/load_t would deadlock on load_with_env's guard.
        load_with_env(|| {
            let toml_path = tmp_toml("[storage]\nbackend = \"aikoql\"\n");

            std::env::remove_var("AIKOQL_BACKEND");
            let cfg = load(&argv(&["aikoql-mcp", "--config", &toml_path]), None, None).unwrap();
            assert_eq!(cfg.backend, Some(StorageBackend::Aikoql)); // TOML beats default

            std::env::set_var("AIKOQL_BACKEND", "aikoql-v2");
            let cfg = load(&argv(&["aikoql-mcp", "--config", &toml_path]), None, None).unwrap();
            assert_eq!(cfg.backend, Some(StorageBackend::AikoqlV2)); // env beats TOML

            let args = argv(&["aikoql-mcp", "--config", &toml_path, "--backend", "redb"]);
            let cfg = load(&args, None, None).unwrap();
            std::env::remove_var("AIKOQL_BACKEND");
            assert_eq!(cfg.backend, Some(StorageBackend::Redb)); // CLI beats env
            Ok(cfg)
        })
        .unwrap();
    }

    /// SE-02 — the default is auto-detection (None), and a mistyped backend
    /// at any layer fails closed, never silently opens a fresh store.
    #[test]
    fn backend_default_auto_and_unknown_fails_closed() {
        load_with_env(|| {
            std::env::remove_var("AIKOQL_BACKEND");
            let default_cfg = load(&argv(&["aikoql-mcp"]), None, None).unwrap();
            assert_eq!(default_cfg.backend, None);

            std::env::set_var("AIKOQL_BACKEND", "nope");
            let env_err = load(&argv(&["aikoql-mcp"]), None, None).unwrap_err();
            std::env::remove_var("AIKOQL_BACKEND");
            assert!(
                env_err.contains("unknown storage backend"),
                "got: {env_err}"
            );

            let toml_path = tmp_toml("[storage]\nbackend = \"nope\"\n");
            let toml_err =
                load(&argv(&["aikoql-mcp", "--config", &toml_path]), None, None).unwrap_err();
            assert!(
                toml_err.contains("unknown storage backend"),
                "got: {toml_err}"
            );
            Ok(default_cfg)
        })
        .unwrap();
    }

    #[test]
    fn defaults_only() {
        let cfg = load_bare(&argv(&["aikoql-mcp"])).unwrap();
        assert_eq!(cfg.db_path, "./aikoql.redb");
        assert!(cfg.listen_addr.is_none());
        assert!(cfg.embedding_provider.is_none());
        assert_eq!(cfg.log_level, "info");
        assert_eq!(cfg.embedding_base_url, "http://localhost:11434");
    }

    #[test]
    fn toml_overrides_defaults() {
        let p = tmp_toml(
            r#"
[server]
listen = "127.0.0.1:7777"
tcp_tokens = ["a:b:admin", "c:d:viewer"]

[database]
path = "/data/kb"

[embedding]
provider = "openai"
model = "mxbai"

[logging]
level = "debug"
"#,
        );
        let cfg = load_t(&argv(&["aikoql-mcp", "--config", &p]), None, None).unwrap();
        assert_eq!(cfg.listen_addr.as_deref(), Some("127.0.0.1:7777"));
        assert_eq!(cfg.tcp_tokens.len(), 2);
        assert_eq!(cfg.db_path, "/data/kb");
        assert_eq!(cfg.embedding_provider.as_deref(), Some("openai"));
        assert_eq!(cfg.embedding_model, "mxbai");
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.config_path.as_deref(), Some(p.as_str()));
    }

    #[test]
    fn toml_candle_provider_maps_to_default() {
        let p = tmp_toml("[embedding]\nprovider = \"candle\"\n");
        let cfg = load_t(&argv(&["aikoql-mcp", "--config", &p]), None, None).unwrap();
        assert!(cfg.embedding_provider.is_none());
    }

    #[test]
    fn http_and_ollama_providers_map_to_http_adapter() {
        for name in ["http", "ollama"] {
            let p = tmp_toml(&format!("[embedding]\nprovider = \"{name}\"\n"));
            let cfg = load_t(&argv(&["aikoql-mcp", "--config", &p]), None, None).unwrap();
            assert_eq!(cfg.embedding_provider.as_deref(), Some("openai"));
        }
    }

    #[test]
    fn unknown_toml_provider_rejected() {
        let p = tmp_toml("[embedding]\nprovider = \"gemini\"\n");
        let err = load_t(&argv(&["aikoql-mcp", "--config", &p]), None, None).unwrap_err();
        assert!(err.contains("candle") && err.contains("http") && err.contains("ollama"));
    }

    #[test]
    fn env_http_provider_maps_to_http_adapter() {
        let cfg = load_with_env(|| {
            std::env::set_var("AIKOQL_EMBEDDING_PROVIDER", "http");
            let r = load(&argv(&["aikoql-mcp"]), None, None);
            std::env::remove_var("AIKOQL_EMBEDDING_PROVIDER");
            r
        })
        .unwrap();
        assert_eq!(cfg.embedding_provider.as_deref(), Some("openai"));
    }

    #[test]
    fn cli_provider_ollama_maps_and_unknown_rejected() {
        let cfg = load_t(
            &argv(&["aikoql-mcp", "--embedding-provider", "ollama"]),
            None,
            None,
        )
        .unwrap();
        assert_eq!(cfg.embedding_provider.as_deref(), Some("openai"));
        // Unknown values are rejected now — the old loop silently fell back to
        // candle (MVP-008/MVP-009: no silent provider swaps).
        let err = load_t(
            &argv(&["aikoql-mcp", "--embedding-provider", "gemini"]),
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("unknown"));
    }

    #[test]
    fn env_overrides_toml() {
        let p = tmp_toml("[server]\nlisten = \"127.0.0.1:7777\"\n");
        let cfg = load_with_env(|| {
            std::env::set_var("AIKOQL_LISTEN", "127.0.0.1:8888");
            let r = load(&argv(&["aikoql-mcp", "--config", &p]), None, None);
            std::env::remove_var("AIKOQL_LISTEN");
            r
        })
        .unwrap();
        assert_eq!(cfg.listen_addr.as_deref(), Some("127.0.0.1:8888"));
    }

    #[test]
    fn cli_overrides_env_and_toml() {
        let p = tmp_toml("[server]\nlisten = \"127.0.0.1:7777\"\n");
        let cfg = load_with_env(|| {
            std::env::set_var("AIKOQL_LISTEN", "127.0.0.1:8888");
            let r = load(
                &argv(&["aikoql-mcp", "--config", &p, "--listen", "0.0.0.0:9999"]),
                None,
                None,
            );
            std::env::remove_var("AIKOQL_LISTEN");
            r
        })
        .unwrap();
        assert_eq!(cfg.listen_addr.as_deref(), Some("0.0.0.0:9999"));
    }

    #[test]
    fn tcp_token_env_sets_single_token() {
        let cfg = load_with_env(|| {
            std::env::set_var("AIKOQL_TCP_TOKEN", "tok:acme:admin");
            let r = load(&argv(&["aikoql-mcp"]), None, None);
            std::env::remove_var("AIKOQL_TCP_TOKEN");
            r
        })
        .unwrap();
        assert_eq!(cfg.tcp_tokens, vec!["tok:acme:admin".to_string()]);
    }

    // R2 (review round 3): tcp_tokens follow REPLACEMENT precedence, not
    // union — a higher layer must be able to fully retract lower-layer
    // credentials (union means an operator can never revoke a TOML token).

    #[test]
    fn tcp_token_precedence_is_replacement_not_union() {
        let p = tmp_toml("[server]\ntcp_tokens = [\"old:acme:viewer\"]\n");
        let cfg = load_with_env(|| {
            std::env::set_var("AIKOQL_TCP_TOKEN", "new:acme:admin");
            let r = load(&argv(&["aikoql-mcp", "--config", &p]), None, None);
            std::env::remove_var("AIKOQL_TCP_TOKEN");
            r
        })
        .unwrap();
        // env REPLACES the TOML list — the TOML token is gone, not appended.
        assert_eq!(cfg.tcp_tokens, vec!["new:acme:admin".to_string()]);

        // Repeated --tcp-token flags accumulate within the CLI layer, then
        // replace every lower layer (still not a union).
        let cfg = load_with_env(|| {
            std::env::set_var("AIKOQL_TCP_TOKEN", "env:acme:admin");
            let r = load(
                &argv(&[
                    "aikoql-mcp",
                    "--config",
                    &p,
                    "--tcp-token",
                    "cli1:a:viewer",
                    "--tcp-token",
                    "cli2::admin",
                ]),
                None,
                None,
            );
            std::env::remove_var("AIKOQL_TCP_TOKEN");
            r
        })
        .unwrap();
        assert_eq!(
            cfg.tcp_tokens,
            vec!["cli1:a:viewer".to_string(), "cli2::admin".to_string()]
        );
    }

    fn tmp_token_file(content: &str) -> String {
        let n = TOML_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("aikoql-token-test-{}-{n}.txt", std::process::id()));
        std::fs::write(&p, content).unwrap();
        TEMP_FILES.with(|t| t.borrow_mut().paths.push(p.clone()));
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn tcp_token_file_env_reads_and_trims() {
        let f = tmp_token_file("  tok:acme:admin \n");
        let cfg = load_with_env(|| {
            std::env::set_var("AIKOQL_TCP_TOKEN_FILE", &f);
            let r = load(&argv(&["aikoql-mcp"]), None, None);
            std::env::remove_var("AIKOQL_TCP_TOKEN_FILE");
            r
        })
        .unwrap();
        assert_eq!(cfg.tcp_tokens, vec!["tok:acme:admin".to_string()]);
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn tcp_token_file_wins_over_inline_env() {
        let f = tmp_token_file("file:acme:admin\n");
        let cfg = load_with_env(|| {
            std::env::set_var("AIKOQL_TCP_TOKEN_FILE", &f);
            std::env::set_var("AIKOQL_TCP_TOKEN", "inline:acme:viewer");
            let r = load(&argv(&["aikoql-mcp"]), None, None);
            std::env::remove_var("AIKOQL_TCP_TOKEN_FILE");
            std::env::remove_var("AIKOQL_TCP_TOKEN");
            r
        })
        .unwrap();
        assert_eq!(cfg.tcp_tokens, vec!["file:acme:admin".to_string()]);
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn tcp_token_file_missing_is_error() {
        let f =
            std::env::temp_dir().join(format!("aikoql-token-missing-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&f);
        let err = load_with_env(|| {
            std::env::set_var("AIKOQL_TCP_TOKEN_FILE", &f);
            let r = load(&argv(&["aikoql-mcp"]), None, None);
            std::env::remove_var("AIKOQL_TCP_TOKEN_FILE");
            r
        })
        .unwrap_err();
        assert!(err.contains("AIKOQL_TCP_TOKEN_FILE"), "got: {err}");
    }

    #[test]
    fn tcp_token_file_empty_is_error() {
        let f = tmp_token_file("   \n");
        let err = load_with_env(|| {
            std::env::set_var("AIKOQL_TCP_TOKEN_FILE", &f);
            let r = load(&argv(&["aikoql-mcp"]), None, None);
            std::env::remove_var("AIKOQL_TCP_TOKEN_FILE");
            r
        })
        .unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn unknown_toml_key_rejected() {
        let p = tmp_toml("[server]\nbogus_key = 1\n");
        let err = load_t(&argv(&["aikoql-mcp", "--config", &p]), None, None).unwrap_err();
        assert!(err.contains("bogus_key"), "got: {err}");
    }

    #[test]
    fn rocksdb_backend_rejected() {
        let p = tmp_toml("[storage]\nbackend = \"rocksdb\"\n");
        let err = load_t(&argv(&["aikoql-mcp", "--config", &p]), None, None).unwrap_err();
        assert!(err.contains("rocksdb"), "got: {err}");
    }

    #[test]
    fn encryption_defaults_disabled() {
        let cfg = load_bare(&argv(&["aikoql-mcp"])).unwrap();
        assert!(!cfg.encryption.enabled);
        assert_eq!(cfg.encryption.key_path, "./aikoql.key");
        assert!(cfg.encryption.passphrase.is_none());
        assert!(cfg.encryption.policies.is_empty());
    }

    #[test]
    fn subcommand_pipeline_sees_env_settings() {
        // R10 (review round 3): the subcommand open path (engine.rs
        // open_kernel_auto) runs the SAME load() pipeline as `serve` — env
        // encryption settings must reach it.
        let _guard = ENV_LOCK.lock().unwrap(); // justified: Mutex poison is unrecoverable
        std::env::set_var("AIKOQL_PASSPHRASE", "discover-test");
        let cfg = load(&[], None, None);
        std::env::remove_var("AIKOQL_PASSPHRASE");
        let enc = cfg.unwrap().encryption;
        assert_eq!(enc.passphrase.as_deref(), Some("discover-test"));
    }

    #[test]
    fn encryption_toml_parsed() {
        let p = tmp_toml(
            r#"
[encryption]
enabled = true
key_path = "/k/kf"
passphrase = "from-toml"

[encryption.policies]
employee = ["salary", "ssn"]
"#,
        );
        let cfg = load_t(&argv(&["aikoql-mcp", "--config", &p]), None, None).unwrap();
        assert!(cfg.encryption.enabled);
        assert_eq!(cfg.encryption.key_path, "/k/kf");
        assert_eq!(cfg.encryption.passphrase.as_deref(), Some("from-toml"));
        assert_eq!(
            cfg.encryption.policies.get("employee"),
            Some(&vec!["salary".to_string(), "ssn".to_string()])
        );
    }

    #[test]
    fn encryption_passphrase_env_overrides_toml() {
        let p = tmp_toml("[encryption]\nenabled = true\npassphrase = \"from-toml\"\n");
        let cfg = load_with_env(|| {
            std::env::set_var("AIKOQL_PASSPHRASE", "from-env");
            let r = load(&argv(&["aikoql-mcp", "--config", &p]), None, None);
            std::env::remove_var("AIKOQL_PASSPHRASE");
            r
        })
        .unwrap();
        assert!(cfg.encryption.enabled);
        assert_eq!(cfg.encryption.passphrase.as_deref(), Some("from-env"));
    }

    #[test]
    fn bad_log_level_rejected() {
        let p = tmp_toml("[logging]\nlevel = \"verbose\"\n");
        let err = load_t(&argv(&["aikoql-mcp", "--config", &p]), None, None).unwrap_err();
        assert!(err.contains("logging.level"), "got: {err}");
    }

    #[test]
    fn env_and_cli_layers_apply_across_sections() {
        let p = tmp_toml(
            "[database]\npath = \"/toml/db\"\n[embedding]\nprovider = \"candle\"\n[server]\nmetrics_addr = \"127.0.0.1:1000\"\n",
        );
        let cfg = load_with_env(|| {
            std::env::set_var("AIKOQL_DB", "/env/db");
            std::env::set_var("AIKOQL_EMBEDDING_PROVIDER", "openai");
            let r = load(
                &argv(&[
                    "aikoql-mcp",
                    "--config",
                    &p,
                    "--metrics-addr",
                    "127.0.0.1:2000",
                ]),
                None,
                None,
            );
            std::env::remove_var("AIKOQL_DB");
            std::env::remove_var("AIKOQL_EMBEDDING_PROVIDER");
            r
        })
        .unwrap();
        assert_eq!(cfg.db_path, "/env/db"); // env > TOML (database)
        assert_eq!(cfg.embedding_provider.as_deref(), Some("openai")); // env > TOML (embedding)
        assert_eq!(cfg.metrics_addr.as_deref(), Some("127.0.0.1:2000")); // CLI > all (server)
    }

    #[test]
    fn rate_limit_toml_parsed() {
        let p = tmp_toml("[rate_limit]\nenabled = false\nmax_calls_per_minute = 5\n");
        let cfg = load_t(&argv(&["aikoql-mcp", "--config", &p]), None, None).unwrap();
        assert!(!cfg.rate_enabled);
        assert_eq!(cfg.rate_max_calls_per_minute, 5);
    }

    #[test]
    fn positional_db_path_still_works() {
        let cfg = load_bare(&argv(&["aikoql-mcp", "./my.redb"])).unwrap();
        assert_eq!(cfg.db_path, "./my.redb");
    }
}
