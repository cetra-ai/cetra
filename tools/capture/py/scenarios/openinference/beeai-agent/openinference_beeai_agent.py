"""BeeAI RequirementAgent with one tool, traced by OpenInference."""

from __future__ import annotations

import asyncio
import os
from datetime import datetime, timezone

from beeai_framework.adapters.openai import OpenAIChatModel
from beeai_framework.agents.requirement import RequirementAgent
from beeai_framework.tools import tool
from openinference.instrumentation.beeai import BeeAIInstrumentor

from capture_py.harness import finish, mock_url, setup

MODEL = "gpt-4o-mini-2024-07-18"


def _pin_tool_call_ids_and_prompt_date() -> None:
    """Replace BeeAI's random tool-call ids with a counter, and freeze the
    date it stamps into the system prompt.

    The ids are 13 characters, under the 16 that ``volatile_attributes``
    needs to replace them where they repeat inside message and state JSON.
    The system prompt ends with the current date, rendered by
    ``RequirementAgentSystemPrompt``'s ``formatDate`` from the ``datetime``
    of its module, and would otherwise drift with the capture day.
    """
    import beeai_framework.agents.requirement._runner as requirement_runner
    import beeai_framework.agents.requirement.prompts as requirement_prompts
    import beeai_framework.backend.chat as backend_chat

    counter = 0

    def generate_random_string(length: int) -> str:
        nonlocal counter
        counter += 1
        return f"{counter:0{length}d}"

    class FixedClock:
        """Stands in for datetime in the system prompt's formatDate."""

        @staticmethod
        def now(tz: timezone) -> datetime:
            return datetime(2026, 11, 23, tzinfo=tz)

    # generate_random_string is imported into those modules but not re-exported.
    backend_chat.generate_random_string = (  # type: ignore[attr-defined]
        generate_random_string
    )
    requirement_runner.generate_random_string = (  # type: ignore[attr-defined]
        generate_random_string
    )
    # The formatDate lambda reads datetime from its module's globals at call time.
    requirement_prompts.datetime = FixedClock  # type: ignore[attr-defined,assignment]


@tool
def get_weather(city: str) -> str:
    """Look up the weather for a city."""
    return f"sunny in {city}"


def main() -> None:
    """Run the agent through one tool call and the final answer."""
    _pin_tool_call_ids_and_prompt_date()
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
