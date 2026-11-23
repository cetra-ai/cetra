"""BeeAI RequirementAgent with one tool, traced by OpenInference."""

from __future__ import annotations

import asyncio
import os

from beeai_framework.adapters.openai import OpenAIChatModel
from beeai_framework.agents.requirement import RequirementAgent
from beeai_framework.tools import tool
from openinference.instrumentation.beeai import BeeAIInstrumentor

from capture_py.harness import finish, mock_url, setup

MODEL = "gpt-4o-mini-2024-07-18"


def _pin_tool_call_ids() -> None:
    """Replace BeeAI's random tool-call ids with a counter.

    The ids are 13 characters, under the 16 that ``volatile_attributes``
    needs to replace them where they repeat inside message and state JSON.
    """
    import beeai_framework.agents.requirement._runner as requirement_runner
    import beeai_framework.backend.chat as backend_chat

    counter = 0

    def generate_random_string(length: int) -> str:
        nonlocal counter
        counter += 1
        return f"{counter:0{length}d}"

    # generate_random_string is imported into those modules but not re-exported.
    backend_chat.generate_random_string = (  # type: ignore[attr-defined]
        generate_random_string
    )
    requirement_runner.generate_random_string = (  # type: ignore[attr-defined]
        generate_random_string
    )


@tool
def get_weather(city: str) -> str:
    """Look up the weather for a city."""
    return f"sunny in {city}"


def main() -> None:
    """Run the agent through one tool call and the final answer."""
    _pin_tool_call_ids()
    provider = setup()
    BeeAIInstrumentor().instrument(tracer_provider=provider)

    async def run_agent() -> None:
        llm = OpenAIChatModel(
            MODEL,
            api_key=os.environ["OPENAI_API_KEY"],
            base_url=mock_url("/v1"),
        )
        agent = RequirementAgent(
            llm=llm,
            tools=[get_weather],
            instructions="You are a weather assistant.",
            name="weather-agent",
        )
        await agent.run("What is the weather in Paris?")

    asyncio.run(run_agent())
    finish(provider)


if __name__ == "__main__":
    main()
