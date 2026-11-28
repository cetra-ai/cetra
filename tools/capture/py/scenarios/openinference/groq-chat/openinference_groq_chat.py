"""Groq SDK chat completion, traced by OpenInference through cetra's SDK."""

from __future__ import annotations

import os

from groq import Groq
from openinference.instrumentation.groq import GroqInstrumentor

from capture_py.harness import finish, real_host_httpx_client, setup

MODEL = "openai/gpt-oss-20b"


def main() -> None:
    """Run one chat completion."""
    provider = setup()
    GroqInstrumentor().instrument(tracer_provider=provider)
    client = Groq(
        api_key=os.environ["OPENAI_API_KEY"],
        http_client=real_host_httpx_client(),
        max_retries=0,
    )
    client.chat.completions.create(
        model=MODEL,
        messages=[{"role": "user", "content": "What is the capital of France?"}],
    )
    finish(provider)
