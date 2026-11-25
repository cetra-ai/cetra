"""The Temporal workflow and activity the capture runs.

The workflow must live in a named module: defined in ``__main__`` the worker fails
with ``Failed validating workflow`` (the sandbox re-imports the module by name).
"""

from __future__ import annotations

import json
from datetime import timedelta

from agents import Agent, Runner
from temporalio import activity, workflow
from temporalio.openai_agents.workflow import activity_as_tool

MODEL = "gpt-4o-mini-2024-07-18"


@activity.defn
async def get_weather(city: str) -> str:
    """Look up the weather for a city."""
    return json.dumps({"temperature_c": 18, "conditions": "sunny", "city": city})


@workflow.defn
class WeatherWorkflow:
    """One agent run through a tool call and the answer."""

    @workflow.run
    async def run(self, question: str) -> str:
        agent = Agent(
            name="weather",
            instructions="You are a weather assistant.",
            model=MODEL,
            tools=[activity_as_tool(get_weather, start_to_close_timeout=timedelta(seconds=10))],
        )
        result = await Runner.run(agent, question)
        return str(result.final_output)
