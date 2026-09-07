---
title: Python SDK
description: PyO3 native bindings for aikoql
---

# Python SDK

Native Python bindings via PyO3. Direct access to the Knowledge Kernel.

## Installation

```bash
pip install aikoql
```

## Usage

```python
import aikoql_py

# Open a database — the default backend is aikoql-v2: a fresh path creates
# a database directory. Existing .redb files still open as redb, and a v1
# WAL still opens as v1 (auto-detection, never reinterpretation).
kernel = aikoql_py.Kernel.open("./kb")

# Create an object
result = kernel.remember({
    "type_name": "Employee",
    "properties": {"name": "Alice", "role": "Architect"},
    "tenant": "acme"
})
print(f"Created: {result.koid}")

# Query
results = kernel.find_similar({
    "type_name": "Employee",
    "text": "engineer"
})
for r in results:
    print(f"{r.koid}: {r.score}")

# aikoql
result = kernel.aikoql("MATCH Employee RETURN name, role")
```

The unified `Agent` interface auto-detects its target the same way — any
filesystem path is embedded mode (fresh paths included), `"host:port"` is
server mode:

```python
from aikoql import Agent

db = Agent.connect("./kb")          # embedded, aikoql-v2 by default
db = Agent.connect("localhost:9090")  # MCP server over TCP
```

## LangGraph + CrewAI

Built-in adapters for AI agent frameworks:

```python
from aikoql_py.adapters import LangGraphCheckpointer

checkpointer = LangGraphCheckpointer(kernel)
# Use as LangGraph's checkpointer for agent state persistence
```
