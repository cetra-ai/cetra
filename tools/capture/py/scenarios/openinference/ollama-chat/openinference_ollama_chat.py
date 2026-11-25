"""Ollama SDK chat, traced by OpenInference through cetra's SDK."""

from __future__ import annotations

import ollama
from openinference.instrumentation.ollama import OllamaInstrumentor

from capture_py.harness import RealHostHttpxTransport, finish, setup

MODEL = "llama3.1"


def main() -> None:
    """Run one chat turn."""
    provider = setup()
    OllamaInstrumentor().instrument(tracer_provider=provider)
    client = ollama.Client(
        host="http://localhost:11434",
        transport=RealHostHttpxTransport(),
    )
    client.chat(
        model=MODEL,
        messages=[{"role": "user", "content": "What is the capital of France?"}],
    )
    finish(provider)
