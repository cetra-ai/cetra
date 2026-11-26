"""Mistral SDK chat completion, traced by OpenInference through cetra's SDK."""

from __future__ import annotations

import os

from mistralai.client import Mistral
from openinference.instrumentation.mistralai import MistralAIInstrumentor

from capture_py.harness import finish, real_host_httpx_client, setup

MODEL = "mistral-small-latest"


def main() -> None:
    """Run one chat completion."""
    provider = setup()
    MistralAIInstrumentor().instrument(tracer_provider=provider)
    client = Mistral(
        api_key=os.environ["OPENAI_API_KEY"],
        # httpx.Client implements the SDK's HttpClient at runtime; its typed build_request differs.
        client=real_host_httpx_client(),  # type: ignore[arg-type]
    )
    client.chat.complete(
        model=MODEL,
        messages=[{"role": "user", "content": "What is the capital of France?"}],  # type: ignore[arg-type]
    )
    finish(provider)
