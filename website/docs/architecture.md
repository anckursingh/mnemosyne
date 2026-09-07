---
title: Architecture
description: The Knowledge Operating System architecture
---

# Architecture

## The Knowledge OS Stack

aikoql is organized as a layered operating system for knowledge:

```
┌──────────────────────────────────────────────┐
│           ACTIVE KNOWLEDGE OBJECTS            │
│  Program · Workflow · Agent · Policy          │
│  Prompt · Trigger · Connector · Benchmark     │
├──────────────────────────────────────────────┤
│           KNOWLEDGE RUNTIME                   │
│  Compiler → KVM · Orchestrator · Policy Engine│
├──────────────────────────────────────────────┤
│           KNOWLEDGE KERNEL                    │
│  MVCC · OCC · HLC · RBAC · Audit · CDC        │
├──────────────────────────────────────────────┤
│           STORAGE KERNEL                      │
│  aikoql-v2 LSM (default) · redb · EncryptedStore│
└──────────────────────────────────────────────┘
```

## Core Design Principle

> **Everything is a Knowledge Object.**

Inspired by three landmark systems:

| System | Abstraction | Everything is a... |
|---|---|---|
| Git | Object | Commit, Blob, Tree, Tag |
| Kubernetes | Resource | Deployment, Service, ConfigMap |
| Unix | File | Data, Device, Socket, Process |
| **aikoql** | **Knowledge Object** | Data, Program, Policy, Agent, Trigger |

A Knowledge Object has:
- **Identity** — immutable KOID
- **Versioning** — MVCC, every change is a new version
- **Provenance** — who created it, when, why
- **Access Control** — who can read/write/execute it
- **Dependencies** — which schemas, programs, ontologies it depends on
- **Events** — every mutation is a KnowledgeEvent
- **Audit Trail** — SHA-256 hash chain, independently verifiable

## Knowledge Kernel

### Transaction Pipeline

```
RememberRequest → Validation → OCC Check → HLC Assignment → Write Batch → Journal → Ack
```

- **MVCC** — Multi-Version Concurrency Control. Readers never block writers.
- **OCC** — Optimistic Concurrency Control. Conflicts detected deterministically.
- **HLC** — Hybrid Logical Clock. Causally consistent timestamps without NTP dependency.
- **SHA-256 Audit Chain** — Every commit extends the journal hash. Tamper-evident.

### Knowledge Lifecycle (v0.3)

The kernel now treats knowledge as a versioned, evidence-backed, evolving object:

- **Epistemic Model** — Every KO carries an epistemic status (`observed`, `extracted`, `asserted`, `inferred`, `verified`, `contradicted`, `superseded`) over a constrained 7-state / 19-move transition table, with append-only epistemic history. Status changes happen only through the semantic ops (`observe`, `assert_knowledge`, `verify_knowledge`, `contradict`, `supersede`, `merge`, `invalidate`, `resolve_conflict`) — the kernel's generic transition primitive is library-level only and is not exposed on any protocol surface.
- **Valid-Time** — `valid_from` / `valid_to` extensions (half-open `valid_at`) alongside MVCC commit-time. Supersession stamps `valid_to = now` and wires a SUPERSEDES edge. Default `MATCH` filters to facts valid now.
- **Evidence** — Canonical evidence extension (source_artifact, location, revision, method, confidence). Mandatory on observe / assert / verify / invalidate — unbacked mutations are rejected at the kernel boundary.
- **Derivation & Confidence** — First-class `Derivation` (operation, actor, model, timestamp, sources, reason) + `ConfidenceContext` (score, confirmations, last_verified). `kernel.derive()` wires DERIVED_FROM edges; invalidation sweeps every dependent via BFS and stamps it stale.
- **Knowledge Transactions** — observe, assert_knowledge, verify_knowledge, contradict (→ `aikoql:conflict` KO), supersede, merge, invalidate, resolve_conflict — all committed under one pipe lock, all lineage-stamped.
- **Agent Experience** — `aikoql:experience` KOs (TTL-bounded valid-time, evidence-backed, confidence context); reuse matching gated by `reuse_conditions`, ACL-scoped; `compile_context` injects matched lessons into the next agent's context package.

### Storage

One `StorageEngine` trait, three engines, ratified default **aikoql-v2**
(2026-09-07 ADR, `docs/STORAGE-ENGINE-ARCHITECTURE-DECISION.md`):

- **aikoql-v2 (default)** — Segmented LSM tree written in Rust: group-commit
  WAL → memtable → immutable segments (bloom filters, block index, v4 dense
  entry cadence) → size-tiered compaction with relocation. On top of the LSM
  sits an **identity/placement layer**: every object has a LogicalId separate
  from where a copy physically lives (ReplicaId → placement directory) — the
  foundation for sharding and replication without a format rewrite. Bounded
  memory (LRU block cache) and bounded recovery (directory checkpoints: at
  600K updates, open = 75 ms / 1.7 MB vs 348 ms / 41 MB full replay).
- **redb** — Copy-on-write B-tree, single file. Compatibility fallback:
  snapshots and pre-v2 databases keep opening as redb.
- **aikoql v1** — WAL + RAM mirror. Read-hot profile for RAM-affordant
  deployments with bounded datasets.
- **EncryptedStore** — Wraps any engine with AES-256-GCM.

The default auto-detects the format at the path: a fresh path creates
aikoql-v2, an existing redb file opens as redb, a v1 WAL opens as v1 — an
upgrade never reinterprets an existing database. The certified cross-engine
matrix (W1–W8, resources, gate status) lives in [Benchmarks](/docs/benchmarks).

### Event System (CDC)

Every mutation emits a `KnowledgeEvent`:
```
Create → Created event
Update → Updated event
Delete → Forgotten event
Lifecycle → Evolved event
```

Subscribers receive events via durable subscriptions with replay and checkpoint.

## Knowledge Runtime

### Compiler Pipeline

```
Aikoql Source → Lexer → Parser → AST → Semantic Analyzer → KIR → Planner → Runtime
```

### Planner Optimizations

1. **Filter Merge** — Consecutive Filters combined into one
2. **Filter Pushdown** — Filters pushed before expensive Search operators
3. **Scan Dedup** — Duplicate Scans on the same type removed (cross-program fusion)

### KVM — Knowledge Virtual Machine

```
Program KO (aikoql)
    ↓
Compiler → Knowledge IR (KIR)
    ↓
Planner → Optimized IR
    ↓
Interpreter → RowSet
```

v1 is a tree-walking interpreter. JIT compilation (Cranelift) and WASM support are post-1.0.

## Active Knowledge Objects (MRFC-0030)

4 tiers of executable artifacts, all KOs:

| Type | Purpose |
|---|---|
| `aikoql:program` | aikoql code as versioned KO |
| `aikoql:workflow` | DAG of programs |
| `aikoql:policy` | RBAC rule as KO |
| `aikoql:trigger` | Event → Condition → Action |
| `aikoql:agent` | AI agent with prompt + memory + tools |
| `aikoql:connector` | Import/export plugin definition |

Every Active KO shares the same lifecycle as data: identity, versioning, provenance, access control, audit.

## Encryption (MRFC-0020)

```
Application Encryption (optional)
    ↓
Knowledge Encryption (field/object level)
    ↓
Storage Encryption (page/WAL level)
    ↓
Disk Encryption (OS-provided)
```

- **AES-256-GCM** — Primary cipher. Cipher-cached for performance (16.6% overhead).
- **ChaCha20-Poly1305** — Secondary cipher for crypto agility.
- **Envelope Encryption** — Key Encryption Key (v2 envelope key file) wraps per-tenant Data Encryption Keys; wrapped DEKs are persisted inside the store so encrypted data survives restarts.
- **Wired into `serve` (v0.2)** — `[encryption]` TOML config (`enabled` / `key_path` / `passphrase` / `policies`), `AIKOQL_PASSPHRASE` env (beats TOML), `aikoql keygen` writes the v2 envelope. Missing/wrong passphrase fails closed — never silent plaintext.
- **Field-Level** — Mark specific properties as encrypted per schema type (`[encryption.policies]`); field name as AAD.
- **Key Rotation** — Online KEK rotation is descoped in v0.2 (no production caller; would require full-store re-encrypt).

## Agent Knowledge Interface (MRFC-0070)

The A0-A10 pipeline that pre-compiles knowledge for AI agents:

```
Source Files (.md, .rs) → Compilers (A1/A2) → KnowledgeIr
    ↓
Multi-Source Merge (A3) → Staleness Detection (A4)
    ↓
Context Compiler (A5) → MCP compile_context (A6)
    ↓
Agent Gateway (A7): audit, RBAC, rate limiting, PII filter
    ↓
Change Reconciliation (A8): git diff → entities → proposals → apply
    ↓
Connector Bridge (A9): DB schemas → KnowledgeIr
```

10 knowledge primitives: Entity, Artifact, Relationship, Claim, Rule, Requirement, Decision, Task, Evidence, Event — with cross-cutting metadata (Scope, Authority, Confidence, Provenance, Temporal, Version) on every Knowledge Object.

### Agent Experience Flow (v0.3)

A0–A10 feeds the knowledge lifecycle layer:

```
record_experience / find_experiences (K5) → reuse-matched lessons
    ↓
compile_context (A6) → "Previous Agent Experience" injected into the context package
```

Knowledge transactions (K4: observe / assert / verify / contradict / supersede / merge / invalidate / resolve) and derivation (K3) keep the knowledge base evolving with lineage; the epistemic model (K1) and valid-time (K2) make *what is true, since when, and why* queryable — `AS_OF`, `BETWEEN`, `HISTORICAL`, `EPISTEMIC`.

## Document Knowledge Compiler (D1-D9)

```
Upload (PDF/DOCX/MD) → D1-D3 (OCR, AST, Classify)
    → D4-D6 (IR, Ontology, Resolution)
        → D7-D9 (Commit, Status, Studio)
```

195 tests. Pipeline status tracked end-to-end.

## Protocol Surface

| Entry Point | Protocol | Use Case |
|---|---|---|
| `aikoql serve` | MCP (JSON-RPC) over stdio/TCP | AI agents |
| `:9091/api/v1/*` | REST (HTTP/JSON) | Web apps, curl (40+ endpoints) |
| `:9091/studio` | Studio SPA (14 panels) | Full management UI |
| `aikoql shell` | Interactive REPL (9 commands) | Human queries |
| `:9091/health` | HTTP health check | Kubernetes probes |
| `:9091/metrics` | Prometheus text format | Monitoring |

## Constraint Engine (MRFC-0060, ~95%)

```
Remember → Property Types → Uniqueness → Cardinality → Domain → Check → Programmable → Commit
```

9-phase constraint system:
- **C1-C3:** Property types, uniqueness, cardinality constraints with OntologyRegistry wiring
- **C4-C5:** Domain constraints + check constraints with transaction-aware deferred evaluation
- **C6-C7:** Constraint dependency graph with write-set filtering + connector pushdown capability
- **C8-C9:** Constraint inference engine + programmable constraints (Arith, If, expressions)

## Crate Map

```
crates/
├── kernel/           Knowledge Kernel (MVCC, OCC, HLC, RBAC, audit)
├── storage/          Storage engines behind one StorageEngine trait
│   ├── aikoql-v2/    Segmented LSM (default): WAL + memtable + tiered
│   │                 compaction, identity/replica/placement directories,
│   │                 checkpoint-bounded recovery
│   └── aikoql/       v1 WAL + RAM mirror (read-hot profile)
├── compiler/         aikoql parser, semantic analyzer, planner
├── runtime/          Physical plan interpreter
├── constraints/      Constraint engine (C1-C9, ~95% complete)
├── engines/
│   ├── graph/        Relationship index + BFS traversal
│   ├── vector/       HNSW + Tantivy (BM25) hybrid search
│   └── scheduler/    Background jobs (index, compaction, rotation)
├── services/
│   ├── api/mcp/      MCP server + REST API (40+ endpoints) + Studio UI (14 panels)
│   ├── reasoning/    If-then rule engine (first-class derive: premises → DERIVED_FROM)
│   ├── semantic/     AI embedding enrichment
│   └── ingestion/    Document ingestion pipeline (D1-D9) + knowledge compilers (A1-A9)
├── connectors/
│   ├── postgres/     PostgreSQL import
│   ├── sqlite/       SQLite import
│   ├── mongodb/      MongoDB import
│   └── neo4j/        Neo4j import
├── sdk/
│   ├── python/       PyO3 native bindings
│   ├── typescript/   MCP JSON-RPC client
│   ├── go/           TCP JSON-RPC client
│   └── java/         Zero-dependency JSON-RPC client
└── cluster/proxy/    Multi-shard proxy with retry/backoff
```

## Dependencies

**Zero external runtime dependencies.** The binary is a single self-contained file:
- Windows: 3.4 MB (PE32+ x86-64)
- Linux: 3.7 MB (ELF64 static musl, no glibc)
- Embedded database (aikoql-v2 segmented LSM by default; redb and aikoql v1 as profile engines) — no external DB server required
