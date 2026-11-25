"""Instructor structured output through a tool call, traced by OpenInference."""

from __future__ import annotations

import os

import instructor
import openai
from openinference.instrumentation.instructor import InstructorInstrumentor
from openinference.instrumentation.openai import OpenAIInstrumentor
from pydantic import BaseModel

from capture_py.harness import finish, mock_url, setup

MODEL = "gpt-4o-mini-2024-07-18"


class User(BaseModel):
    """The model extracted from the prompt."""

    name: str
    age: int


def main() -> None:
    """Extract one User through a tool call."""
    provider = setup()
    InstructorInstrumentor().instrument(tracer_provider=provider)
    OpenAIInstrumentor().instrument(tracer_provider=provider)
    client = instructor.from_openai(
        openai.OpenAI(
            api_key=os.environ["OPENAI_API_KEY"],
            base_url=mock_url("/v1"),
            max_retries=0,
        )
    )
    client.chat.completions.create(
        model=MODEL,
        response_model=User,
        messages=[{"role": "user", "content": "Ada is 36."}],
    )
    finish(provider)


if __name__ == "__main__":
    main()
