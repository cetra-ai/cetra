import asyncio, os, sys, json, time, uuid
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import common
from common import Fake, Recv, serve, REQLOG
OUT = os.path.abspath(os.environ.get("OUTDIR", "out")); os.makedirs(OUT, exist_ok=True); common.Recv.out = OUT
fp = serve(Fake); rp = serve(Recv)
from datetime import timedelta
from agents import Agent, Runner, ModelSettings
from agents.models.openai_provider import OpenAIProvider
from openai import AsyncOpenAI
from opentelemetry import trace
from opentelemetry.sdk.trace.export import BatchSpanProcessor
from opentelemetry.sdk.resources import Resource
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from temporalio import activity, workflow
from temporalio.client import Client
from temporalio.contrib.opentelemetry import create_tracer_provider, OpenTelemetryPlugin
from temporalio.openai_agents import OpenAIAgentsPlugin, ModelActivityParameters
from temporalio.openai_agents.workflow import activity_as_tool
from temporalio.testing import WorkflowEnvironment
from temporalio.worker import Worker

from wfmod import WeatherWorkflow, get_weather

async def main():
    tp = create_tracer_provider(resource=Resource.create({"service.name": "capture-app"}))
    tp.add_span_processor(BatchSpanProcessor(OTLPSpanExporter(endpoint=f"http://127.0.0.1:{rp}/v1/traces", headers={"Authorization": "Bearer test-key"}, compression=None)))
    trace.set_tracer_provider(tp)
    client_ai = AsyncOpenAI(base_url=f"http://127.0.0.1:{fp}/v1", api_key="sk-fake", max_retries=0)
    plugin = OpenAIAgentsPlugin(model_provider=OpenAIProvider(openai_client=client_ai, use_responses=True), model_params=ModelActivityParameters(start_to_close_timeout=timedelta(seconds=30)), use_otel_instrumentation=True,
                                add_temporal_spans=os.environ.get("NO_TEMPORAL_SPANS") is None)
    t0 = time.time()
    env = await WorkflowEnvironment.start_local(dev_server_existing_path=os.environ["TEMPORAL_BIN"], dev_server_extra_args=["--headless", "--log-level", "warn"], ui=False)
    print("dev server up after %.2fs" % (time.time() - t0))
    plugins = [plugin] + ([OpenTelemetryPlugin(add_temporal_spans=True)] if os.environ.get("OTEL_PLUGIN") else [])
    client = await Client.connect(env.client.service_client.config.target_host, plugins=plugins)
    async with Worker(client, task_queue="tq", workflows=[WeatherWorkflow], activities=[get_weather]):
        t1 = time.time()
        res = await client.execute_workflow(WeatherWorkflow.run, "What is the weather in Paris?", id="wf-" + uuid.uuid4().hex[:8], task_queue="tq")
        print("workflow result:", res, "in %.2fs" % (time.time() - t1))
    tp.force_flush(); tp.shutdown()
    await env.shutdown()
    print("posts:", Recv.n)
    print("upstream:", [(x["path"], x["model"]) for x in REQLOG])
asyncio.run(main())
