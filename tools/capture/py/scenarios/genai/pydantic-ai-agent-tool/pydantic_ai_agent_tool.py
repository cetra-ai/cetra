"""Pydantic AI agent with one tool, traced by Pydantic AI's own OpenTelemetry support."""

from __future__ import annotations

import os

from pydantic_ai import Agent
from pydantic_ai.agent import InstrumentationSettings
from pydantic_ai.models.openai import OpenAIChatModel
from pydantic_ai.providers.openai import OpenAIProvider

from capture_py.harness import finish, mock_url, setup

MODEL = "gpt-4o-mini-2024-07-18"


def main() -> None:
    """Run the agent through one tool call and the answer."""
    provider = setup()
    Agent.instrument_all(InstrumentationSettings(tracer_provider=provider, include_content=True))
    model = OpenAIChatModel(
        MODEL,
        provider=OpenAIProvider(base_url=mock_url("/v1"), api_key=os.environ["OPENAI_API_KEY"]),
    )
    agent = Agent(model, name="weather_agent", system_prompt="You are a weather bot.")

    @agent.tool_plain
    def get_weather(city: str) -> str:
        """Get the weather."""
        return f"sunny in {city}"

    agent.run_sync("What is the weather in Paris?")
    finish(provider)
