"""Strands Agents agent with one tool, latest GenAI semantic conventions."""

from __future__ import annotations

import os

from strands import Agent, tool
from strands.models.openai import OpenAIModel
from strands.telemetry import StrandsTelemetry

from capture_py.harness import finish, mock_url, setup

MODEL = "gpt-4o-mini-2024-07-18"


@tool
def get_weather(city: str) -> str:
    """Get the weather."""
    return f"sunny in {city}"


def main() -> None:
    """Run the agent through one tool call and the answer."""
    provider = setup()
    StrandsTelemetry(tracer_provider=provider)
    model = OpenAIModel(
        client_args={
            "base_url": mock_url("/v1"),
            "api_key": os.environ["OPENAI_API_KEY"],
        },
        model_id=MODEL,
    )
    agent = Agent(
        model=model,
        tools=[get_weather],
        name="weather_agent",
        system_prompt="You are a weather bot.",
        callback_handler=None,
        trace_attributes={"session.id": "s1"},
    )
    agent("What is the weather in Paris?")
    finish(provider)
