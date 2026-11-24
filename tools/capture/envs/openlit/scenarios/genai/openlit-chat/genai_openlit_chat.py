"""OpenLIT's OpenAI instrumentation: one chat with a tool call, the answer, one embedding."""

from __future__ import annotations

import json
import os
from typing import Any, cast

from openai import OpenAI
from openai.types.chat import ChatCompletionFunctionToolParam, ChatCompletionMessageFunctionToolCall
from openlit import init
from opentelemetry import trace
from opentelemetry.sdk.trace import TracerProvider

from capture_py.harness import finish, mock_url, receiver_base_url

MODEL = "gpt-4o-mini-2024-07-18"
EMBEDDING_MODEL = "text-embedding-3-small"
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


def main() -> None:
    """Start OpenLIT against the receiver with an empty price table, then run the calls."""
    init(
        otlp_endpoint=receiver_base_url(),
        disable_metrics=True,
        capture_message_content=True,
        pricing_json=os.path.join(os.path.dirname(os.path.abspath(__file__)), "pricing.json"),
        disabled_instrumentors=["httpx", "requests", "urllib3", "aiohttp"],
    )
    # openlit 1.45.0 needs openai<3, whose client is built on httpx: the mock's URL
    # is the base URL, and no real-host client is needed.
    client = OpenAI(
        base_url=mock_url("/v1"),
        api_key=os.environ["OPENAI_API_KEY"],
        max_retries=0,
    )
    messages: list[Any] = [
        {"role": "system", "content": "You are a weather assistant."},
        {"role": "user", "content": "What's the weather in Paris?"},
    ]
    first = client.chat.completions.create(model=MODEL, messages=messages, tools=TOOLS)
    message = first.choices[0].message
    if not message.tool_calls:
        raise RuntimeError("the mock answers a tool call first")
    call = message.tool_calls[0]
    if not isinstance(call, ChatCompletionMessageFunctionToolCall):
        raise TypeError("the mock answers a function tool call")
    arguments = json.loads(call.function.arguments)
    result = json.dumps({"temperature_c": 18, "conditions": "sunny", "city": arguments["city"]})
    messages.append(message.model_dump(exclude_none=True))
    messages.append({"role": "tool", "tool_call_id": call.id, "content": result})
    client.chat.completions.create(model=MODEL, messages=messages, tools=TOOLS)
    client.embeddings.create(model=EMBEDDING_MODEL, input=["sunny"], dimensions=8)
    finish(cast("TracerProvider", trace.get_tracer_provider()))


if __name__ == "__main__":
    main()
