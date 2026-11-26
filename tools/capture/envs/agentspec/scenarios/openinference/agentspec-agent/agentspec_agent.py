"""Oracle Agent Spec agent with one server tool, traced by OpenInference."""

from __future__ import annotations

import os
from typing import Any, cast

from opentelemetry import trace
from opentelemetry.sdk.trace import TracerProvider

from capture_py.harness import finish, mock_url, setup

# The LangGraph adapter's ChatOpenAI is built with no base_url: it ignores
# OpenAiConfig.url and reads OPENAI_BASE_URL instead, so the mock is set here,
# before pyagentspec and the adapter load.
os.environ["OPENAI_BASE_URL"] = mock_url("/v1")

from openinference.instrumentation.agentspec import AgentSpecInstrumentor
from pyagentspec.adapters.langgraph import AgentSpecLoader
from pyagentspec.agent import Agent
from pyagentspec.llms import OpenAiConfig
from pyagentspec.property import StringProperty
from pyagentspec.tools import ServerTool

MODEL = "gpt-4o-mini"

READ_FILE = ServerTool(
    name="read_file",
    description="Read a file from the working directory and return its text.",
    inputs=[StringProperty(title="path")],
)


def read_file(path: str) -> str:
    """Read a file and return its text (the loader's tool_registry callable)."""
    with open(path, encoding="utf-8") as file:
        return file.read()


def main() -> None:
    """Run the agent through one tool call and the answer."""
    provider = setup()
    AgentSpecInstrumentor().instrument(tracer_provider=provider)
    llm = OpenAiConfig(
        name="openai-gpt-4o-mini",
        model_id=MODEL,
        api_key=os.environ["OPENAI_API_KEY"],
    )
    agent = Agent(
        name="assistant",
        description="A general purpose agent that can read files.",
        llm_config=llm,
        system_prompt="You are a helpful assistant. Help the user answering politely.",
        tools=[READ_FILE],
    )
    # load_component's union return type also covers models and tools; an Agent
    # always loads to a compiled LangGraph runnable.
    langgraph_agent = cast(
        "Any", AgentSpecLoader(tool_registry={"read_file": read_file}).load_component(agent)
    )
    response = langgraph_agent.invoke(
        input={
            "messages": [{"role": "user", "content": "Read hello.txt and tell me what it says."}]
        },
        config={"configurable": {"thread_id": "1"}},
    )
    print("AGENT >>>", response["messages"][-1].content)
    finish(cast("TracerProvider", trace.get_tracer_provider()))


if __name__ == "__main__":
    main()
