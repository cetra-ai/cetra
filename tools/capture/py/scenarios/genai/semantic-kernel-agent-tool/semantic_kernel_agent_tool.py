"""Semantic Kernel agent with one plugin function, traced by Semantic Kernel's own diagnostics."""

from __future__ import annotations

import asyncio
import os

from capture_py.harness import finish, mock_url, setup

MODEL = "gpt-4o-mini-2024-07-18"


def main() -> None:
    """Run the agent through one tool call and the answer."""
    # Semantic Kernel reads these switches when it is imported, so they are set first. The
    # message content goes to log records, which the capture does not export (D-IN-23 item 4).
    os.environ["SEMANTICKERNEL_EXPERIMENTAL_GENAI_ENABLE_OTEL_DIAGNOSTICS"] = "true"
    os.environ["SEMANTICKERNEL_EXPERIMENTAL_GENAI_ENABLE_OTEL_DIAGNOSTICS_SENSITIVE"] = "true"

    from openai import AsyncOpenAI
    from semantic_kernel.agents import ChatCompletionAgent
    from semantic_kernel.connectors.ai.open_ai import OpenAIChatCompletion
    from semantic_kernel.functions import kernel_function

    class WeatherPlugin:
        """Plugin that answers the weather question."""

        @kernel_function(name="get_weather", description="Get the weather")
        def get_weather(self, city: str) -> str:
            return f"sunny in {city}"

    provider = setup()
    client = AsyncOpenAI(base_url=mock_url("/v1"), api_key=os.environ["OPENAI_API_KEY"])
    service = OpenAIChatCompletion(ai_model_id=MODEL, async_client=client)

    async def run() -> None:
        agent = ChatCompletionAgent(
            service=service,
            name="WeatherAgent",
            instructions="You are a weather bot.",
            plugins=[WeatherPlugin()],
        )
        await agent.get_response(messages="What is the weather in Paris?")

    asyncio.run(run())
    finish(provider)
