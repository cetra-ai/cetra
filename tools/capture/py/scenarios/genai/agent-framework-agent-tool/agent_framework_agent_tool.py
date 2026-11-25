"""Microsoft Agent Framework agent with one tool, traced by the framework's own telemetry."""

from __future__ import annotations

import asyncio
import os

from agent_framework import Agent, tool
from agent_framework.observability import enable_instrumentation
from agent_framework.openai import OpenAIChatCompletionClient

from capture_py.harness import finish, mock_url, setup

MODEL = "gpt-4o-mini-2024-07-18"


@tool
def get_weather(city: str) -> str:
    """Get the weather."""
    return f"sunny in {city}"


def main() -> None:
    """Run the agent through one tool call and the answer."""
    provider = setup()
    enable_instrumentation(enable_sensitive_data=True)
    client = OpenAIChatCompletionClient(
        model=MODEL, api_key=os.environ["OPENAI_API_KEY"], base_url=mock_url("/v1")
    )
    agent = Agent(
        client=client,
        name="WeatherAgent",
        instructions="You are a weather bot.",
        tools=[get_weather],
    )

    async def run() -> None:
        await agent.run("What is the weather in Paris?")

    asyncio.run(run())
    finish(provider)
