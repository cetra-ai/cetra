"""smolagents CodeAgent with one tool, traced by OpenInference."""

from __future__ import annotations

import os
from types import SimpleNamespace

from openinference.instrumentation.smolagents import SmolagentsInstrumentor
from smolagents import CodeAgent, OpenAIServerModel, tool

from capture_py.harness import finish, mock_url, setup

MODEL = "gpt-4o-mini-2024-07-18"


def _pin_step_times() -> None:
    """Replace smolagents' wall clock with a stepping counter.

    Step start times land inside the Step spans' ``input.value`` repr, where
    neither ``volatile_attributes`` nor ``timing_attributes`` can reach them.
    """
    import smolagents.agents as agents_module

    ticks = 0

    def time() -> float:
        nonlocal ticks
        ticks += 1
        return 1_700_000_000.0 + ticks * 0.25

    agents_module.time = SimpleNamespace(time=time)


@tool  # type: ignore[untyped-decorator]
def get_weather(city: str) -> str:
    """Look up the weather for a city.

    Args:
        city: the city name
    """
    return f"sunny in {city}"


def main() -> None:
    """Run the agent through one code block and the final answer."""
    _pin_step_times()
    provider = setup()
    SmolagentsInstrumentor().instrument(tracer_provider=provider)
    model = OpenAIServerModel(
        model_id=MODEL,
        api_key=os.environ["OPENAI_API_KEY"],
        api_base=mock_url("/v1"),
        client_kwargs={"max_retries": 0},
    )
    agent = CodeAgent(tools=[get_weather], model=model, name="weather_agent", max_steps=3)
    agent.run("What is the weather in Paris?")
    finish(provider)


if __name__ == "__main__":
    main()
