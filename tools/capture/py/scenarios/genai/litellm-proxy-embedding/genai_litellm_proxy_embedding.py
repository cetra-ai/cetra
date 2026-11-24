"""LiteLLM Proxy embeddings, traced by the proxy's otel callback."""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

from openai import OpenAI

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
LITELLM_PROXY_VERSION = "1.109.1"
MASTER_KEY = (
    "sk-spike-master-key-1234567890abcdef"  # a test value the template ran with, not a secret
)
START_TIMEOUT = 60.0
EXPORT_DELAY = 8.0
STOP_WAIT = 20.0


def main() -> None:
    """Run the proxy, send one embeddings request, then stop it so its exporter flushes."""
    litellm = tool_binary("litellm-proxy", LITELLM_PROXY_VERSION, "bin", "litellm")
    port = free_port()
    with tempfile.TemporaryDirectory() as tmp:
        config = str(Path(tmp) / "config.yaml")
        shutil.copyfile(HERE / "config.yaml", config)
        proc = subprocess.Popen(  # noqa: S603 - a pinned tool
            [litellm, "--config", config, "--port", str(port), "--host", "127.0.0.1"],
            env=gateway_env(),
        )
        try:
            wait_http(f"http://127.0.0.1:{port}/health/liveliness", START_TIMEOUT)
            client = OpenAI(
                base_url=f"http://127.0.0.1:{port}/v1",
                api_key=MASTER_KEY,
                max_retries=0,
            )
            client.embeddings.create(model="text-embedding-3-small", input="hello")
            time.sleep(EXPORT_DELAY)
        finally:
            stop_gateway(proc, STOP_WAIT)
    finish_tool(None, {"litellm": LITELLM_PROXY_VERSION, **importlib_versions(["openai"])})


def gateway_env() -> dict[str, str]:
    """The proxy's whole environment; the runner clears the parent's."""
    return {
        "PATH": os.environ["PATH"],
        "HOME": os.environ["HOME"],
        "FAKE_BASE": mock_url("/v1"),
        "LITELLM_LOCAL_MODEL_COST_MAP": "True",
        "DISABLE_SCHEMA_UPDATE": "true",
        "OTEL_EXPORTER": "otlp_http",
        "OTEL_ENDPOINT": receiver_base_url(),
        # A literal space: LiteLLM sends the value as is, so %20 would arrive encoded.
        "OTEL_HEADERS": "Authorization=Bearer capture-mock-key",
        "OTEL_SERVICE_NAME": "litellm-proxy",
    }


if __name__ == "__main__":
    main()
