"""Azure OpenAI chat through the OpenAI SDK, traced by OpenInference through cetra's SDK."""

from __future__ import annotations

import os

from openai import AzureOpenAI

from capture_py.harness import cetra_runtime, finish, real_host_client


def main() -> None:
    """Run one chat completion against the deployment."""
    provider = cetra_runtime(["openai"])
    client = AzureOpenAI(
        azure_endpoint="https://capture.openai.azure.com",
        api_version="2024-10-21",
        api_key=os.environ["OPENAI_API_KEY"],
        http_client=real_host_client(),
        max_retries=0,
    )
    client.chat.completions.create(
        model="my-gpt4o-deployment",
        messages=[{"role": "user", "content": "What is the capital of France?"}],
    )
    finish(provider)
