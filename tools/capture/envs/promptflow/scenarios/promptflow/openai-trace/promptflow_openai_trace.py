"""Prompt Flow's @trace functions around two chat calls, exported by Prompt Flow's own setup."""

from __future__ import annotations

import json
import os
from typing import Any, cast

from openai import OpenAI
from openai.types.chat import ChatCompletionFunctionToolParam, ChatCompletionMessageFunctionToolCall
from opentelemetry import trace
from opentelemetry.sdk.trace import TracerProvider
from promptflow.tracing import trace as pf_trace
from promptflow.tracing._start_trace import setup_exporter_from_environ

from capture_py.harness import finish, mock_url

MODEL = "gpt-4o-mini-2024-07-18"
TOOLS: list[ChatCompletionFunctionToolParam] = [
    {
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Look up the weather for a city.",
            "parameters": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"],
            },
        },
    }
]

# Set by main(): the decorated functions read the client rather than take it as an input.
CLIENT: OpenAI | None = None


def client() -> OpenAI:
    """Return the client that main() created."""
    if CLIENT is None:
        raise RuntimeError("main() sets the client before flow() runs")
    return CLIENT


@pf_trace
def get_weather(city: str) -> str:
    """Look up the weather for a city (a tool in the flow)."""
    return json.dumps({"temperature_c": 18, "conditions": "sunny", "city": city})


@pf_trace
def flow(question: str) -> str:
    """Answer one question: a tool call, the tool's result, then the answer."""
    messages: list[Any] = [
        {"role": "system", "content": "You are a weather assistant."},
        {"role": "user", "content": question},
    ]
    first = client().chat.completions.create(model=MODEL, messages=messages, tools=TOOLS)
    message = first.choices[0].message
    if not message.tool_calls:
        raise RuntimeError("the mock answers a tool call first")
    call = message.tool_calls[0]
    if not isinstance(call, ChatCompletionMessageFunctionToolCall):
        raise TypeError("the mock answers a function tool call")
    arguments = json.loads(call.function.arguments)
    result = get_weather(arguments["city"])
    messages.append(message.model_dump(exclude_none=True))
    messages.append({"role": "tool", "tool_call_id": call.id, "content": result})
    second = client().chat.completions.create(model=MODEL, messages=messages, tools=TOOLS)
    return second.choices[0].message.content or ""


def main() -> None:
    """Run the flow through one tool call and the answer."""
    global CLIENT
    # Prompt Flow's @trace is a no-op unless PF_DISABLE_TRACING is "false"
    # (promptflow/tracing/_utils.py).
    os.environ["PF_DISABLE_TRACING"] = "false"
    # Prompt Flow posts to OTEL_EXPORTER_OTLP_ENDPOINT exactly as given: the full traces URL.
    os.environ["OTEL_EXPORTER_OTLP_ENDPOINT"] = os.environ["CETRA_CAPTURE_OTLP_ENDPOINT"]
    setup_exporter_from_environ()
    CLIENT = OpenAI(
        base_url=mock_url("/v1"),
        api_key=os.environ["OPENAI_API_KEY"],
        max_retries=0,
    )
    flow("What's the weather in Paris?")
    finish(cast("TracerProvider", trace.get_tracer_provider()))


if __name__ == "__main__":
    main()
