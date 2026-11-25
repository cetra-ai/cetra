"""Langflow Basic Prompting run through the API, traced by Langflow's Traceloop export."""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import time
import urllib.request
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

from capture_py.harness import (
    finish_tool,
    free_port,
    mock_url,
    receiver_base_url,
    stop_gateway,
    tool_binary,
    wait_http,
)

LANGFLOW_VERSION = "1.13.2"
MOCK_KEY = "capture-mock-key"
MODEL = "gpt-4o-mini"
START_TIMEOUT = 120.0
REQUEST_TIMEOUT = 120.0
EXPORT_DELAY = 8.0
STOP_WAIT = 30.0
QUESTION = "What is the weather in Paris?"
# A fixed flow id: Langflow puts the flow id in the root span name ("<flow name> - <id>").
FLOW_ID = "00000000-0000-4000-8000-00000000f10a"


def main() -> None:
    """Start Langflow, run the starter flow once through its API, then stop it so it flushes."""
    langflow = tool_binary("langflow", LANGFLOW_VERSION, "bin", "langflow")
    starter = tool_binary(
        "langflow",
        LANGFLOW_VERSION,
        "lib",
        "python3.13",
        "site-packages",
        "langflow",
        "initial_setup",
        "starter_projects",
        "Basic Prompting.json",
    )
    port = free_port()
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        proc = subprocess.Popen(  # noqa: S603 - a pinned tool
            [
                langflow,
                "run",
                "--host",
                "127.0.0.1",
                "--port",
                str(port),
                "--backend-only",
                "--no-open-browser",
            ],
            env=gateway_env(work),
            cwd=tmp,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        base = f"http://127.0.0.1:{port}"
        try:
            wait_http(f"{base}/health", START_TIMEOUT, proc=proc)
            run_basic_prompting(base, Path(starter))
            time.sleep(EXPORT_DELAY)
        finally:
            stop_gateway(proc, STOP_WAIT)
    finish_tool(None, {"langflow": LANGFLOW_VERSION})


def gateway_env(work: Path) -> dict[str, str]:
    """Langflow's whole environment: temporary database, switches off, mock and Traceloop."""
    config = work / "config"
    mock_origin = f"http://{urlsplit(mock_url()).hostname}"
    return {
        "PATH": os.environ["PATH"],
        "HOME": os.environ["HOME"],
        "LANGFLOW_CONFIG_DIR": str(config),
        "LANGFLOW_DATABASE_URL": f"sqlite:///{config / 'langflow.db'}",
        "LANGFLOW_AUTO_LOGIN": "true",
        "LANGFLOW_OPEN_BROWSER": "false",
        "LANGFLOW_DO_NOT_TRACK": "true",
        "DO_NOT_TRACK": "true",
        "LANGFLOW_NATIVE_TRACING": "false",
        "LANGFLOW_MODELS_DEV_REFRESH": "false",
        "LANGFLOW_PROVIDER_CREDENTIAL_ALLOWED_HOSTS": mock_origin,
        "OPENAI_BASE_URL": mock_url("/v1"),
        "TRACELOOP_BASE_URL": receiver_base_url(),
        "TRACELOOP_API_KEY": MOCK_KEY,
        "TRACELOOP_TELEMETRY": "false",
    }


def call(
    base: str,
    method: str,
    path: str,
    body: dict[str, Any] | None = None,
    token: str | None = None,
    api_key: str | None = None,
) -> dict[str, Any]:
    """One JSON request to Langflow's API; the response body is a JSON object."""
    headers = {"content-type": "application/json"}
    if token is not None:
        headers["authorization"] = f"Bearer {token}"
    if api_key is not None:
        headers["x-api-key"] = api_key
    data = None if body is None else json.dumps(body).encode("utf-8")
    request = urllib.request.Request(  # noqa: S310 - a loopback URL the scenario built
        base + path, data=data, headers=headers, method=method
    )
    with urllib.request.urlopen(request, timeout=REQUEST_TIMEOUT) as response:  # noqa: S310
        decoded: dict[str, Any] = json.loads(response.read() or b"{}")
    return decoded


def run_basic_prompting(base: str, starter: Path) -> None:
    """The API sequence of notes: auto-login, create the flow, create a key, run the flow."""
    token: str = call(base, "GET", "/api/v1/auto_login")["access_token"]
    flow: dict[str, Any] = json.loads(starter.read_text(encoding="utf-8"))
    set_model(flow["data"])
    created = call(
        base,
        "POST",
        "/api/v1/flows/",
        {"id": FLOW_ID, "name": "cetra capture", "description": "", "data": flow["data"]},
        token=token,
    )
    key: str = call(base, "POST", "/api/v1/api_key/", {"name": "cetra capture"}, token=token)[
        "api_key"
    ]
    call(
        base,
        "POST",
        f"/api/v1/run/{created['id']}?stream=false",
        {
            "input_value": QUESTION,
            "input_type": "chat",
            "output_type": "chat",
            "session_id": "sess-1",
        },
        token=token,
        api_key=key,
    )


def set_model(data: dict[str, Any]) -> None:
    """Point the starter's Language Model node at gpt-4o-mini with the mock key."""
    for node in data["nodes"]:
        component = node["data"]["node"]
        if component.get("display_name") != "Language Model":
            continue
        template = component["template"]
        template["model"]["value"] = [
            {
                "name": MODEL,
                "provider": "OpenAI",
                "icon": "OpenAI",
                "category": "OpenAI",
                "metadata": {
                    "model_class": "ChatOpenAI",
                    "model_name_param": "model",
                    "api_key_param": "api_key",
                },
            }
        ]
        template["api_key"]["value"] = MOCK_KEY
        template["api_key"]["load_from_db"] = False


if __name__ == "__main__":
    main()
