"""Temporal dev server with the OpenAI Agents plugin: one workflow with a tool call."""

from __future__ import annotations

import asyncio
import os
from datetime import timedelta

from agents.models.openai_provider import OpenAIProvider
from openai import AsyncOpenAI
from openinference_temporal_openai_agents_workflow import WeatherWorkflow, get_weather
from opentelemetry import trace
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace.export import BatchSpanProcessor
from temporalio.client import Client
from temporalio.contrib.opentelemetry import OpenTelemetryPlugin, create_tracer_provider
from temporalio.openai_agents import ModelActivityParameters, OpenAIAgentsPlugin
from temporalio.testing import WorkflowEnvironment
from temporalio.worker import Worker

from capture_py.harness import finish_tool, importlib_versions, mock_url, tool_binary

TEMPORAL_CLI_VERSION = "1.10.1"
WORKFLOW_ID = "wf-capture"
TASK_QUEUE = "tq"
LIBRARIES = [
    "openai",
    "openai-agents",
    "openinference-instrumentation-openai-agents",
    "opentelemetry-sdk",
    "temporalio",
    "temporalio-openai-agents",
]


async def run() -> None:
    """Run one workflow whose agent calls a tool activity, then shut the dev server down."""
    temporal = tool_binary("temporal", TEMPORAL_CLI_VERSION, "temporal")
    provider = create_tracer_provider(resource=Resource.create({"service.name": "capture-app"}))
    provider.add_span_processor(BatchSpanProcessor(OTLPSpanExporter()))
    trace.set_tracer_provider(provider)
    openai_client = AsyncOpenAI(
        base_url=mock_url("/v1"),
        api_key=os.environ["OPENAI_API_KEY"],
        max_retries=0,
    )
    plugin = OpenAIAgentsPlugin(
        model_provider=OpenAIProvider(openai_client=openai_client, use_responses=True),
        model_params=ModelActivityParameters(start_to_close_timeout=timedelta(seconds=30)),
        use_otel_instrumentation=True,
    )
    environment = await WorkflowEnvironment.start_local(
        dev_server_existing_path=temporal,
        dev_server_extra_args=["--headless", "--log-level", "warn"],
        ui=False,
    )
    try:
        client = await Client.connect(
            environment.client.service_client.config.target_host,
            plugins=[plugin, OpenTelemetryPlugin(add_temporal_spans=True)],
        )
        async with Worker(
            client,
            task_queue=TASK_QUEUE,
            workflows=[WeatherWorkflow],
            activities=[get_weather],
        ):
            await client.execute_workflow(
                WeatherWorkflow.run,
                "What is the weather in Paris?",
                id=WORKFLOW_ID,
                task_queue=TASK_QUEUE,
            )
    finally:
        await environment.shutdown()
    # ReplaySafeTracerProvider is not the SDK TracerProvider finish_tool takes, so the
    # provider is flushed and shut down here; finish_tool writes the versions only.
    provider.force_flush()
    provider.shutdown()
    finish_tool(None, {**importlib_versions(LIBRARIES), "temporal-cli": TEMPORAL_CLI_VERSION})


def main() -> None:
    """Entry point for the capture runner."""
    asyncio.run(run())


if __name__ == "__main__":
    main()
