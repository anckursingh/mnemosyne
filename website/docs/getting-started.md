---
title: Getting Started
description: Install and run aikoql in 5 minutes
---

# Getting Started

## CLI Commands

```
aikoql comes with 10 CLI commands:
  shell [DB]             Interactive knowledge shell
  serve [OPTIONS] [DB]   Start MCP server (stdio by default; --listen for TCP)
  ingest-dir [PATH] [DB] Ingest directory into knowledge base
  report [PATH]          Print knowledge report for directory (read-only)
  backup [DB]            Create verified backup
  restore BACKUP [DB]    Restore from backup
  audit [DB]             Print encryption compliance report
  keygen [PATH]          Generate master encryption key
  import <SOURCE> [ARGS] Import from postgres / sqlite / mongodb
  model install [MODEL]  Install an embedding model for offline use
```

### Ingest a Codebase

```bash
# Analyze any directory without storing (read-only report)
aikoql report ~/my-project

# Ingest and store as Knowledge Objects
aikoql ingest-dir ~/my-project ./kb
```

A fresh path creates an **aikoql-v2** database directory — the default since
2026-09-07. Existing `*.redb` files still open as redb and a v1 WAL still
opens as v1: auto-detection never reinterprets an existing database.

Every entity (file, module, function, test, section) becomes its own Knowledge
Object with kernel relationships between them (`depends_on`, `implements`,
`tested_by`). The directory is stored as a parent KO that `contains` one File
KO per source file, and each File KO `contains` its entities — every entity is
reachable from the root of the graph. Re-ingesting the same path is idempotent
— it updates in place.

The ingest engine classifies every file:
- `.md` → Markdown Knowledge Compiler (sections, ADRs, facts)
- `.rs` → Rust Code Parser (DEPENDS_ON, IMPLEMENTS, TESTED_BY)
- Mixed sources → Merged + deduplicated + staleness-checked

## Installation

### npm (recommended for MCP clients)

```bash
npm install -g aikoql-mcp
aikoql-mcp --version
```

First run downloads the pinned platform binary from GitHub Releases and
verifies its SHA-256 — npm@X always executes native binary X.

### Download Binary

aikoql ships as a single, self-contained binary. No dependencies, no installers.

**Windows:**
```bash
curl -LO https://github.com/anckursingh/aikoql/releases/download/v0.1.19/aikoql-mcp.exe
.\aikoql-mcp.exe --help
```

**Linux (static musl — any distro):**
```bash
curl -LO https://github.com/anckursingh/aikoql/releases/download/v0.1.19/aikoql-mcp-linux-musl
chmod +x aikoql-mcp-linux-musl && mv aikoql-mcp-linux-musl /usr/local/bin/aikoql
```

A glibc build (`aikoql-mcp-linux`) is also available for distros that prefer dynamic linking.

**macOS (Apple Silicon / Intel):**
```bash
# Apple Silicon
curl -LO https://github.com/anckursingh/aikoql/releases/download/v0.1.19/aikoql-mcp-macos-arm64
chmod +x aikoql-mcp-macos-arm64 && mv aikoql-mcp-macos-arm64 /usr/local/bin/aikoql

# Intel
curl -LO https://github.com/anckursingh/aikoql/releases/download/v0.1.19/aikoql-mcp-macos
chmod +x aikoql-mcp-macos && mv aikoql-mcp-macos /usr/local/bin/aikoql
```

### Verify

```bash
aikoql --version
# aikoql-mcp 0.1.19
```

## 5-Second Start

### Interactive Shell

```bash
aikoql shell :memory:
```
```
aikoql> CREATE Person name == "Alice", role == "Architect"
Created: 019fdc... (v1)

aikoql> MATCH Person RETURN *
── 1 row(s) ──
  019fdc...  v1  Person   Alice Architect

aikoql> .tables
  Person

aikoql> .exit
Bye.
```

### MCP Server (stdio mode)

```bash
aikoql serve ./my-knowledge
```

Connects via stdin/stdout — perfect for Claude Code, VS Code, and other MCP clients. Add to your MCP config:

```json
{
  "mcpServers": {
    "aikoql": {
      "command": "aikoql",
      "args": ["serve", "./my-knowledge"]
    }
  }
}
```

### TCP Server + Web UI

TCP mode requires at least one auth token (`TOKEN[:TENANT[:ROLE1,ROLE2]]`); clients
pass it as `params.token` to MCP `initialize`.

```bash
aikoql serve --listen 127.0.0.1:9090 --tcp-token mytoken:acme:admin \
  --metrics-addr 127.0.0.1:9091 ./my-knowledge
```

- MCP endpoint: `tcp://127.0.0.1:9090` (token auth required)
- Graph Browser: `http://127.0.0.1:9091/ui`
- REST API: `http://127.0.0.1:9091/api/v1/`
- Health check: `http://127.0.0.1:9091/health`

## First Commands

### Using the REST API

```bash
# Create an object
curl -X POST http://127.0.0.1:9091/api/v1/remember \
  -H 'Content-Type: application/json' \
  -d '{"type_name":"Note","properties":{"body":"Hello aikoql"}}'

# Search
curl -X POST http://127.0.0.1:9091/api/v1/aikoql \
  -H 'Content-Type: application/json' \
  -d '{"query":"MATCH Note RETURN *"}'

# Schema discovery
curl http://127.0.0.1:9091/api/v1/schema
```

### Using the Shell

```bash
# Open a database (fresh path = aikoql-v2 directory)
aikoql shell ./kb

# Create objects
aikoql> CREATE Employee name == "Alice", dept == "Engineering", salary == 125000

# Search
aikoql> MATCH Employee WHERE dept == "Engineering" RETURN name, salary

# Backup
aikoql> .backup

# See all commands
aikoql> .help
```

## Embeddings (Optional)

Semantic search (`MATCH ... USING EMBEDDING`) runs fully offline — the server
never downloads models at runtime. Install the bundled model once:

```bash
aikoql model install
```

Without an installed model, `serve` still starts and `/health` reports
`semantic: {"state": "unavailable"}` together with the install command.
Point `--embedding-provider http` + `--embedding-base-url` at an
Ollama/OpenAI-compatible endpoint to use a remote model instead
(`ollama` is an alias of `http`; providers are `candle` | `http` | `ollama`).

## Connecting from Code

### Python
```python
import aikoql_py
kernel = aikoql_py.Kernel.open("./kb")  # fresh path = aikoql-v2 database directory
result = kernel.remember({"type_name": "Note", "properties": {"body": "Hello"}})
```

### TypeScript
```typescript
import { AikoqlClient } from 'aikoql-sdk';
const client = new AikoqlClient({ command: './aikoql' });
await client.connect();
await client.remember({ type_name: 'Note', properties: { body: 'Hello' } });
```

### Go
```go
import "github.com/ancku/aikoql-sdk"
client := aikoql.NewClient("127.0.0.1:9090")
client.Connect()
result, _ := client.Remember(map[string]interface{}{"type_name": "Note"})
```

## Encryption (Optional)

Enable encryption at rest (v0.2 — wired end-to-end into `serve`):

```bash
# Generate a master key (v2 envelope; passphrase from env, or generated + printed once)
export AIKOQL_PASSPHRASE="your-secure-passphrase"
aikoql keygen ./aikoql.key

# Configure encryption + field-level policies
cat > aikoql.toml <<'EOF'
[encryption]
enabled = true
key_path = "./aikoql.key"

[encryption.policies]
employee = ["salary", "ssn"]
EOF

# Start — wrong/missing passphrase fails closed, never silent plaintext
aikoql serve --listen :9090 --tcp-token mytoken:acme:admin --metrics-addr :9091 ./encrypted-kb
```

See [Encryption Guide](/docs/guides/encryption) for details on key hierarchy, field-level encryption, restart behavior, and compliance.

## Storage Backends

Three engines behind one `StorageEngine` trait, default **aikoql-v2**:

| backend | profile |
|---|---|
| `aikoql-v2` | the default — segmented LSM, bounded memory + recovery, best write-mixed throughput |
| `aikoql` | read-hot, RAM-affordant, bounded dataset |
| `redb` | compatibility fallback — snapshots and pre-v2 files |

Select with `--backend`, `AIKOQL_BACKEND`, or `storage.backend` in TOML; the
default auto-detects what is already at the path. Details in
[Architecture](/docs/architecture), numbers in [Benchmarks](/docs/benchmarks).

## Next Steps

- [Architecture Overview](/docs/architecture) — Understanding the Knowledge OS
- [Benchmarks](/docs/benchmarks) — The certified cross-engine matrix
- [API Reference](/docs/api-reference) — All endpoints and tools
- [Import Data](/docs/guides/import) — PostgreSQL, SQLite, MongoDB, Neo4j
- [Programs-as-KOs](/docs/guides/programs) — Deploy your first knowledge program
- [Development](/docs/development) — Build and test from source
