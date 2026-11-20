"""Tests for ``context()`` and the context processor (plan F § P4)."""

import asyncio
import contextvars
import logging
import sys
from collections.abc import Callable, Mapping
from concurrent.futures import ThreadPoolExecutor
from types import MappingProxyType
from typing import Any

import pytest
from opentelemetry import trace as trace_api
from opentelemetry.baggage import get_all, set_baggage
from opentelemetry.context import attach, detach, set_value
from opentelemetry.sdk.trace import ReadableSpan
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter
from opentelemetry.util.types import AnyValue

import cetra
from cetra._context import (
    CONTEXT_KEY,
    ContextValues,
    ExperimentLink,
    current_values,
    experiment_scope,
)
from cetra._prompts import Prompt
from cetra._runtime import Runtime

Memory = tuple[InMemorySpanExporter, Callable[..., Runtime]]
Spans = Callable[[InMemorySpanExporter], list[ReadableSpan]]


def attrs(span: ReadableSpan) -> Mapping[str, AnyValue]:
    """The span's attributes; every span these tests build carries them."""
    assert span.attributes is not None
    return span.attributes


def _redact(value: object) -> object:
    return "[redacted]" if value == "alice@example.com" else value


def test_values_on_span_child_and_third_party_tracer(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    runtime = start(environment="prod", release="1.2.3")
    with cetra.context(
        session_id="chat-42", user_id="u-17", tags=["beta"], metadata={"plan": "pro"}
    ):
        with (
            runtime.tracer.start_as_current_span("outer"),
            runtime.tracer.start_as_current_span("child"),
        ):
            pass
        third_party = runtime.provider.get_tracer("thirdparty")
        with third_party.start_as_current_span("third"):
            pass
    by_name = {span.name: span for span in spans(exporter)}
    assert set(by_name) == {"outer", "child", "third"}
    for name, span in by_name.items():
        attributes = attrs(span)
        assert attributes["cetra.session.id"] == "chat-42", name
        assert attributes["cetra.user.id"] == "u-17", name
        assert attributes["cetra.tags"] == ("beta",), name
        assert attributes["cetra.metadata.plan"] == "pro", name
        assert attributes["cetra.environment"] == "prod", name
        assert attributes["cetra.release"] == "1.2.3", name


def test_nested_merge(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    runtime = start()
    with (
        cetra.context(
            session_id="outer-s",
            user_id="outer-u",
            tags=["a", "b"],
            metadata={"k": "outer", "keep": 1},
        ),
        cetra.context(session_id="inner-s", tags=["b", "c"], metadata={"k": "inner"}),
        runtime.tracer.start_as_current_span("inner"),
    ):
        pass
    attributes = attrs(spans(exporter)[0])
    assert attributes["cetra.session.id"] == "inner-s"  # the inner session wins when given
    assert (
        attributes["cetra.user.id"] == "outer-u"
    )  # the outer user is kept when the inner has none
    assert attributes["cetra.tags"] == ("a", "b", "c")  # union, outer first, no duplicates
    assert attributes["cetra.metadata.k"] == "inner"  # metadata merges by key, inner wins
    assert attributes["cetra.metadata.keep"] == 1


def test_exit_restores_outer_values(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    runtime = start()
    with cetra.context(session_id="outer-s", tags=["a"]):
        with cetra.context(session_id="inner-s", tags=["b"]):
            pass
        assert current_values().session_id == "outer-s"
        assert current_values().tags == ("a",)
        with runtime.tracer.start_as_current_span("after"):
            pass
    assert current_values() == ContextValues()
    attributes = attrs(spans(exporter)[0])
    assert attributes["cetra.session.id"] == "outer-s"
    assert attributes["cetra.tags"] == ("a",)


def test_concurrent_async_tasks_never_share_values(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    runtime = start()

    async def task(session: str, delay: float) -> None:
        with cetra.context(session_id=session, tags=[session]):
            await asyncio.sleep(delay)
            assert current_values().session_id == session
            with runtime.tracer.start_as_current_span(f"span-{session}"):
                pass

    async def main() -> None:
        await asyncio.gather(task("s1", 0.05), task("s2", 0.0))

    asyncio.run(main())
    by_name = {span.name: span for span in spans(exporter)}
    assert attrs(by_name["span-s1"])["cetra.session.id"] == "s1"
    assert attrs(by_name["span-s1"])["cetra.tags"] == ("s1",)
    assert attrs(by_name["span-s2"])["cetra.session.id"] == "s2"
    assert attrs(by_name["span-s2"])["cetra.tags"] == ("s2",)


def test_copy_context_in_executor_keeps_values(memory: Memory) -> None:
    _, start = memory
    start()

    def read() -> str | None:
        return current_values().session_id

    with cetra.context(session_id="chat-42"), ThreadPoolExecutor(max_workers=2) as executor:
        copied = executor.submit(contextvars.copy_context().run, read).result()
        plain = executor.submit(read).result()
    assert copied == "chat-42"  # the documented pattern (D-F-14)
    if not getattr(sys.flags, "thread_inherit_context", False):
        assert plain is None


def test_propagate_true_sets_only_the_two_baggage_keys() -> None:
    with cetra.context(
        session_id="chat-42",
        user_id="u-17",
        tags=["beta"],
        metadata={"plan": "pro"},
        propagate=True,
    ):
        assert dict(get_all()) == {"cetra.session.id": "chat-42", "cetra.user.id": "u-17"}
    assert not get_all()


def test_no_baggage_without_propagate() -> None:
    with cetra.context(session_id="chat-42", tags=["beta"], metadata={"plan": "pro"}):
        assert not get_all()


def test_span_gets_session_user_from_baggage_only(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    runtime = start()
    ctx = set_baggage("cetra.session.id", "from-baggage")
    ctx = set_baggage("cetra.user.id", "u-9", context=ctx)
    token = attach(ctx)
    try:
        with runtime.tracer.start_as_current_span("bagged"):
            pass
    finally:
        detach(token)
    attributes = attrs(spans(exporter)[0])
    assert attributes["cetra.session.id"] == "from-baggage"
    assert attributes["cetra.user.id"] == "u-9"
    assert "cetra.tags" not in attributes


def test_invalid_arguments_are_dropped() -> None:
    bad_ids: dict[str, Any] = {"session_id": 123, "user_id": object(), "tags": "beta"}
    with cetra.context(**bad_ids):
        assert current_values() == ContextValues(tags=("beta",))  # a bare string is one tag
    bad_items: dict[str, Any] = {"tags": ["beta", 1, "gamma"], "metadata": {1: "x", "ok": 2}}
    with cetra.context(**bad_items):
        assert current_values() == ContextValues(tags=("beta", "gamma"), metadata={"ok": 2})
    bad_types: dict[str, Any] = {"tags": 123, "metadata": ["a"]}
    with cetra.context(**bad_types):
        assert current_values() == ContextValues()


def test_metadata_attributes_follow_the_key_rule(
    memory: Memory, spans: Spans, caplog: pytest.LogCaptureFixture
) -> None:
    exporter, start = memory
    runtime = start()
    with (
        caplog.at_level(logging.DEBUG, logger="cetra"),
        cetra.context(metadata={"seats": 3, "obj": {"a": 1}, "bad key": 1}),
    ):
        with runtime.tracer.start_as_current_span("one"):
            pass
        with runtime.tracer.start_as_current_span("two"):
            pass
    for span in spans(exporter):
        attributes = attrs(span)
        assert attributes["cetra.metadata.seats"] == 3
        assert attributes["cetra.metadata.obj"] == '{"a":1}'
        assert "cetra.metadata.bad key" not in attributes
    skips = [
        r for r in caplog.records if r.name == "cetra" and "metadata key skipped" in r.getMessage()
    ]
    assert len(skips) == 1  # logged once per key, not once per span


def test_processor_never_raises_on_garbage_under_the_key(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    runtime = start()
    token = attach(set_value(CONTEXT_KEY, object()))
    try:
        with runtime.tracer.start_as_current_span("garbage"):
            pass
    finally:
        detach(token)
    assert current_values() == ContextValues()
    assert [span.name for span in spans(exporter)] == ["garbage"]


def test_reentering_an_entered_context_is_a_noop(caplog: pytest.LogCaptureFixture) -> None:
    block = cetra.context(session_id="s")
    with block:
        assert current_values().session_id == "s"
        with caplog.at_level(logging.DEBUG, logger="cetra"), block:
            assert current_values().session_id == "s"
        assert current_values().session_id == "s"  # the inner exit detached nothing
    assert current_values() == ContextValues()  # the outer exit did
    messages = [record.getMessage() for record in caplog.records if record.name == "cetra"]
    assert messages == ["cetra: context re-entered; this entry is a no-op"]


def test_async_context_manager_stamps_and_restores(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    runtime = start()

    async def main() -> None:
        async with cetra.context(session_id="chat-9"):
            assert current_values().session_id == "chat-9"
            with runtime.tracer.start_as_current_span("async"):
                pass
        assert current_values() == ContextValues()

    asyncio.run(main())
    assert attrs(spans(exporter)[0])["cetra.session.id"] == "chat-9"


def test_metadata_masked_on_entry(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    runtime = start(mask=_redact)
    with (
        cetra.context(metadata={"email": "alice@example.com", "n": 1}),
        runtime.tracer.start_as_current_span("masked"),
    ):
        pass
    attributes = attrs(spans(exporter)[0])
    assert attributes["cetra.metadata.email"] == "[redacted]"
    assert attributes["cetra.metadata.n"] == 1


def test_experiment_scope_drops_the_callers_values(memory: Memory) -> None:
    _, start = memory
    runtime = start()
    with (
        cetra.context(session_id="s", metadata={"a": 1}),
        runtime.tracer.start_as_current_span("outer"),
    ):
        with experiment_scope("e", "i"):
            assert current_values() == ContextValues(experiment=ExperimentLink("e", "i"))
            assert not trace_api.get_current_span().get_span_context().is_valid
        assert current_values().session_id == "s"


def test_context_inside_a_scope_keeps_the_link() -> None:
    with experiment_scope("e", "i"), cetra.context(user_id="u"):
        assert current_values() == ContextValues(user_id="u", experiment=ExperimentLink("e", "i"))


def _prompt(name: str, version: int | None) -> Prompt:
    return Prompt(
        name=name,
        type="text",
        template="x",
        version=version,
        config=MappingProxyType({}),
        labels=(),
        variables=(),
        placeholders=(),
        is_fallback=version is None,
    )


def test_context_prompt_is_stamped(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    runtime = start()
    with (
        cetra.context(prompt=_prompt("support-reply", 3)),
        runtime.tracer.start_as_current_span("inside"),
    ):
        pass
    attributes = attrs(spans(exporter)[0])
    assert attributes["cetra.prompt.name"] == "support-reply"
    assert attributes["cetra.prompt.version"] == 3


def test_nested_context_keeps_the_prompt_and_the_inner_one_wins(
    memory: Memory, spans: Spans
) -> None:
    exporter, start = memory
    runtime = start()
    with (
        cetra.context(prompt=_prompt("outer", 1)),
        cetra.context(user_id="u"),
        runtime.tracer.start_as_current_span("kept"),
    ):
        pass
    with (
        cetra.context(prompt=_prompt("outer", 1)),
        cetra.context(prompt=_prompt("inner", 2)),
        runtime.tracer.start_as_current_span("inner"),
    ):
        pass
    with (
        cetra.context(prompt=_prompt("fallback", None)),
        runtime.tracer.start_as_current_span("fallback"),
    ):
        pass
    by_name = {span.name: attrs(span) for span in spans(exporter)}
    assert by_name["kept"]["cetra.prompt.name"] == "outer"
    assert by_name["kept"]["cetra.prompt.version"] == 1
    assert by_name["inner"]["cetra.prompt.name"] == "inner"
    assert by_name["inner"]["cetra.prompt.version"] == 2
    assert by_name["fallback"]["cetra.prompt.name"] == "fallback"
    assert "cetra.prompt.version" not in by_name["fallback"]


def test_context_prompt_rejects_other_types(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    runtime = start()
    bad: dict[str, Any] = {"prompt": "support-reply"}
    with cetra.context(**bad):
        assert current_values().prompt is None
        with runtime.tracer.start_as_current_span("plain"):
            pass
    assert "cetra.prompt.name" not in attrs(spans(exporter)[0])
