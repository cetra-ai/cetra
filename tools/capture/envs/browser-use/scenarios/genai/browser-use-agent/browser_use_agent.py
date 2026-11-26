"""Browser Use 1.0 on headless Chromium, traced by Pydantic AI's GenAI spans."""

from __future__ import annotations

import asyncio
import os
from typing import cast

from opentelemetry import trace
from opentelemetry.sdk.trace import TracerProvider

from capture_py.harness import finish, mock_url, setup, tool_binary

# Browser Use's telemetry and cloud sync reach other hosts unless these are off.
os.environ["ANONYMIZED_TELEMETRY"] = "false"
os.environ["BROWSER_USE_CLOUD_SYNC"] = "false"

from browser_use import Agent, Browser
from browser_use.llm import ChatOpenAI
from pydantic_ai import Agent as PydanticAgent

MODEL = "gpt-4o-mini-2024-07-18"
TASK = "Reply with done."


async def run_agent() -> None:
    """Run one agent step through the mock and stop at done."""
    browser = Browser(
        kind="chromium",
        headless=True,
        executable_path=tool_binary("playwright", "chromium-1255", "chrome-linux64", "chrome"),
    )
    agent = Agent(
        task=TASK,
        llm=ChatOpenAI(model=MODEL, api_key=os.environ["OPENAI_API_KEY"], base_url=mock_url("/v1")),
        browser=browser,
        judge=False,
    )
    await agent.run(max_steps=1)


def main() -> None:
    """Instrument Pydantic AI, run the agent, and flush the provider."""
    setup()
    PydanticAgent.instrument_all(True)
    asyncio.run(run_agent())
    finish(cast("TracerProvider", trace.get_tracer_provider()))


if __name__ == "__main__":
    main()
