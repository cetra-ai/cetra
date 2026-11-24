"""Envoy AI Gateway (`aigw` with Envoy): one chat completion, traced by the external processor."""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
from pathlib import Path
from urllib.parse import urlparse

from openai import OpenAI
from openai.types.chat import ChatCompletionMessageParam

from capture_py.harness import (
    finish_tool,
    free_port,
    importlib_versions,
    mock_url,
    stop_gateway,
    tool_binary,
    wait_http,
)

HERE = Path(__file__).resolve().parent
AIGW_VERSION = "1.2.1"
ENVOY_VERSION = "1.39.1"
START_TIMEOUT = 30.0
STOP_WAIT = 20.0


def main() -> None:
    """Run aigw, send one chat request, then stop it so the exporter flushes at once."""
    aigw = tool_binary("aigw", AIGW_VERSION, "aigw")
    envoy = tool_binary("envoy", ENVOY_VERSION, "envoy")
    listen = free_port()
    admin = free_port()
    proc: subprocess.Popen[bytes] | None = None
    base: str | None = None
    try:
        # A short base path: AIGW_RUNTIME_DIR holds a Unix socket, and a path longer than
        # about 100 characters fails with `bind: invalid argument`.
        base = tempfile.mkdtemp(prefix="aigw-", dir="/tmp")
        config = str(Path(base) / "aigw.yaml")
        template = (HERE / "aigw.tmpl.yaml").read_text(encoding="utf-8")
        template = template.replace("@LISTEN@", str(listen)).replace("@FAKE@", mock_port())
        Path(config).write_text(template, encoding="utf-8")
        # Pre-place Envoy where func-e looks for an installed version, so nothing is downloaded.
        # All four homes are one directory, which is func-e's legacy mode: the binaries live
        # in $DataHome/versions, not envoy-versions. ENVOY_VERSION names the version. func-e
        # then needs no versions list, and envoy-gateway's infra runner gets none from aigw.
        envoy_bin = Path(base) / "versions" / ENVOY_VERSION / "bin"
        envoy_bin.mkdir(parents=True)
        os.symlink(envoy, envoy_bin / "envoy")
        proc = subprocess.Popen(  # noqa: S603 - a pinned binary
            [aigw, "run", config, "--admin-port", str(admin)],
            env=gateway_env(base),
        )
        wait_http(f"http://127.0.0.1:{listen}/v1/models", START_TIMEOUT, proc=proc)
        client = OpenAI(
            base_url=f"http://127.0.0.1:{listen}/v1",
            api_key=os.environ["OPENAI_API_KEY"],
            max_retries=0,
        )
        messages: list[ChatCompletionMessageParam] = [
            {"role": "system", "content": "You are a weather bot."},
            {"role": "user", "content": "What is the weather in Paris?"},
        ]
        client.chat.completions.create(model="gpt-4o-mini", messages=messages, user="user-1")
    finally:
        if proc is not None:
            stop_gateway(proc, STOP_WAIT)
        if base is not None:
            shutil.rmtree(base, ignore_errors=True)
    finish_tool(
        None,
        {"aigw": AIGW_VERSION, "envoy": ENVOY_VERSION, **importlib_versions(["openai"])},
    )


def mock_port() -> str:
    """The mock's port, for the rendered Backend endpoint."""
    return str(urlparse(mock_url()).port or 80)


def gateway_env(base: str) -> dict[str, str]:
    """The gateway's whole environment; the runner clears the parent's."""
    return {
        "PATH": os.environ["PATH"],
        "HOME": os.environ["HOME"],
        "AIGW_CONFIG_HOME": base,
        "AIGW_DATA_HOME": base,
        "AIGW_STATE_HOME": base,
        "AIGW_RUNTIME_DIR": base,
        "ENVOY_VERSION": ENVOY_VERSION,
        "OPENAI_API_KEY": "sk-capture",
        "OTEL_SERVICE_NAME": "aigw",
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT": os.environ["CETRA_CAPTURE_OTLP_ENDPOINT"],
        "OTEL_EXPORTER_OTLP_PROTOCOL": "http/protobuf",
        "OTEL_EXPORTER_OTLP_HEADERS": "Authorization=Bearer capture-mock-key",
        "OTEL_EXPORTER_OTLP_COMPRESSION": "none",
        "OTEL_TRACES_EXPORTER": "otlp",
        "OTEL_METRICS_EXPORTER": "none",
        "OTEL_LOGS_EXPORTER": "none",
    }


if __name__ == "__main__":
    main()
