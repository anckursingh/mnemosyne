---
title: Development
description: Build, test and extend aikoql from source
---

# Development Guide

## Prerequisites

- **Rust** — stable toolchain. On Windows: MSVC (Build Tools) — the storage
  engine intentionally avoids asm-shim dependencies that fail on MSVC.
- **Node.js** — only for the TypeScript SDK and npm packaging.
- **Python 3.9+** — only for the PyO3 SDK (`crates/sdk/python`).

No database server, no service dependencies. Everything is embedded.

## Clone & Build

```bash
git clone https://github.com/anckursingh/aikoql.git
cd aikoql
cargo build --release
# binary: target/release/aikoql-mcp(.exe)
```

## Test

```bash
cargo test --workspace        # full suite (245+ tests across ~60 binaries)
cargo fmt --all --check       # formatting guardrail
cargo clippy --workspace --all-targets -- -D warnings   # lint guardrail
```

Guardrails are the definition of done: no milestone is complete without a
green fmt + clippy + suite run.

## Run from Source

```bash
cargo run -p aikoql-mcp -- serve ./kb        # fresh path = aikoql-v2 directory
cargo run -p aikoql-mcp -- shell :memory:
```

The backend knob (`--backend redb | aikoql | aikoql-v2`) and auto-detection
contract live in `docs/STORAGE-BACKENDS.md`.

## Project Layout

```
crates/
├── kernel/           Knowledge Kernel (MVCC, OCC, HLC, RBAC, audit)
├── storage/          aikoql-v2 (segmented LSM, default) + aikoql v1 (WAL+mirror)
├── compiler/         aikoql parser, semantic analyzer, planner
├── runtime/          Physical plan interpreter
├── constraints/      Constraint engine (C1-C9)
├── engines/          graph, vector, scheduler
├── services/         api/mcp (MCP + REST + Studio), reasoning, semantic, ingestion
├── connectors/       postgres, sqlite, mongodb, neo4j
├── sdk/              python, typescript, go, java
└── cluster/proxy/    Multi-shard proxy
```

See [Architecture](/docs/architecture) for the full crate map and the
[Benchmarks](/docs/benchmarks) for the certified numbers.

## Engineering Conventions

- **TDD per milestone.** Storage-engine work ships as SE2-M0…M40 milestones,
  each with its own test suite (`crates/storage/aikoql-v2/tests/`) — every
  capability is pinned by a test before the code exists (RED → GREEN).
- **Evidence packs.** Measured results land in `artifacts/` (workload
  matrices, checkpoint probes, crash-window certs); the certification history
  lives in `docs/TESTING-PLAN-V2.md` and
  `docs/testing/STORAGE_ENGINE_MVP_CERTIFICATION_CLOSURE.md`.
- **Architecture decisions** are recorded as ADRs in `docs/` (see
  `STORAGE-ENGINE-ARCHITECTURE-DECISION.md`).
- Commits land on feature branches, one commit per milestone; the maintainer
  pushes and tags releases.
