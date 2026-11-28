"""MCP client of the scenario: one get_weather call and one tools/list over stdio."""

from __future__ import annotations

import asyncio
import os
import sys
from typing import cast

from mcp import Client
from mcp.client.stdio import StdioServerParameters
from opentelemetry import trace
from opentelemetry.sdk.trace import TracerProvider

from capture_py.harness import finish, setup

SERVER = os.path.join(os.path.dirname(os.path.abspath(__file__)), "genai_mcp_tool_server.py")


def server_env() -> dict[str, str]:
    """Return the server's environment: PATH, HOME, and the OTel and capture variables."""
    return {
        key: value
        for key, value in os.environ.items()
        if key in ("PATH", "HOME") or key.startswith(("OTEL_", "CETRA_CAPTURE_"))
    }


async def converse() -> None:
    """Call get_weather once, then list the tools."""
    params = StdioServerParameters(command=sys.executable, args=[SERVER], env=server_env())
    async with Client(params) as client:
        await client.call_tool("get_weather", {"city": "Paris"})
        await client.list_tools()


def main() -> None:
    """Export the client's spans, run the conversation, and write the library versions."""
    setup()
    asyncio.run(converse())
    finish(cast("TracerProvider", trace.get_tracer_provider()))


if __name__ == "__main__":
    main()
