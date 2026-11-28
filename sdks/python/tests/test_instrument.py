"""Tests for the instrumentor registry and wiring (plan F § P10, D-F-7)."""

import asyncio
import sys
import types
from collections.abc import Callable, Mapping
from typing import Any, Protocol

import anthropic
import httpx2
import openai
import pytest
from anthropic.types import TextBlock
from openai.resources.chat.completions import AsyncCompletions, Completions
from opentelemetry.sdk.trace import ReadableSpan
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter
from opentelemetry.util.types import AnyValue

import cetra
import cetra._runtime as runtime_module
from cetra._config import ConfigError
from cetra._host import HOST
from cetra._instrument import REGISTRY, Instrumentor, resolve
from cetra._runtime import Runtime

Memory = tuple[InMemorySpanExporter, Callable[..., Runtime]]
Spans = Callable[[InMemorySpanExporter], list[ReadableSpan]]


class Recorder(Protocol):
    """The surface of the ``http_recorder`` fixture used by the tests."""

    url: str


OPENAI_SPAN = "ChatCompletion"
ANTHROPIC_SPAN = "messages.create"


def attrs(span: ReadableSpan) -> Mapping[str, AnyValue]:
    """The span's attributes; every span these tests create carries them."""
    assert span.attributes is not None
    return span.attributes


def one(finished: list[ReadableSpan], name: str) -> ReadableSpan:
    """The single span named ``name``."""
    matches = [span for span in finished if span.name == name]
    assert len(matches) == 1
    return matches[0]


def llm_spans(finished: list[ReadableSpan], name: str) -> list[ReadableSpan]:
    """The OpenInference spans of kind LLM among ``finished``."""
    return [
        span
        for span in finished
        if attrs(span).get("openinference.span.kind") == "LLM" and span.name == name
    ]


def test_resolve_unknown_name_lists_known_names() -> None:
    with pytest.raises(ConfigError) as info:
        resolve(["nope"])
    message = str(info.value)
    assert "instrument: unknown instrumentor 'nope'" in message
    for name in REGISTRY:
        assert name in message


def test_resolve_missing_module_names_the_extra(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    entry = Instrumentor(
        name="fake", extra="fake", module="cetra_tests_missing_module", cls="FakeInstrumentor"
    )
    monkeypatch.setitem(REGISTRY, entry.name, entry)
    with pytest.raises(ConfigError) as info:
        resolve(["fake"])
    assert str(info.value) == "instrument: 'fake' needs `pip install cetra-sdk[fake]`"


def test_resolve_missing_module_names_the_package_when_there_is_no_extra(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    entry = Instrumentor(
        name="fake",
        extra=None,
        module="cetra_tests_missing_module",
        cls="FakeInstrumentor",
        package="openinference-instrumentation-autogen-agentchat",
    )
    monkeypatch.setitem(REGISTRY, entry.name, entry)
    with pytest.raises(ConfigError) as info:
        resolve(["fake"])
    assert (
        str(info.value)
        == "instrument: 'fake' needs `pip install openinference-instrumentation-autogen-agentchat`"
    )


def test_init_with_bad_name_installs_nothing(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("CETRA_API_KEY", raising=False)
    with pytest.raises(ConfigError):
        cetra.init(api_key="sk-cetra-test", instrument=["nope"])
    assert runtime_module.current() is None


def test_conflicting_dependency_maps_to_the_extra_hint(
    monkeypatch: pytest.MonkeyPatch, memory: Memory
) -> None:
    """A DependencyConflictError becomes the extra hint and leaves nothing installed."""
    from opentelemetry.instrumentation.dependencies import (
        DependencyConflict,
        DependencyConflictError,
    )

    class ConflictInstrumentor:
        def __init__(self) -> None:
            self._is_instrumented_by_opentelemetry = False

        @property
        def is_instrumented_by_opentelemetry(self) -> bool:
            return self._is_instrumented_by_opentelemetry

        def instrument(self, **kwargs: object) -> None:
            conflict = DependencyConflict(required="openai>=99", found="openai 1.0.0")
            raise DependencyConflictError(conflict)

    module = types.ModuleType("cetra_tests_conflict")
    module.ConflictInstrumentor = ConflictInstrumentor  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "cetra_tests_conflict", module)
    monkeypatch.setitem(
        REGISTRY,
        "conflict",
        Instrumentor(
            name="conflict",
            extra="conflict",
            module="cetra_tests_conflict",
            cls="ConflictInstrumentor",
        ),
    )
    _, start = memory
    with pytest.raises(ConfigError) as info:
        start(instrument=["conflict"])
    assert str(info.value) == "instrument: 'conflict' needs `pip install cetra-sdk[conflict]`"
    assert runtime_module.current() is None


def test_openai_end_to_end(memory: Memory, spans: Spans, http_recorder: Recorder) -> None:
    exporter, start = memory
    start(instrument=["openai"])
    client = openai.OpenAI(base_url=f"{http_recorder.url}/v1", api_key="test")

    @cetra.observe(name="ask")
    def ask(question: str) -> str:
        completion = client.chat.completions.create(
            model="gpt-4o-mini", messages=[{"role": "user", "content": question}]
        )
        return completion.choices[0].message.content or ""

    with cetra.context(session_id="s1"):
        answer = ask("hello")
    assert answer == "hi"

    finished = spans(exporter)
    assert {span.name for span in finished} == {"ask", OPENAI_SPAN}
    ours = one(finished, "ask")
    instrumented = one(finished, OPENAI_SPAN)
    assert instrumented.parent is not None
    assert instrumented.parent.span_id == ours.context.span_id
    assert attrs(instrumented)["openinference.span.kind"] == "LLM"
    assert attrs(instrumented)["cetra.session.id"] == "s1"


def test_anthropic_end_to_end(memory: Memory, spans: Spans, http_recorder: Recorder) -> None:
    exporter, start = memory
    start(instrument=["anthropic"])
    # The Anthropic SDK prepends /v1 itself; base_url is the recorder root.
    client = anthropic.Anthropic(base_url=http_recorder.url, api_key="test")

    @cetra.observe(name="ask")
    def ask(question: str) -> str:
        message = client.messages.create(
            model="claude-opus-4-6",
            max_tokens=8,
            messages=[{"role": "user", "content": question}],
        )
        if message.content and isinstance(message.content[0], TextBlock):
            return message.content[0].text
        return ""

    with cetra.context(session_id="s1"):
        answer = ask("hello")
    assert answer == "hi"

    finished = spans(exporter)
    assert {span.name for span in finished} == {"ask", ANTHROPIC_SPAN}
    ours = one(finished, "ask")
    instrumented = one(finished, ANTHROPIC_SPAN)
    assert instrumented.parent is not None
    assert instrumented.parent.span_id == ours.context.span_id
    assert attrs(instrumented)["openinference.span.kind"] == "LLM"
    assert attrs(instrumented)["cetra.session.id"] == "s1"


def test_anthropic_sdk_span_is_not_doubled(
    memory: Memory, spans: Spans, http_recorder: Recorder
) -> None:
    exporter, start = memory
    runtime = start(instrument=["anthropic"])
    # The SDK's own span goes to the provider its client is given (what global mode gives it).
    client = anthropic.Anthropic(
        base_url=http_recorder.url,
        api_key="test",
        open_telemetry={"tracer_provider": runtime.provider},
    )
    client.messages.create(
        model="claude-opus-4-6",
        max_tokens=8,
        messages=[{"role": "user", "content": "hello"}],
    )

    finished = spans(exporter)
    assert [span.name for span in finished] == [ANTHROPIC_SPAN]
    assert not any(
        span.instrumentation_scope is not None
        and span.instrumentation_scope.name.startswith("com.anthropic.sdk")
        for span in finished
    )


def test_anthropic_sdk_span_reaches_cetra_without_instrumentor(
    memory: Memory, spans: Spans, http_recorder: Recorder
) -> None:
    exporter, start = memory
    runtime = start()
    client = anthropic.Anthropic(
        base_url=http_recorder.url,
        api_key="test",
        open_telemetry={"tracer_provider": runtime.provider},
    )
    client.messages.create(
        model="claude-opus-4-6",
        max_tokens=8,
        messages=[{"role": "user", "content": "hello"}],
    )

    finished = spans(exporter)
    assert [span.name for span in finished] == ["anthropic.messages.create"]
    assert attrs(finished[0])["gen_ai.usage.input_tokens"] == 5


def test_capture_content_false_hides_instrumentor_payloads(
    memory: Memory, spans: Spans, http_recorder: Recorder
) -> None:
    exporter, start = memory
    start(instrument=["openai"], capture_content=False)
    client = openai.OpenAI(base_url=f"{http_recorder.url}/v1", api_key="test")
    client.chat.completions.create(
        model="gpt-4o-mini", messages=[{"role": "user", "content": "hello"}]
    )

    (instrumented,) = llm_spans(spans(exporter), OPENAI_SPAN)
    keys = set(attrs(instrumented))
    assert not keys & {"input.value", "output.value"}
    assert not any(key.startswith("llm.input_messages.") for key in keys)
    assert not any(key.startswith("llm.output_messages.") for key in keys)


def test_shutdown_uninstruments(memory: Memory, spans: Spans, http_recorder: Recorder) -> None:
    exporter, start = memory
    original = openai.OpenAI.request
    chat_create = Completions.create
    async_chat_create = AsyncCompletions.create
    start(instrument=["openai"])
    assert openai.OpenAI.request is not original  # wrapped
    assert Completions.create is not chat_create
    assert AsyncCompletions.create is not async_chat_create
    client = openai.OpenAI(base_url=f"{http_recorder.url}/v1", api_key="test")
    client.chat.completions.create(
        model="gpt-4o-mini", messages=[{"role": "user", "content": "hello"}]
    )
    assert len(spans(exporter)) == 1

    runtime_module.shutdown()
    assert openai.OpenAI.request is original  # uninstrumented
    assert Completions.create is chat_create
    assert AsyncCompletions.create is async_chat_create

    # A call after shutdown creates no span, even under a fresh runtime.
    start()
    client.chat.completions.create(
        model="gpt-4o-mini", messages=[{"role": "user", "content": "hello"}]
    )
    assert len(spans(exporter)) == 1  # no new span


OTHER_ENTRIES = [entry for entry in REGISTRY.values() if entry.name not in ("openai", "anthropic")]


@pytest.mark.parametrize("entry", OTHER_ENTRIES, ids=[entry.name for entry in OTHER_ENTRIES])
def test_other_registry_entries_import(entry: Instrumentor) -> None:
    """The twenty other names: module and class exist when the extra or package is installed."""
    install = f"cetra-sdk[{entry.extra}]" if entry.extra is not None else entry.package
    module: Any = pytest.importorskip(entry.module, reason=f"{install} not installed")
    assert getattr(module, entry.cls, None) is not None


# --- The OpenAI client host stamp (D-IN-18, spec 10 § Instrumentors) ---

CHAT_BODY = {
    "id": "c",
    "object": "chat.completion",
    "created": 1,
    "model": "m",
    "choices": [
        {
            "index": 0,
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": "stop",
        }
    ],
    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
}
EMBEDDING_BODY = {
    "object": "list",
    "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}],
    "model": "m",
    "usage": {"prompt_tokens": 1, "total_tokens": 1},
}
STREAM_BODY = (
    b'data: {"id":"c","object":"chat.completion.chunk","created":1,"model":"m",'
    b'"choices":[{"index":0,"delta":{"role":"assistant","content":"hi"},"finish_reason":null}]}\n\n'
    b'data: {"id":"c","object":"chat.completion.chunk","created":1,"model":"m",'
    b'"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\n'
    b"data: [DONE]\n\n"
)


def _json_transport(body: dict[str, Any]) -> httpx2.MockTransport:
    return httpx2.MockTransport(lambda request: httpx2.Response(200, json=body))


def _llm_span(finished: list[ReadableSpan]) -> ReadableSpan:
    """The single OpenInference LLM span among ``finished``."""
    matches = [span for span in finished if attrs(span).get("openinference.span.kind") == "LLM"]
    assert len(matches) == 1
    return matches[0]


def _chat(base_url: str) -> None:
    client = openai.OpenAI(
        base_url=base_url,
        api_key="test",
        http_client=httpx2.Client(transport=_json_transport(CHAT_BODY)),
    )
    client.chat.completions.create(
        model="gpt-4o-mini", messages=[{"role": "user", "content": "hello"}]
    )


def test_openai_span_records_the_client_host(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start(instrument=["openai"])
    _chat("https://api.novita.ai/openai")
    assert attrs(_llm_span(spans(exporter)))["server.address"] == "api.novita.ai"


def test_async_openai_span_records_the_client_host(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start(instrument=["openai"])
    client = openai.AsyncOpenAI(
        base_url="https://api.novita.ai/openai",
        api_key="test",
        http_client=httpx2.AsyncClient(transport=_json_transport(CHAT_BODY)),
    )

    async def call() -> None:
        await client.chat.completions.create(
            model="gpt-4o-mini", messages=[{"role": "user", "content": "hello"}]
        )

    asyncio.run(call())
    assert attrs(_llm_span(spans(exporter)))["server.address"] == "api.novita.ai"


def test_streaming_span_records_the_client_host(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start(instrument=["openai"])
    client = openai.OpenAI(
        base_url="https://api.novita.ai/openai",
        api_key="test",
        http_client=httpx2.Client(
            transport=httpx2.MockTransport(
                lambda request: httpx2.Response(
                    200,
                    headers={"content-type": "text/event-stream"},
                    content=STREAM_BODY,
                )
            )
        ),
    )
    stream = client.chat.completions.create(
        model="gpt-4o-mini",
        messages=[{"role": "user", "content": "hello"}],
        stream=True,
    )
    for _ in stream:
        pass
    assert attrs(_llm_span(spans(exporter)))["server.address"] == "api.novita.ai"


def test_embeddings_span_records_the_client_host(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start(instrument=["openai"])
    client = openai.OpenAI(
        base_url="https://api.deepinfra.com/v1/openai",
        api_key="test",
        http_client=httpx2.Client(transport=_json_transport(EMBEDDING_BODY)),
    )
    client.embeddings.create(model="m", input="hello")
    (embedding,) = [
        span
        for span in spans(exporter)
        if attrs(span).get("openinference.span.kind") == "EMBEDDING"
    ]
    assert attrs(embedding)["server.address"] == "api.deepinfra.com"


def test_host_is_not_stamped_outside_a_call(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    runtime = start(instrument=["openai"])
    runtime.tracer.start_span("manual").end()
    (manual,) = [span for span in spans(exporter) if span.name == "manual"]
    assert "server.address" not in attrs(manual)


def test_existing_server_address_is_kept(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    runtime = start(instrument=["openai"])
    token = HOST.set("api.novita.ai")
    try:
        runtime.tracer.start_span("x", attributes={"server.address": "custom"}).end()
    finally:
        HOST.reset(token)
    (span,) = [span for span in spans(exporter) if span.name == "x"]
    assert attrs(span)["server.address"] == "custom"


def test_no_stamp_without_the_openai_instrumentor(memory: Memory) -> None:
    _, start = memory
    original = Completions.create
    start(instrument=[])
    assert Completions.create is original
