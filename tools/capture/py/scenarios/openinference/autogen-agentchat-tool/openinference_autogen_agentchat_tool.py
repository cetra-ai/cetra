"""AutoGen AgentChat agent with one tool, traced by the AutoGen OpenInference instrumentor only."""

from __future__ import annotations

import asyncio
import os
import uuid
from datetime import datetime, timezone
from types import SimpleNamespace

from capture_py.harness import finish, mock_url, setup

MODEL = "gpt-4o-mini-2024-07-18"


async def get_weather(city: str) -> str:
    """Get the weather."""
    return f"sunny in {city}"


def _pin_ids_clock_and_token_repr() -> None:
    """Replace AutoGen's random message ids and clock, and the token's address, with fixed values.

    The agent span's input.value holds all three, and volatile_attributes cannot reach inside it.
    """
    import autogen_agentchat.messages as messages
    from autogen_core import CancellationToken

    counter = 0

    def uuid4() -> uuid.UUID:
        nonlocal counter
        counter += 1
        return uuid.UUID(int=counter)

    class FixedClock:
        """Stands in for datetime in the message default factory."""

        @staticmethod
        def now(tz: timezone) -> datetime:
            return datetime(2026, 1, 1, tzinfo=tz)

    messages.uuid = SimpleNamespace(uuid4=uuid4)  # type: ignore[attr-defined,assignment]
    messages.datetime = FixedClock  # type: ignore[attr-defined,assignment]
    CancellationToken.__repr__ = lambda self: "<CancellationToken>"  # type: ignore[method-assign]


def main() -> None:
    """Run the agent through one tool call and the answer."""
    # The autogen runtime reads this switch at import. Only the AutoGen instrumentor is enabled:
    # the OpenAI instrumentor would add a second llm span for each call (D-IN-23 item 3).
    os.environ["AUTOGEN_DISABLE_RUNTIME_TRACING"] = "true"

    from autogen_agentchat.agents import AssistantAgent
    from autogen_ext.models.openai import OpenAIChatCompletionClient
    from openinference.instrumentation.autogen_agentchat import AutogenAgentChatInstrumentor

    _pin_ids_clock_and_token_repr()
    provider = setup()
    AutogenAgentChatInstrumentor().instrument(tracer_provider=provider)
    client = OpenAIChatCompletionClient(
        model=MODEL,
        base_url=mock_url("/v1"),
        api_key=os.environ["OPENAI_API_KEY"],
    )
    agent = AssistantAgent(
        "weather_agent",
        model_client=client,
        tools=[get_weather],
        system_message="You are a weather bot.",
        reflect_on_tool_use=True,
    )

    async def run() -> None:
        await agent.run(task="What is the weather in Paris?")

    asyncio.run(run())
    finish(provider)
