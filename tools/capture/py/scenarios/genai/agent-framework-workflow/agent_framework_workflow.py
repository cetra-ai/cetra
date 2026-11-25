"""Microsoft Agent Framework workflow with two executors and no model call."""

from __future__ import annotations

import asyncio
from typing import Never

from agent_framework import WorkflowBuilder, WorkflowContext, executor
from agent_framework.observability import enable_instrumentation

from capture_py.harness import finish, setup


@executor(id="upper")
async def upper(text: str, ctx: WorkflowContext[str]) -> None:
    await ctx.send_message(text.upper())


@executor(id="sink")
async def sink(text: str, ctx: WorkflowContext[Never, str]) -> None:
    await ctx.yield_output(text)


def main() -> None:
    """Run the two-executor workflow once."""
    provider = setup()
    enable_instrumentation(enable_sensitive_data=True)
    workflow = WorkflowBuilder(start_executor=upper).add_edge(upper, sink).build()

    async def run() -> None:
        await workflow.run("hello")

    asyncio.run(run())
    finish(provider)
