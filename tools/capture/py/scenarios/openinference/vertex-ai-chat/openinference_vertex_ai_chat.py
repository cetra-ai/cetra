"""Vertex AI generateContent through google-genai, traced by OpenInference through cetra."""

from __future__ import annotations

from google import genai
from google.genai import types
from google.oauth2.credentials import Credentials
from openinference.instrumentation.google_genai import GoogleGenAIInstrumentor

from capture_py.harness import finish, real_host_httpx_client, setup

MODEL = "gemini-2.5-flash"


def main() -> None:
    """Run one generateContent call on Vertex AI."""
    provider = setup()
    GoogleGenAIInstrumentor().instrument(tracer_provider=provider)
    client = genai.Client(
        vertexai=True,
        project="capture",
        location="us-central1",
        credentials=Credentials(token="capture-mock-key"),  # noqa: S106 - a mock token, not a secret
        http_options=types.HttpOptions(httpx_client=real_host_httpx_client()),
    )
    client.models.generate_content(
        model=MODEL,
        contents="What is the capital of France?",
    )
    finish(provider)
