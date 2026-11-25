"""Haystack pipeline with a prompt builder and a generator, traced by OpenInference."""

from __future__ import annotations

import os

from haystack import Pipeline
from haystack.components.builders import ChatPromptBuilder
from haystack.components.generators.chat import OpenAIChatGenerator
from haystack.dataclasses import ChatMessage
from haystack.utils import Secret
from openinference.instrumentation.haystack import HaystackInstrumentor

from capture_py.harness import finish, mock_url, setup

MODEL = "gpt-4o-mini-2024-07-18"


def main() -> None:
    """Run the pipeline once."""
    provider = setup()
    HaystackInstrumentor().instrument(tracer_provider=provider)
    pipeline = Pipeline()
    pipeline.add_component(
        "prompt",
        ChatPromptBuilder(
            template=[ChatMessage.from_user("Capital of {{country}}?")],
            required_variables=["country"],
        ),
    )
    pipeline.add_component(
        "llm",
        OpenAIChatGenerator(
            model=MODEL,
            api_base_url=mock_url("/v1"),
            api_key=Secret.from_token(os.environ["OPENAI_API_KEY"]),
            max_retries=0,
        ),
    )
    pipeline.connect("prompt.prompt", "llm.messages")
    pipeline.run({"prompt": {"country": "France"}})
    finish(provider)


if __name__ == "__main__":
    main()
