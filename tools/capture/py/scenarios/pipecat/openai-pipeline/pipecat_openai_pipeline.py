"""Pipecat's aggregator pipeline with OpenAILLMService, one function, and built-in tracing."""

from __future__ import annotations

import asyncio
import os
from typing import Any, cast

from opentelemetry import trace
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from opentelemetry.sdk.trace import TracerProvider
from pipecat.adapters.llm.schemas.function_schema import FunctionSchema
from pipecat.frames.frames import LLMRunFrame
from pipecat.pipeline.pipeline import Pipeline
from pipecat.pipeline.task import PipelineTask
from pipecat.processors.aggregators.llm_context import LLMContext
from pipecat.processors.aggregators.llm_response_universal import LLMContextAggregatorPair
from pipecat.services.llm_service import FunctionCallParams
from pipecat.services.openai.llm import OpenAILLMService
from pipecat.utils.tracing.setup import setup_tracing
from pipecat.workers.runner import WorkerRunner

from capture_py.harness import finish, mock_url

MODEL = "gpt-4o-mini-2024-07-18"


async def get_weather(params: FunctionCallParams) -> None:
    """Answer the model's weather call."""
    city = str(params.arguments.get("city", ""))
    await params.result_callback({"temperature_c": 18, "conditions": "sunny", "city": city})


async def converse() -> None:
    """Run one user turn through the pipeline, including the function call and the answer."""
    llm = OpenAILLMService(
        api_key=os.environ["OPENAI_API_KEY"],
        base_url=mock_url("/v1"),
        settings=OpenAILLMService.Settings(model=MODEL),
    )
    llm.register_function("get_weather", get_weather)
    schema = FunctionSchema(
        name="get_weather",
        description="Look up the weather for a city.",
        properties={"city": {"type": "string"}},
        required=["city"],
    )
    context = LLMContext(
        messages=[
            {"role": "system", "content": "You are a weather assistant."},
            {"role": "user", "content": "What is the weather in Paris?"},
        ],
        tools=[schema],
    )
    pair = LLMContextAggregatorPair(context)
    pipeline = Pipeline([pair.user(), llm, pair.assistant()])
    task = PipelineTask(pipeline, enable_tracing=True, conversation_id="conv-capture-1")
    running = asyncio.create_task(WorkerRunner(handle_sigint=False).run(task))
    await task.queue_frames([LLMRunFrame()])
    # Stop only after the answer is in the context: stopping earlier cancels the second call.
    await wait_for_answer(context)
    await task.stop_when_done()
    await running


async def wait_for_answer(context: LLMContext) -> None:
    """Return once the last message of ``context`` is the assistant's text answer."""
    for _ in range(400):
        messages: list[Any] = list(context.get_messages())
        if messages and messages[-1].get("role") == "assistant" and messages[-1].get("content"):
            return
        await asyncio.sleep(0.05)
    raise TimeoutError("the pipeline did not answer within 20 s")


def main() -> None:
    """Set up Pipecat's tracing and run one conversation."""
    # setup_tracing reads the OTLP endpoint and protocol from the environment the runner sets.
    setup_tracing("capture-app", exporter=OTLPSpanExporter())
    asyncio.run(converse())
    finish(cast("TracerProvider", trace.get_tracer_provider()))


if __name__ == "__main__":
    main()
