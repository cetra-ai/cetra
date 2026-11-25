"""n8n webhook run: a workflow with an AI Agent and a Chat Model, traced by n8n's OTel export."""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from http import HTTPStatus
from pathlib import Path

from capture_py.harness import (
    finish_tool,
    free_port,
    mock_url,
    receiver_base_url,
    stop_gateway,
    tool_binary,
    wait_http,
)

HERE = Path(__file__).resolve().parent
N8N_VERSION = "2.48.4"
WORKFLOW_ID = "wf1"
START_TIMEOUT = 120.0
COMMAND_TIMEOUT = 120.0
WEBHOOK_TIMEOUT = 60.0
EXPORT_DELAY = 8.0
STOP_WAIT = 30.0


def main() -> None:
    """Import the credential and the workflow, start the server, post one webhook, then stop it."""
    n8n = tool_binary("n8n", N8N_VERSION, "node_modules", ".bin", "n8n")
    port = free_port()
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        env = gateway_env(work, port)
        credentials = work / "credentials.json"
        credentials.write_text(
            (HERE / "credentials.json")
            .read_text(encoding="utf-8")
            .replace("@MOCK_URL@", mock_url("/v1")),
            encoding="utf-8",
        )
        n8n_cli(n8n, env, work, "import:credentials", f"--input={credentials}")
        n8n_cli(n8n, env, work, "import:workflow", f"--input={HERE / 'workflow.json'}")
        n8n_cli(n8n, env, work, "update:workflow", f"--id={WORKFLOW_ID}", "--active=true")
        proc = subprocess.Popen(  # noqa: S603 - a pinned tool
            [n8n, "start"],
            env=env,
            cwd=tmp,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            wait_http(f"http://127.0.0.1:{port}/healthz", START_TIMEOUT, proc=proc)
            post_webhook(port)
            time.sleep(EXPORT_DELAY)
        finally:
            stop_gateway(proc, STOP_WAIT)
    finish_tool(None, {"n8n": N8N_VERSION})


def gateway_env(work: Path, port: int) -> dict[str, str]:
    """The n8n process's whole environment: user folder, switches off, OTel export."""
    return {
        "PATH": os.environ["PATH"],
        "HOME": os.environ["HOME"],
        "N8N_USER_FOLDER": str(work / "user"),
        "N8N_DIAGNOSTICS_ENABLED": "false",
        "N8N_VERSION_NOTIFICATIONS_ENABLED": "false",
        "N8N_TEMPLATES_ENABLED": "false",
        "N8N_AI_ENABLED": "false",
        "N8N_HOST": "127.0.0.1",
        "N8N_LISTEN_ADDRESS": "127.0.0.1",
        "N8N_PORT": str(port),
        "N8N_OTEL_ENABLED": "true",
        "N8N_OTEL_EXPORTER_OTLP_ENDPOINT": receiver_base_url(),
        "N8N_OTEL_EXPORTER_OTLP_PROTOCOL": "http/protobuf",
        "N8N_OTEL_EXPORTER_OTLP_HEADERS": "authorization=Bearer capture-mock-key",
        "N8N_OTEL_TRACES_PRODUCTION_ONLY": "false",
    }


def n8n_cli(n8n: str, env: dict[str, str], work: Path, *args: str) -> None:
    """Run one n8n CLI command to completion; a non-zero exit fails the scenario."""
    subprocess.run(  # noqa: S603 - a pinned tool
        [n8n, *args],
        env=env,
        cwd=work,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=True,
        timeout=COMMAND_TIMEOUT,
    )


def post_webhook(port: int) -> None:
    """Post one request to the workflow's webhook and wait for the answer.

    Right after start-up the server may not have registered the activated workflow's
    webhook yet, and answers 404. A 404 never reaches the mock, so the request is sent
    again until the webhook answers or START_TIMEOUT passes. Any other status is final.
    """
    deadline = time.monotonic() + START_TIMEOUT
    while True:
        request = urllib.request.Request(
            f"http://127.0.0.1:{port}/webhook/spike",
            data=json.dumps({"q": "What is the weather in Paris?"}).encode("utf-8"),
            headers={"content-type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=WEBHOOK_TIMEOUT) as response:  # noqa: S310
                response.read()
            return
        except urllib.error.HTTPError as error:
            if error.code != HTTPStatus.NOT_FOUND or time.monotonic() >= deadline:
                raise
        time.sleep(0.5)


if __name__ == "__main__":
    main()
