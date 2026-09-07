---
title: aikoql
description: The Agent-First Knowledge Database
---

# aikoql

**The Knowledge Operating System for AI agents.**

aikoql is an embedded knowledge database that treats **everything as a Knowledge Object** — data, programs, workflows, policies, agents, prompts, and connectors all share the same lifecycle: identity, versioning, provenance, access control, audit trail.

```bash
# 5 seconds to start
npm install -g aikoql-mcp
aikoql-mcp shell :memory:
```

## Why aikoql?

| Problem | Aikoql Solution |
|---|---|
| AI agents need schema discovery before queries | `GET /api/v1/schema` — auto-discovers types and properties |
| Agents retry — idempotency matters | Every mutation has a `idempotency_key` |
| Programs should be versioned like data | Programs are Knowledge Objects — deploy, version, execute, audit |
| Knowledge needs provenance | SHA-256 audit chain. Every mutation is a `KnowledgeEvent` |
| Sensitive data needs encryption | AES-256-GCM + ChaCha20-Poly1305 at rest. Field-level policies via `[encryption]` TOML |
| Agents need to know *what's true, since when, and why* | Epistemic status model + valid-time + evidence lineage on every Knowledge Object (v0.3) |
| Data comes from many sources | 4 connectors: PostgreSQL, SQLite, MongoDB, Neo4j |

## Core Capabilities

### For AI Agents

- **MCP Protocol** — Tools over stdio or TCP. Autodiscovery via `tools/list`.
- **Agent Knowledge Interface** — Pre-compile knowledge so agents spend tokens on problem-solving, not discovery.
- **Context Compiler** — 40-60% reduction in agent discovery tokens. Compiles code + docs into minimum sufficient context.
- **Schema Discovery** — Agents learn what types and properties exist before composing queries.
- **Idempotent Mutations** — Safe to retry. Same `idempotency_key` = exact-once commit.
- **REST API** — 40+ endpoints with JSON, Bearer auth, OpenAPI 3.0 spec.
- **4 SDKs** — Python (PyO3), TypeScript, Go, Java.
- **Knowledge Transactions (v0.3)** — observe, assert, verify, contradict, supersede, merge, invalidate, resolve — every operation evidence-backed and lineage-stamped.
- **Agent Experience Reuse (v0.3)** — record execution outcomes as TTL-bounded experience KOs; match them to new tasks under reuse conditions; injected into `compile_context`.

### For Knowledge Engineers

- **aikoql** — Purpose-built query language for knowledge graphs.
  ```aikoql
  MATCH Employee WHERE dept == "Engineering" RETURN name, salary
  MATCH Employee AS_OF 1724025600000 RETURN *          # what was true then
  MATCH Employee HISTORICAL RETURN *                   # every committed version
  MATCH Employee EPISTEMIC verified RETURN *           # only verified knowledge
  ```
- **Hybrid Search** — Vector (HNSW) + text (BM25) with RRF fusion.
- **Graph Traversal** — Relationship-first queries with depth and direction.
- **Programs-as-KOs** — Deploy aikoql programs as versioned objects. Execute with `{{param}}` substitution.
- **Knowledge Compiler** — Markdown + Rust code → KnowledgeIr. Multi-source merging. Staleness detection.
- **Document Pipeline** — Upload PDF/DOCX/Markdown → OCR → structure analysis → queryable Knowledge Objects.
- **Connector Bridge** — PostgreSQL, SQLite, MongoDB, Neo4j schemas → KnowledgeIr conversion.
- **Constraint Engine** — Property types, uniqueness, cardinality, domain/check constraints, programmable constraints.
- **Change Reconciliation** — Git diff → affected entities → auto-proposals → validate → apply.
- **Epistemic + Temporal Model (v0.3)** — 7-state constrained transition table (observed → asserted → verified → contradicted → superseded), valid-time extensions, supersession stamps.
- **Evidence & Derivation Lineage (v0.3)** — mandatory evidence on observation/assertion/verification; first-class `Derivation` + `ConfidenceContext`; `trace()` answers WHY / FROM WHAT / HOW / BY WHOM / WHEN / WITH WHICH EVIDENCE.

### For Operations

- **Encryption at Rest** — AES-256-GCM + ChaCha20-Poly1305. Envelope encryption (KEK → DEK → Data).
- **Multi-Tenancy** — Tenant-aware quotas, per-tenant encryption keys.
- **RBAC** — Roles, policies, ACLs. Policies are themselves Knowledge Objects.
- **Backup/Restore** — Verified backups with PITR metadata.
- **Graph Browser** — Neo4j-style visualization with tenant filtering, aikoql query runner.

## Architecture

```
Knowledge Objects (passive + active)
        ↓
Knowledge Runtime (Compiler → KVM · Orchestrator · Policy Engine)
        ↓
Knowledge Kernel (MVCC · OCC · HLC · RBAC · Audit · CDC · Epistemic · Valid-Time · Evidence)
        ↓
Storage Kernel (aikoql-v2 LSM · redb · EncryptedStore)
```

**Everything is a Knowledge Object.** Data, programs, workflows, policies, triggers, agents — unified lifecycle.

## Quick Links

- [Getting Started](/docs/getting-started) — Install and run in 5 minutes
- [API Reference](/docs/api-reference) — All MCP tools + 40+ REST endpoints
- [Architecture](/docs/architecture) — Deep dive into the Knowledge OS
- [Benchmarks](/docs/benchmarks) — Certified cross-engine matrix (W1–W8 + resources)
- [Development](/docs/development) — Build, test and extend from source
- [Programs-as-KOs](/docs/guides/programs) — Deploy, execute, version, audit
- [Encryption](/docs/guides/encryption) — AES-256-GCM setup and key management
- [Import Data](/docs/guides/import) — PostgreSQL, SQLite, MongoDB, Neo4j connectors

## License

Apache 2.0. Open source. Free for commercial use.
