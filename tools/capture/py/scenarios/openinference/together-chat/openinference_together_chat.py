"""Together SDK chat completion, traced by OpenInference through cetra's SDK."""

from __future__ import annotations

import os

from openinference.instrumentation.together import TogetherInstrumentor
from together import Together

from capture_py.harness import finish, real_host_httpx_client, setup

MODEL = "meta-llama/Llama-3.3-70B-Instruct-Turbo"


def main() -> None:
    """Run one chat completion."""
    provider = setup()
    TogetherInstrumentor().instrument(tracer_provider=provider)
    client = Together(
        api_key=os.environ["OPENAI_API_KEY"],
        http_client=real_host_httpx_client(),
        max_retries=0,
    )
    client.chat.completions.create(
        model=MODEL,
        messages=[{"role": "user", "content": "What is the capital of France?"}],
    )
    finish(provider)
