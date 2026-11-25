"""M9 spike: Oracle Agent Spec (pyagentspec 26.3.1) + openinference-instrumentation-agentspec 0.1.22, LangGraph adapter.
OTLP to the harness sink (base.url); model calls to schemafake on 127.0.0.1:18081 (OpenAI-compatible)."""
import os, sys
sys.path.insert(0, os.path.dirname(__file__))
import schemafake
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import SimpleSpanProcessor
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from opentelemetry import trace

base = open("base.url").read().strip()
import socket, time, urllib.parse
_u = urllib.parse.urlparse(base)
for _ in range(200):  # the harness writes base.url before its sink listens on some runs
    try:
        socket.create_connection((_u.hostname, _u.port), timeout=1).close()
        break
    except OSError:
        time.sleep(0.05)
schemafake.start(18081)
tp = TracerProvider()
tp.add_span_processor(SimpleSpanProcessor(OTLPSpanExporter(endpoint=base + "/v1/traces", headers={"authorization": "Bearer sk-cetra-fake-m9"})))
trace.set_tracer_provider(tp)

from openinference.instrumentation.agentspec import AgentSpecInstrumentor
AgentSpecInstrumentor().instrument(tracer_provider=tp)

from pyagentspec.agent import Agent
from pyagentspec.llms import OpenAiConfig
from pyagentspec.adapters.langgraph import AgentSpecLoader

# The LangGraph adapter ignores OpenAiConfig.url (it builds ChatOpenAI from env); OPENAI_BASE_URL is set in the env file.
llm = OpenAiConfig(name="openai-gpt-4o-mini", model_id="gpt-4o-mini", url="http://127.0.0.1:18081/v1", api_key="sk-fake-m9")
agent = Agent(name="assistant", description="A general purpose agent without tools",
              llm_config=llm, system_prompt="You are a helpful assistant. Help the user answering politely.")
langgraph_agent = AgentSpecLoader().load_component(agent)
response = langgraph_agent.invoke(input={"messages": [{"role": "user", "content": "What is one benefit of tracing AI agents?"}]},
                                  config={"configurable": {"thread_id": "1"}})
print("AGENT >>>", response["messages"][-1].content.strip())
tp.force_flush()
print("calls to fake:", schemafake.CALLS)
