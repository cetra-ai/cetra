"""Tests for traced generators and async generators (plan F § P6)."""

import asyncio
import gc
import inspect
import logging
from collections.abc import AsyncGenerator, AsyncIterator, Callable, Generator, Iterator, Mapping
from typing import cast

import pytest
from opentelemetry.sdk.trace import ReadableSpan
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter
from opentelemetry.trace import StatusCode
from opentelemetry.util.types import AnyValue

import cetra
from cetra import _generators
from cetra._generators import TracedAsyncGenerator, TracedGenerator
from cetra._runtime import Runtime

Memory = tuple[InMemorySpanExporter, Callable[..., Runtime]]
Spans = Callable[[InMemorySpanExporter], list[ReadableSpan]]


def attrs(span: ReadableSpan) -> Mapping[str, AnyValue]:
    """The span's attributes; every span these tests build carries them."""
    assert span.attributes is not None
    return span.attributes


def started_after(later: ReadableSpan, earlier: ReadableSpan) -> bool:
    """Whether ``later`` started strictly after ``earlier``."""
    assert later.start_time is not None
    assert earlier.start_time is not None
    return later.start_time > earlier.start_time


def by_name(spans: list[ReadableSpan]) -> dict[str, ReadableSpan]:
    """The finished spans by name; every test uses distinct names."""
    return {span.name: span for span in spans}


def test_exhaustion_joined_text(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    @cetra.observe(name="stream")
    def stream(prompt: str) -> Iterator[str]:
        yield "Hel"
        yield "lo"

    assert list(stream("hi")) == ["Hel", "lo"]
    [span] = spans(exporter)
    attributes = attrs(span)
    assert attributes["cetra.input"] == '{"prompt":"hi"}'
    assert attributes["cetra.input.format"] == "json"
    assert attributes["cetra.output"] == "Hello"
    assert attributes["cetra.output.format"] == "text"
    assert span.status.status_code is StatusCode.UNSET


def test_exhaustion_json_list(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    @cetra.observe(name="numbers")
    def numbers() -> Iterator[dict[str, int]]:
        yield {"n": 1}
        yield {"n": 2}

    @cetra.observe(name="mixed")
    def mixed() -> Iterator[object]:
        yield "a"
        yield 1

    assert list(numbers()) == [{"n": 1}, {"n": 2}]
    assert list(mixed()) == ["a", 1]
    finished = spans(exporter)
    names = by_name(finished)
    numbers_attributes = attrs(names["numbers"])
    assert numbers_attributes["cetra.input"] == "{}"
    assert numbers_attributes["cetra.output"] == '[{"n":1},{"n":2}]'
    assert numbers_attributes["cetra.output.format"] == "json"
    mixed_attributes = attrs(names["mixed"])
    assert mixed_attributes["cetra.output"] == '["a",1]'
    assert mixed_attributes["cetra.output.format"] == "json"


def test_span_starts_at_first_next(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    @cetra.observe(name="gen")
    def gen() -> Iterator[int]:
        yield 1
        yield 2

    with cetra.span("outer"):
        generator = gen()
        with cetra.span("middle"):
            pass
        assert next(generator) == 1
        assert next(generator) == 2
        with pytest.raises(StopIteration):
            next(generator)
    names = by_name(spans(exporter))
    gen_span = names["gen"]
    middle_span = names["middle"]
    outer_span = names["outer"]
    # "middle", created between gen() and the first next(), is a sibling of
    # the generator's span, and start times follow creation order.
    assert gen_span.parent is not None
    assert gen_span.parent.span_id == outer_span.context.span_id
    assert middle_span.parent is not None
    assert middle_span.parent.span_id == outer_span.context.span_id
    assert gen_span.start_time is not None
    assert started_after(gen_span, middle_span)
    assert started_after(gen_span, outer_span)


def test_span_current_only_while_body_runs(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()
    body_ids: list[str | None] = []
    consumer_ids: list[str | None] = []

    @cetra.observe(name="gen")
    def gen() -> Iterator[str]:
        for text in ("a", "b"):
            body_ids.append(cetra.current_span_id())
            yield text

    with cetra.span("consumer"):
        consumer_id = cetra.current_span_id()
        for _ in gen():
            consumer_ids.append(cetra.current_span_id())
    names = by_name(spans(exporter))
    gen_span = names["gen"]
    assert body_ids == [format(gen_span.context.span_id, "016x")] * 2
    assert consumer_ids == [consumer_id, consumer_id]
    assert gen_span.parent is not None
    assert gen_span.parent.span_id == names["consumer"].context.span_id


def test_break_ends_ok_with_partial_items(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    @cetra.observe(name="partial")
    def partial() -> Iterator[str]:
        yield "a"
        yield "b"
        yield "c"

    for _ in partial():
        break
    gc.collect()
    [span] = spans(exporter)
    attributes = attrs(span)
    assert span.status.status_code is StatusCode.OK
    assert attributes["cetra.output"] == "a"
    assert attributes["cetra.output.format"] == "text"


def test_send_and_close(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    @cetra.observe(name="echo")
    def echo() -> Generator[str, int, None]:
        got = yield "start"
        yield f"got:{got}"

    generator = echo()
    assert isinstance(generator, TracedGenerator)
    assert generator.send(None) == "start"
    assert generator.send(7) == "got:7"
    generator.close()
    [span] = spans(exporter)
    attributes = attrs(span)
    assert attributes["cetra.input"] == "{}"
    assert attributes["cetra.output"] == "startgot:7"
    assert attributes["cetra.output.format"] == "text"
    assert span.status.status_code is StatusCode.OK


def test_body_exception_error_and_reraised(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    class ScenarioError(Exception):
        pass

    @cetra.observe(name="fails")
    def fails() -> Iterator[str]:
        yield "a"
        raise ScenarioError("boom")

    generator = fails()
    assert next(generator) == "a"
    with pytest.raises(ScenarioError, match="boom"):
        next(generator)
    [span] = spans(exporter)
    attributes = attrs(span)
    assert span.status.status_code is StatusCode.ERROR
    assert span.status.description == "ScenarioError: boom"
    assert "cetra.output" not in attributes
    [event] = span.events
    assert event.name == "exception"
    event_attributes = event.attributes
    assert event_attributes is not None
    assert event_attributes["exception.type"] == "ScenarioError"
    assert event_attributes["exception.message"] == "boom"


def test_throw_from_consumer_error(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    @cetra.observe(name="thrown")
    def thrown() -> Generator[str, None, None]:
        yield "a"
        yield "b"

    generator = thrown()
    assert next(generator) == "a"
    with pytest.raises(ValueError, match="stop"):
        generator.throw(ValueError("stop"))
    [span] = spans(exporter)
    assert span.status.status_code is StatusCode.ERROR
    assert span.status.description == "ValueError: stop"
    [event] = span.events
    assert event.name == "exception"
    event_attributes = event.attributes
    assert event_attributes is not None
    assert event_attributes["exception.type"] == "ValueError"


def test_reduce_output(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    def count_and_first(items: list[object]) -> object:
        return {"count": len(items), "first": items[0] if items else None}

    @cetra.observe(name="reduced", reduce_output=count_and_first)
    def reduced() -> Iterator[str]:
        yield "a"
        yield "b"

    assert list(reduced()) == ["a", "b"]
    [span] = spans(exporter)
    attributes = attrs(span)
    assert attributes["cetra.output"] == '{"count":2,"first":"a"}'
    assert attributes["cetra.output.format"] == "json"


def test_reduce_output_raising_falls_back(
    memory: Memory, spans: Spans, caplog: pytest.LogCaptureFixture
) -> None:
    exporter, start = memory
    start()

    def boom(items: list[object]) -> object:
        raise RuntimeError("no")

    @cetra.observe(name="fallback", reduce_output=boom)
    def fallback() -> Iterator[str]:
        yield "x"
        yield "y"

    with caplog.at_level(logging.DEBUG, logger="cetra"):
        assert list(fallback()) == ["x", "y"]
    [span] = spans(exporter)
    attributes = attrs(span)
    assert attributes["cetra.output"] == "xy"
    assert attributes["cetra.output.format"] == "text"
    assert any("reduce_output" in record.getMessage() for record in caplog.records)


def test_empty_generator_records_no_output_and_skips_reduce(memory: Memory, spans: Spans) -> None:
    """No yielded items → no output attribute at all, and ``reduce_output``
    is not even called (TypeScript does the same in ``recordOutput``)."""
    exporter, start = memory
    start()
    calls: list[int] = []

    def counter(items: list[object]) -> object:
        calls.append(len(items))
        return {"count": len(items)}

    @cetra.observe(name="empty", reduce_output=counter)
    def empty() -> Iterator[int]:
        return
        yield 1  # makes this a generator function that yields nothing

    assert list(empty()) == []
    [span] = spans(exporter)
    assert "cetra.output" not in attrs(span)
    assert calls == []


def test_unbindable_call_records_one_span_with_the_error(memory: Memory, spans: Spans) -> None:
    """Python checks arguments at call time even for generator functions, so a
    call that cannot bind raises before any generator exists: like the sync
    wrapper, one span with the D-F-11 input fallback and the error."""
    exporter, start = memory
    start()

    @cetra.observe(name="bad")
    def bad(a: int, b: int = 1) -> Iterator[int]:
        yield a

    with pytest.raises(TypeError):
        bad(*[1, 2, 3])  # one argument too many: the call cannot bind

    @cetra.observe(name="abad")
    async def abad(a: int) -> AsyncIterator[int]:
        yield a

    with pytest.raises(TypeError):
        abad(*[1, 2])

    by_name_spans = by_name(spans(exporter))
    inputs = {"bad": '{"args":[1,2,3],"kwargs":{}}', "abad": '{"args":[1,2],"kwargs":{}}'}
    for name, expected_input in inputs.items():
        span = by_name_spans[name]
        attributes = attrs(span)
        assert attributes["cetra.input"] == expected_input
        assert attributes["cetra.span.kind"] == "chain"
        assert span.status.status_code is StatusCode.ERROR
        assert span.status.description is not None
        [event] = span.events
        assert event.name == "exception"


def test_capture_content_false_keeps_no_items(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start(capture_content=False)

    @cetra.observe(name="many")
    def many() -> Generator[int, None, None]:
        yield from range(10_000)

    generator = many()
    for _ in range(3):
        next(generator)
    assert cast(TracedGenerator[int, None, None], generator)._items == []
    generator.close()
    [span] = spans(exporter)
    assert dict(attrs(span)) == {"cetra.span.kind": "chain"}


def test_dropped_without_iteration_no_span_no_warning(
    memory: Memory, spans: Spans, caplog: pytest.LogCaptureFixture
) -> None:
    exporter, start = memory
    start()

    @cetra.observe(name="dropped")
    def dropped() -> Iterator[int]:
        yield 1

    generator = dropped()
    del generator
    gc.collect()
    assert spans(exporter) == []
    assert [record for record in caplog.records if record.levelno >= logging.WARNING] == []


def test_gc_ends_half_consumed_generator_with_ok(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    @cetra.observe(name="gc")
    def half() -> Iterator[str]:
        yield "a"
        yield "b"

    generator = half()
    assert next(generator) == "a"
    del generator
    gc.collect()
    [span] = spans(exporter)
    assert span.status.status_code is StatusCode.OK
    assert attrs(span)["cetra.output"] == "a"


def test_without_runtime_inner_generator_unchanged(memory: Memory, spans: Spans) -> None:
    exporter, _start = memory

    @cetra.observe(name="off")
    def off(x: int) -> Iterator[int]:
        yield x

    generator = off(1)
    assert inspect.isgenerator(generator)
    assert not isinstance(generator, TracedGenerator)
    assert list(generator) == [1]
    assert spans(exporter) == []


def test_wrapper_is_a_plain_function(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    @cetra.observe(name="gen")
    def gen() -> Iterator[int]:
        yield 1

    async def agen() -> AsyncIterator[int]:
        yield 1

    traced_async_gen = cetra.observe(name="agen")(agen)
    assert inspect.isgeneratorfunction(gen) is False
    assert inspect.isasyncgenfunction(traced_async_gen) is False
    assert "isasyncgenfunction" in (_generators.__doc__ or "")

    async def main() -> list[int]:
        return [item async for item in traced_async_gen()]

    assert list(gen()) == [1]
    assert asyncio.run(main()) == [1]
    assert set(by_name(spans(exporter))) == {"gen", "agen"}


def test_async_exhaustion_joined_text(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    @cetra.observe(name="stream")
    async def stream(prompt: str) -> AsyncIterator[str]:
        yield "Hel"
        yield "lo"

    async def main() -> list[str]:
        return [text async for text in stream("hi")]

    assert asyncio.run(main()) == ["Hel", "lo"]
    [span] = spans(exporter)
    attributes = attrs(span)
    assert attributes["cetra.input"] == '{"prompt":"hi"}'
    assert attributes["cetra.output"] == "Hello"
    assert attributes["cetra.output.format"] == "text"
    assert span.status.status_code is StatusCode.UNSET


def test_async_exhaustion_json_list(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    @cetra.observe(name="numbers")
    async def numbers() -> AsyncIterator[dict[str, int]]:
        yield {"n": 1}
        yield {"n": 2}

    async def main() -> list[dict[str, int]]:
        return [item async for item in numbers()]

    assert asyncio.run(main()) == [{"n": 1}, {"n": 2}]
    [span] = spans(exporter)
    attributes = attrs(span)
    assert attributes["cetra.input"] == "{}"
    assert attributes["cetra.output"] == '[{"n":1},{"n":2}]'
    assert attributes["cetra.output.format"] == "json"


def test_async_span_starts_at_first_anext(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    @cetra.observe(name="agen")
    async def agen() -> AsyncGenerator[int, None]:
        yield 1
        yield 2

    async def main() -> None:
        with cetra.span("outer"):
            generator = agen()
            with cetra.span("middle"):
                pass
            assert await generator.__anext__() == 1
            await generator.aclose()

    asyncio.run(main())
    names = by_name(spans(exporter))
    agen_span = names["agen"]
    middle_span = names["middle"]
    outer_span = names["outer"]
    assert agen_span.parent is not None
    assert agen_span.parent.span_id == outer_span.context.span_id
    assert middle_span.parent is not None
    assert middle_span.parent.span_id == outer_span.context.span_id
    assert agen_span.start_time is not None
    assert started_after(agen_span, middle_span)
    assert started_after(agen_span, outer_span)


def test_async_span_current_only_while_body_runs(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()
    body_ids: list[str | None] = []
    consumer_ids: list[str | None] = []

    @cetra.observe(name="agen")
    async def agen() -> AsyncIterator[str]:
        for text in ("a", "b"):
            body_ids.append(cetra.current_span_id())
            yield text

    async def main() -> str | None:
        with cetra.span("consumer"):
            consumer_id = cetra.current_span_id()
            async for _ in agen():
                consumer_ids.append(cetra.current_span_id())
        return consumer_id

    consumer_id = asyncio.run(main())
    names = by_name(spans(exporter))
    agen_span = names["agen"]
    assert body_ids == [format(agen_span.context.span_id, "016x")] * 2
    assert consumer_ids == [consumer_id, consumer_id]
    assert agen_span.parent is not None
    assert agen_span.parent.span_id == names["consumer"].context.span_id


def test_async_aclose_ok_with_partial_items(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    @cetra.observe(name="partial")
    async def partial() -> AsyncGenerator[str, None]:
        yield "a"
        yield "b"

    async def main() -> None:
        generator = partial()
        assert await generator.__anext__() == "a"
        await generator.aclose()
        with pytest.raises(StopAsyncIteration):
            await generator.__anext__()

    asyncio.run(main())
    [span] = spans(exporter)
    attributes = attrs(span)
    assert span.status.status_code is StatusCode.OK
    assert attributes["cetra.output"] == "a"
    assert attributes["cetra.output.format"] == "text"


def test_async_exception_error_and_reraised(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    class ScenarioError(Exception):
        pass

    @cetra.observe(name="fails")
    async def fails() -> AsyncIterator[str]:
        yield "a"
        raise ScenarioError("boom")

    async def main() -> None:
        generator = fails()
        assert await generator.__anext__() == "a"
        with pytest.raises(ScenarioError, match="boom"):
            await generator.__anext__()

    asyncio.run(main())
    [span] = spans(exporter)
    attributes = attrs(span)
    assert span.status.status_code is StatusCode.ERROR
    assert span.status.description == "ScenarioError: boom"
    assert "cetra.output" not in attributes
    [event] = span.events
    assert event.name == "exception"
    event_attributes = event.attributes
    assert event_attributes is not None
    assert event_attributes["exception.type"] == "ScenarioError"


def test_async_athrow_from_consumer_error(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    @cetra.observe(name="thrown")
    async def thrown() -> AsyncGenerator[str, None]:
        yield "a"
        yield "b"

    async def main() -> None:
        generator = thrown()
        assert await generator.__anext__() == "a"
        with pytest.raises(ValueError, match="stop"):
            await generator.athrow(ValueError("stop"))

    asyncio.run(main())
    [span] = spans(exporter)
    assert span.status.status_code is StatusCode.ERROR
    assert span.status.description == "ValueError: stop"
    [event] = span.events
    assert event.name == "exception"


def test_async_reduce_output(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    @cetra.observe(name="count", reduce_output=len)
    async def count() -> AsyncIterator[int]:
        yield 1
        yield 2

    async def main() -> list[int]:
        return [item async for item in count()]

    assert asyncio.run(main()) == [1, 2]
    [span] = spans(exporter)
    attributes = attrs(span)
    assert attributes["cetra.output"] == "2"
    assert attributes["cetra.output.format"] == "json"


def test_async_capture_content_false_keeps_no_items(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start(capture_content=False)

    @cetra.observe(name="many")
    async def many() -> AsyncGenerator[int, None]:
        for number in range(10_000):
            yield number

    async def main() -> None:
        generator = many()
        for _ in range(3):
            await generator.__anext__()
        assert cast(TracedAsyncGenerator[int, None], generator)._items == []
        await generator.aclose()

    asyncio.run(main())
    [span] = spans(exporter)
    assert dict(attrs(span)) == {"cetra.span.kind": "chain"}


def test_abandoned_async_generator_exports_nothing(memory: Memory, spans: Spans) -> None:
    exporter, start = memory
    start()

    @cetra.observe(name="abandoned")
    async def abandoned() -> AsyncIterator[int]:
        yield 1
        yield 2

    async def main() -> None:
        generator = abandoned()
        assert await generator.__anext__() == 1
        del generator  # never exhausted, never aclose()d: nothing raises

    asyncio.run(main())
    gc.collect()
    assert spans(exporter) == []


def test_async_without_runtime_inner_unchanged(memory: Memory, spans: Spans) -> None:
    exporter, _start = memory

    @cetra.observe(name="off")
    async def off() -> AsyncIterator[int]:
        yield 1

    async def main() -> bool:
        generator = off()
        is_async_generator = inspect.isasyncgen(generator)
        items = [item async for item in generator]
        return is_async_generator and items == [1]

    assert asyncio.run(main())
    assert spans(exporter) == []
