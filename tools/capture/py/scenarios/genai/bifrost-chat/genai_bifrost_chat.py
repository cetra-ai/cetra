"""Bifrost gateway: a chat tool call and its answer, traced by the gateway's otel plugin."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

from openai import OpenAI
from openai.types.chat import (
    ChatCompletionMessage,
    ChatCompletionMessageFunctionToolCall,
    ChatCompletionMessageParam,
    ChatCompletionToolParam,
)

from capture_py.harness import (
    finish_tool,
    free_port,
    importlib_versions,
    mock_url,
    receiver_base_url,
    stop_gateway,
    tool_binary,
    wait_http,
)

HERE = Path(__file__).resolve().parent
BIFROST_VERSION = "2.5.1"
START_TIMEOUT = 30.0
STOP_WAIT = 20.0


def main() -> None:
    """Run Bifrost, send a tool call and its answer, then stop it so its exporter flushes."""
    binary = tool_binary("bifrost", BIFROST_VERSION, "bifrost-http")
    port = free_port()
    with tempfile.TemporaryDirectory() as tmp:
        app = Path(tmp) / "app"
        app.mkdir()
        for name in ("pricing.json", "model-parameters.json"):
            shutil.copyfile(HERE / name, app / name)
        config = (HERE / "config.json").read_text(encoding="utf-8")
        config = (
            config.replace("@APP_DIR@", str(app))
            .replace("@MOCK_URL@", mock_url())
            .replace("@OTLP_ENDPOINT@", receiver_base_url())
        )
        (app / "config.json").write_text(config, encoding="utf-8")
        proc = subprocess.Popen(  # noqa: S603 - a pinned binary
            [
                binary,
                "-app-dir",
                str(app),
                "-port",
                str(port),
                "-host",
                "127.0.0.1",
                "-log-style",
                "pretty",
            ],
            env=gateway_env(),
        )
        try:
            wait_http(f"http://127.0.0.1:{port}/health", START_TIMEOUT, proc=proc)
            client = OpenAI(
                base_url=f"http://127.0.0.1:{port}/v1",
                api_key=os.environ["OPENAI_API_KEY"],
                max_retries=0,
            )
            messages: list[ChatCompletionMessageParam] = [
                {"role": "system", "content": "You are a weather bot."},
                {"role": "user", "content": "What is the weather in Paris?"},
            ]
            tools = [weather_tool()]
            first = client.chat.completions.create(
                model="openai/gpt-4o-mini",
                messages=messages,
                tools=tools,
                user="user-1",
            )
            call = function_call(first.choices[0].message)
            messages.append(
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.function.name,
                                "arguments": call.function.arguments,
                            },
                        }
                    ],
                }
            )
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": call.id,
                    "content": json.dumps({"temperature_c": 18, "conditions": "sunny"}),
                }
            )
            client.chat.completions.create(
                model="openai/gpt-4o-mini",
                messages=messages,
                tools=tools,
                user="user-1",
            )
        finally:
            stop_gateway(proc, STOP_WAIT)
    finish_tool(None, {"bifrost": BIFROST_VERSION, **importlib_versions(["openai"])})


def gateway_env() -> dict[str, str]:
    """The gateway's whole environment; the runner clears the parent's."""
    return {
        "PATH": os.environ["PATH"],
        "HOME": os.environ["HOME"],
        "CETRA_OTLP_AUTH": "Bearer capture-mock-key",
        "UPSTREAM_KEY": "sk-capture",
    }


def weather_tool() -> ChatCompletionToolParam:
    return {
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get weather",
            "parameters": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
            },
        },
    }


def function_call(message: ChatCompletionMessage) -> ChatCompletionMessageFunctionToolCall:
    for call in message.tool_calls or []:
        if isinstance(call, ChatCompletionMessageFunctionToolCall):
            return call
    raise RuntimeError("tool call missing")


if __name__ == "__main__":
    main()
