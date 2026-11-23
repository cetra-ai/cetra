"""Cohere SDK v2 chat, traced by OpenInference through cetra's SDK."""

from __future__ import annotations

import os

import cohere
from openinference.instrumentation.cohere import CohereInstrumentor

from capture_py.harness import finish, real_host_httpx_client, setup

MODEL = "command-a-03-2025"


def main() -> None:
    """Run one chat turn."""
    provider = setup()
    CohereInstrumentor().instrument(tracer_provider=provider)
    client = cohere.ClientV2(
        api_key=os.environ["OPENAI_API_KEY"],
        httpx_client=real_host_httpx_client(),
    )
    client.chat(
        model=MODEL,
        messages=[{"role": "user", "content": "What is the capital of France?"}],  # type: ignore[list-item]
    )
    finish(provider)
