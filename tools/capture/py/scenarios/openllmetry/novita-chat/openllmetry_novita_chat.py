"""OpenAI client call through Novita, traced by OpenLLMetry."""

from __future__ import annotations

import os

from openai import OpenAI
from opentelemetry.instrumentation.openai import OpenAIInstrumentor

from capture_py.harness import finish, real_host_client, setup

MODEL = "deepseek/deepseek-v3.2"
BASE_URL = "https://api.novita.ai/openai"


def main() -> None:
    """Run one chat completion."""
    provider = setup()
    OpenAIInstrumentor().instrument(tracer_provider=provider)
    client = OpenAI(
        base_url=BASE_URL,
        api_key=os.environ["OPENAI_API_KEY"],
        max_retries=0,
        http_client=real_host_client(),
    )
    client.chat.completions.create(
        model=MODEL,
        messages=[{"role": "user", "content": "What is the capital of France?"}],
    )
    finish(provider)
