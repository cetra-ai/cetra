"""AG2 agent with one tool, traced by AG2's telemetry middleware."""

from __future__ import annotations

import asyncio
import os

from ag2 import Agent, tool
from ag2.config.openai import OpenAIConfig
from ag2.middleware.builtin.telemetry import TelemetryMiddleware

from capture_py.harness import finish, mock_url, setup

MODEL = "gpt-4o-mini-2024-07-18"


@tool
def get_weather(city: str) -> str:
    """Get the weather."""
    return f"sunny in {city}"


def main() -> None:
    """Run the agent through one tool call and the answer."""
    provider = setup()
    config = OpenAIConfig(
        model=MODEL, api_key=os.environ["OPENAI_API_KEY"], base_url=mock_url("/v1")
    )
    middleware = [
        TelemetryMiddleware(
            tracer_provider=provider,
            agent_name="weather_agent",
            provider_name="openai",
            model_name=MODEL,
        )
    ]
    agent = Agent(
        "weather_agent",
        prompt="You are a weather bot.",
        config=config,
        tools=[get_weather],
        middleware=middleware,
    )

    async def run() -> None:
        reply = await agent.ask("What is the weather in Paris?")
        await reply.content()

    asyncio.run(run())
    finish(provider)
