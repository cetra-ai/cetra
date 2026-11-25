"""Mirascope call with one tool, traced by Mirascope's ops tracing."""

from __future__ import annotations

import os

from mirascope import llm, ops

from capture_py.harness import finish, mock_url, setup

MODEL = "openai:completions/gpt-4o-mini-2024-07-18"


@llm.tool
def get_weather(city: str) -> str:
    """Get the weather."""
    return f"sunny in {city}"


@ops.trace
@llm.call(MODEL, tools=[get_weather])
def ask(question: str) -> str:
    return question


@ops.trace
def run(question: str) -> str:
    response = ask(question)
    while response.tool_calls:
        response = response.resume(response.execute_tools())
    return response.text()


def main() -> None:
    """Run the agent through one tool call and the answer, inside one session."""
    provider = setup()
    llm.register_provider(
        "openai:completions",
        scope="openai:completions/",
        api_key=os.environ["OPENAI_API_KEY"],
        base_url=mock_url("/v1"),
    )
    ops.configure(tracer_provider=provider)
    ops.instrument_llm()
    with ops.session(id="sess-1"):
        run("What is the weather in Paris?")
    finish(provider)
