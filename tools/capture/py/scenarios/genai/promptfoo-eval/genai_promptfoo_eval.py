"""Promptfoo eval: a contains and an llm-rubric assertion against the mock, traced by Promptfoo."""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
from pathlib import Path
from typing import Any

from capture_py.harness import finish_tool, mock_url, tool_binary

PROMPTFOO_VERSION = "0.125.1"
MOCK_KEY = "capture-mock-key"
MODEL = "gpt-4o-mini"
EVAL_TIMEOUT = 150.0


def main() -> None:
    """Write the config into a temporary directory, run one eval there, then write the versions."""
    promptfoo = tool_binary("promptfoo", PROMPTFOO_VERSION, "node_modules", ".bin", "promptfoo")
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        config = work / "promptfooconfig.yaml"
        # JSON is YAML, and promptfoo reads the file as YAML.
        config.write_text(json.dumps(eval_config(), indent=2), encoding="utf-8")
        subprocess.run(  # noqa: S603 - a pinned tool
            [promptfoo, "eval", "-c", str(config), "--max-concurrency", "1"],
            env=promptfoo_env(work),
            cwd=tmp,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=True,
            timeout=EVAL_TIMEOUT,
        )
    finish_tool(None, {"promptfoo": PROMPTFOO_VERSION})


def promptfoo_env(work: Path) -> dict[str, str]:
    """The eval's whole environment: its config directory, the switches off, and the OTel export."""
    config_dir = work / "config"
    config_dir.mkdir()
    return {
        "PATH": os.environ["PATH"],
        "HOME": os.environ["HOME"],
        "PROMPTFOO_CONFIG_DIR": str(config_dir),
        "PROMPTFOO_DISABLE_TELEMETRY": "1",
        "PROMPTFOO_DISABLE_UPDATE": "1",
        "PROMPTFOO_DISABLE_SHARING": "1",
        "PROMPTFOO_OTEL_ENABLED": "true",
        "PROMPTFOO_OTEL_ENDPOINT": os.environ["CETRA_CAPTURE_OTLP_ENDPOINT"],
    }


def eval_config() -> dict[str, Any]:
    """One prompt, one provider, one test with a contains and an llm-rubric assertion."""
    provider: dict[str, Any] = {
        "id": f"openai:chat:{MODEL}",
        "config": {"apiBaseUrl": mock_url("/v1"), "apiKey": MOCK_KEY},
    }
    return {
        "description": "cetra capture",
        "prompts": ["What is the capital of {{country}}?"],
        "providers": [provider],
        # Promptfoo starts its OTel SDK only when the suite enables tracing.
        "tracing": {"enabled": True},
        "tests": [
            {
                "vars": {"country": "France"},
                "assert": [
                    {"type": "contains", "value": "Paris"},
                    {
                        "type": "llm-rubric",
                        "value": "answers with a city",
                        "provider": provider,
                    },
                ],
            }
        ],
    }


if __name__ == "__main__":
    main()
