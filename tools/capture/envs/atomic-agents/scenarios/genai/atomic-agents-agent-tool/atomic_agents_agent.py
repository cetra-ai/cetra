"""An Atomic Agents chat agent and an explicit tool call, traced by its instrumentor."""

from __future__ import annotations

import os

from capture_py.harness import finish, mock_url, setup

# Message capture is off by default; the capture turns it on so the llm span
# carries gen_ai.input.messages and gen_ai.output.messages.
os.environ["ATOMIC_AGENTS_TRACE_CAPTURE_MESSAGES"] = "true"

import instructor
import openai
from atomic_agents import (
    AgentConfig,
    AtomicAgent,
    BaseIOSchema,
    BaseTool,
    BasicChatInputSchema,
    BasicChatOutputSchema,
)
from atomic_agents.observability import AtomicAgentsInstrumentor
from pydantic import Field

MODEL = "gpt-4o-mini"


class ReadFileInput(BaseIOSchema):
    """Path of a file to read from the working directory."""

    path: str = Field(..., description="The path of the file to read.")


class ReadFileOutput(BaseIOSchema):
    """Text the file contained."""

    result: str = Field(..., description="The text the file contained.")


class ReadFileTool(BaseTool[ReadFileInput, ReadFileOutput]):
    def run(self, params: ReadFileInput) -> ReadFileOutput:
        """Read the file at `path` and return its text."""
        with open(params.path, encoding="utf-8") as file:
            return ReadFileOutput(result=file.read())


def main() -> None:
    """Run the agent once, then call the tool explicitly inside the same turn span."""
    provider = setup()
    AtomicAgentsInstrumentor().instrument(tracer_provider=provider)
    client = instructor.from_openai(
        openai.OpenAI(
            api_key=os.environ["OPENAI_API_KEY"],
            base_url=mock_url("/v1"),
            max_retries=0,
        )
    )
    agent = AtomicAgent[BasicChatInputSchema, BasicChatOutputSchema](
        config=AgentConfig(client=client, model=MODEL)
    )
    tracer = provider.get_tracer("capture.app")
    with tracer.start_as_current_span("turn"):
        response = agent.run(
            BasicChatInputSchema(chat_message="Read hello.txt and tell me what it says.")
        )
        print("AGENT >>>", response.chat_message)
        print("TOOL >>>", ReadFileTool().run(ReadFileInput(path="hello.txt")).result)
    finish(provider)


if __name__ == "__main__":
    main()
