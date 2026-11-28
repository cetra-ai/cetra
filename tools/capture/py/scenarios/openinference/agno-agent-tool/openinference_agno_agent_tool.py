"""Agno agent with one tool, traced by OpenInference."""

from __future__ import annotations

import os
import uuid

from agno.agent import Agent
from agno.models.openai import OpenAIChat
from openinference.instrumentation.agno import AgnoInstrumentor

from capture_py.harness import finish, mock_url, setup

MODEL = "gpt-4o-mini-2024-07-18"


def _pin_message_ids_and_time() -> None:
    """Replace agno's random message ids, times and durations with fixed values.

    All three land inside the llm spans' ``input.value`` JSON, where
    ``volatile_attributes`` cannot reach them.
    """
    import agno.models.message as message_model
    import agno.utils.timer as timer_module

    counter = 0

    def uuid4() -> uuid.UUID:
        nonlocal counter
        counter += 1
        return uuid.UUID(int=counter)

    def time() -> float:
        return 1_700_000_000.0

    ticks = 0

    def perf_counter() -> float:
        nonlocal ticks
        ticks += 1
        return ticks * 0.1

    message_model.uuid4 = uuid4  # type: ignore[attr-defined]
    message_model.time = time  # type: ignore[attr-defined]
    timer_module.perf_counter = perf_counter  # type: ignore[attr-defined]


def get_weather(city: str) -> str:
    """Look up the weather for a city."""
    return f"sunny in {city}"


def main() -> None:
    """Run the agent through one tool call and the answer."""
    _pin_message_ids_and_time()
    provider = setup()
    AgnoInstrumentor().instrument(tracer_provider=provider)
    agent = Agent(
        name="weather-agent",
        model=OpenAIChat(
            id=MODEL,
            api_key=os.environ["OPENAI_API_KEY"],
            base_url=mock_url("/v1"),
            max_retries=0,
        ),
        tools=[get_weather],
        instructions="You are a weather assistant.",
    )
    agent.run("What is the weather in Paris?")
    finish(provider)


if __name__ == "__main__":
    main()
