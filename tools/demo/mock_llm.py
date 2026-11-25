#!/usr/bin/env python3
"""A tiny OpenAI-compatible Chat Completions server that plays the LLM judge.

Every `POST /v1/chat/completions` gets a made-up judgement that fits the schema the judge sent
(`response_format` or a forced tool call): a number in [0, 1], a boolean, or one of the labels.
`GET /v1/models` lists one model. Any `POST /hook/...` is printed, so the server doubles as the
webhook receiver for alert channels and webhook evaluators (a webhook evaluator gets a score
back). Standard library only; for local demos, never production.

    python3 tools/demo/mock_llm.py [--port 8090]

Use it as an LLM connection with adapter `openai`, base URL `http://127.0.0.1:8090/v1`, any API
key, and model `mock-judge`. cetra must run with
`CETRA_SECURITY__ALLOW_PRIVATE_NETWORK_EGRESS=true` to reach a loopback address.
"""

from __future__ import annotations

import argparse
import json
import random
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODEL = "mock-judge"
REASONS = [
    "The answer addresses the question directly.",
    "Partly relevant; it misses one detail the user asked for.",
    "The answer is off topic.",
]


def schema_of(body: dict) -> dict:
    fmt = body.get("response_format") or {}
    if fmt.get("type") == "json_schema":
        return fmt.get("json_schema", {}).get("schema", {})
    for tool in body.get("tools") or []:
        return tool.get("function", {}).get("parameters", {})
    return {}


def answer(schema: dict) -> dict:
    props = schema.get("properties") or {"explanation": {}, "value": {"type": "number"}}
    out: dict = {}
    if "explanation" in props:
        out["explanation"] = random.choice(REASONS)
    if "label" in props:
        out["label"] = random.choice(props["label"].get("enum") or ["unknown"])
    elif "value" in props:
        if props["value"].get("type") == "boolean":
            out["value"] = random.random() < 0.75
        else:
            out["value"] = round(random.betavariate(4, 2), 2)
    return out


class Handler(BaseHTTPRequestHandler):
    def _send(self, status: int, payload: dict) -> None:
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self) -> None:
        if self.path.rstrip("/").endswith("/models"):
            self._send(
                200,
                {"object": "list", "data": [{"id": MODEL, "object": "model", "owned_by": "demo"}]},
            )
        else:
            self._send(404, {"error": {"message": "not found"}})

    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        try:
            body = json.loads(self.rfile.read(length) or b"{}")
        except json.JSONDecodeError:
            self._send(400, {"error": {"message": "invalid JSON"}})
            return
        if self.path.startswith("/hook"):
            print(f"mock-llm: webhook {self.path}:\n{json.dumps(body, indent=2)}", flush=True)
            self._send(200, {"value": round(random.random(), 2), "reason": "mock webhook"})
            return
        if not self.path.rstrip("/").endswith("/chat/completions"):
            self._send(404, {"error": {"message": "not found"}})
            return
        result = answer(schema_of(body))
        text = json.dumps(result)
        if body.get("tools"):
            name = body["tools"][0]["function"]["name"]
            message = {
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {
                        "id": "call_demo",
                        "type": "function",
                        "function": {"name": name, "arguments": text},
                    }
                ],
            }
            finish = "tool_calls"
        else:
            message = {"role": "assistant", "content": text}
            finish = "stop"
        self._send(
            200,
            {
                "id": f"chatcmpl-demo-{time.time_ns()}",
                "object": "chat.completion",
                "created": int(time.time()),
                "model": body.get("model", MODEL),
                "choices": [{"index": 0, "message": message, "finish_reason": finish}],
                "usage": {"prompt_tokens": 120, "completion_tokens": 18, "total_tokens": 138},
            },
        )

    def log_message(self, fmt: str, *args: object) -> None:
        print(
            f"mock-llm: {self.command} {self.path} -> {args[1] if len(args) > 1 else ''}",
            flush=True,
        )


def main() -> None:
    parser = argparse.ArgumentParser(description="OpenAI-compatible mock judge")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8090)
    args = parser.parse_args()
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(
        f"mock LLM on http://{args.host}:{args.port}/v1 (model {MODEL}), webhooks on /hook/...",
        flush=True,
    )
    server.serve_forever()


if __name__ == "__main__":
    main()
