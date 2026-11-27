"""M9 spike: Atomic Agents 2.16.0 offline run. OTLP to the harness sink (base.url), model calls to schemafake."""
import os, sys
sys.path.insert(0, os.path.dirname(__file__))
import schemafake
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import SimpleSpanProcessor
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from opentelemetry import trace

base = open("base.url").read().strip()
schemafake.start(18081)
tp = TracerProvider()
tp.add_span_processor(SimpleSpanProcessor(OTLPSpanExporter(endpoint=base + "/v1/traces", headers={"authorization": "Bearer sk-cetra-fake-m9"})))
trace.set_tracer_provider(tp)

import instructor, openai
from pydantic import Field
from sympy import sympify
from atomic_agents import AtomicAgent, AgentConfig, BaseIOSchema, BaseTool, BaseToolConfig, BasicChatInputSchema, BasicChatOutputSchema
from atomic_agents.observability import AtomicAgentsInstrumentor

AtomicAgentsInstrumentor().instrument(tracer_provider=tp)
client = instructor.from_openai(openai.OpenAI(base_url="http://127.0.0.1:18081/v1", api_key="sk-fake-m9"))
agent = AtomicAgent[BasicChatInputSchema, BasicChatOutputSchema](config=AgentConfig(client=client, model="gpt-4o-mini"))
tracer = tp.get_tracer("m9.app")

class CalcIn(BaseIOSchema):
    """Expression to evaluate."""
    expression: str = Field(...)

class CalcOut(BaseIOSchema):
    """Result."""
    result: str = Field(...)

class Calc(BaseTool[CalcIn, CalcOut]):
    def run(self, params: CalcIn) -> CalcOut:
        return CalcOut(result=str(sympify(params.expression)))

with tracer.start_as_current_span("turn"):
    out = agent.run(BasicChatInputSchema(chat_message="What is 12 * 7 + 3?"))
    print("AGENT >>>", out.chat_message)
    print("TOOL >>>", Calc().run(CalcIn(expression="12*7+3")).result)
tp.force_flush()
print("calls to fake:", schemafake.CALLS)
