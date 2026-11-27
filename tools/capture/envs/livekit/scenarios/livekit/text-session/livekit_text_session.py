"""LiveKit Agents in text mode: one session, one function tool, the OpenAI plugin on the mock."""

from __future__ import annotations

import asyncio
import json
import os
from typing import Any

import openai
from livekit.agents import Agent, AgentSession, RunContext, function_tool
from livekit.agents.telemetry import set_tracer_provider
from livekit.plugins import openai as lk_openai
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import SimpleSpanProcessor

from capture_py.harness import finish, real_host_async_client

MODEL = "gpt-4o-mini-2024-07-18"
REAL_BASE_URL = "https://api.openai.com/v1"


class WeatherAgent(Agent):
    """An agent with one function tool."""

    def __init__(self) -> None:
        """Set the instructions."""
        super().__init__(instructions="You are a weather assistant.")

    @function_tool
    async def get_weather(self, context: RunContext[Any], city: str) -> str:
        """Look up the weather for a city."""
        return json.dumps({"temperature_c": 18, "conditions": "sunny", "city": city})


async def converse() -> None:
    """Run one user turn through the session, including the tool call and the answer."""
    client = openai.AsyncClient(
        base_url=REAL_BASE_URL,
        api_key=os.environ["OPENAI_API_KEY"],
        http_client=real_host_async_client(),
        max_retries=0,
    )
    session: AgentSession[Any] = AgentSession(llm=lk_openai.LLM(model=MODEL, client=client))
    await session.start(WeatherAgent())
    await session.run(user_input="What's the weather in Paris?")
    await session.aclose()


def main() -> None:
    """Export LiveKit's spans, with its content, and run one session."""
    provider = TracerProvider()
    provider.add_span_processor(SimpleSpanProcessor(OTLPSpanExporter()))
    # allow_pii keeps the lk.pii.* content (the default, stated here so the capture is explicit).
    set_tracer_provider(provider, allow_pii=True)
    asyncio.run(converse())
    finish(provider)


if __name__ == "__main__":
    main()
