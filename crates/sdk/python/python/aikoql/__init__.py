"""Aikoql Python SDK — Agent-first Knowledge Database (MRFC-0040).

Unified interface for AI agents. Supports embedded (PyO3) and server (MCP) modes.

    from aikoql import Agent

    # Embedded mode (in-process); a fresh path creates an aikoql-v2 database:
    db = Agent.connect("./kb")

    # Server mode (talks to aikoql-mcp over TCP):
    db = Agent.connect("localhost:9090")

    result = db.remember(type_name="Task", properties={"title": "Fix auth bug"})
    tasks = db.aikoql("MATCH Task WHERE status == 'open' RETURN *")
"""

from aikoql.agent import Agent
from aikoql.mcp_client import McpClient, McpError
from aikoql.adapters.crewai import AikoqlCrewAIMemory
from aikoql.adapters.langgraph import AikoqlLangGraphSaver

# PyO3 native module — may be unavailable in pure-MCP deployments.
try:
    from aikoql._aikoql import aikoql
except ImportError:
    aikoql = None  # type: ignore

__all__ = [
    "Agent", "McpClient", "McpError",
    "aikoql", "AikoqlCrewAIMemory", "AikoqlLangGraphSaver",
]
