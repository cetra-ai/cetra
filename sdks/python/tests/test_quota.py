"""Tests for the pause after a 402 (plan p-entitlements M3 § P11, D-P-34)."""

import logging
from typing import Protocol

import pytest
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import SimpleSpanProcessor
from opentelemetry.trace import Tracer

import llmobs
from llmobs._quota import QuotaPauseExporter, QuotaState, quota_session
from llmobs._runtime import Runtime

# 2027-01-15T08:00:00Z: the wall clock the warnings are checked against.
WALL = 1_800_000_000.0
PAUSE_WARNING = (
    "llmobs: the organization reached its monthly units cap; telemetry is dropped until %s "
    "(retrying at most every hour)"
)


class Recorder(Protocol):
    """The surface of the ``http_recorder`` fixture used by the tests."""

    url: str
    requests: list[tuple[str, str, dict[str, str], bytes]]

    def queue_response(self, status: int, headers: dict[str, str], body: bytes) -> None: ...


class FakeClock:
    """A monotonic clock the test moves by hand."""

    def __init__(self) -> None:
        self.now = 0.0

    def __call__(self) -> float:
        return self.now


def trace_posts(recorder: Recorder) -> int:
    return sum(1 for _, path, _, _ in recorder.requests if path == "/v1/traces")


def score_posts(recorder: Recorder) -> int:
    return sum(1 for _, path, _, _ in recorder.requests if path == "/api/v1/scores")


def llmobs_warnings(caplog: pytest.LogCaptureFixture) -> list[str]:
    return [record.getMessage() for record in caplog.records if record.name == "llmobs"]


def otlp_tracer(recorder: Recorder, state: QuotaState) -> Tracer:
    """A tracer whose every span ends in one synchronous export through the pause wrapper."""
    exporter = OTLPSpanExporter(
        endpoint=f"{recorder.url}/v1/traces",
        timeout=5,
        session=quota_session(state),
    )
    provider = TracerProvider()
    provider.add_span_processor(SimpleSpanProcessor(QuotaPauseExporter(state, exporter)))
    return provider.get_tracer("test")


@pytest.fixture
def clock() -> FakeClock:
    return FakeClock()


@pytest.fixture
def state(clock: FakeClock) -> QuotaState:
    return QuotaState(clock=clock, wall=lambda: WALL + clock.now)


def test_402_pauses_span_export_until_retry_after(
    http_recorder: Recorder, clock: FakeClock, state: QuotaState
) -> None:
    tracer = otlp_tracer(http_recorder, state)
    http_recorder.queue_response(402, {"Retry-After": "60"}, b"")

    tracer.start_span("first").end()
    assert trace_posts(http_recorder) == 1

    tracer.start_span("paused").end()
    clock.now = 59.0
    tracer.start_span("still paused").end()
    assert trace_posts(http_recorder) == 1

    clock.now = 60.0
    tracer.start_span("resumed").end()
    assert trace_posts(http_recorder) == 2


def test_retry_after_is_capped_at_one_hour(clock: FakeClock, state: QuotaState) -> None:
    state.on_402("86400")

    clock.now = 3599.0
    assert state.paused()
    clock.now = 3600.0
    assert not state.paused()


@pytest.mark.parametrize(
    "retry_after", [None, "", "soon", "0", "-5", "Wed, 21 Oct 2015 07:28:00 GMT"]
)
def test_unusable_retry_after_pauses_one_hour(
    clock: FakeClock, state: QuotaState, retry_after: str | None
) -> None:
    state.on_402(retry_after)

    clock.now = 3599.0
    assert state.paused()
    clock.now = 3600.0
    assert not state.paused()


def test_one_warning_per_pause_with_the_utc_reset_instant(
    caplog: pytest.LogCaptureFixture, clock: FakeClock, state: QuotaState
) -> None:
    with caplog.at_level(logging.WARNING, logger="llmobs"):
        state.on_402("60")
        state.on_402("60")
        assert llmobs_warnings(caplog) == [PAUSE_WARNING % "2027-01-15T08:01:00Z"]

        clock.now = 60.0
        state.on_402("60")
        assert llmobs_warnings(caplog) == [
            PAUSE_WARNING % "2027-01-15T08:01:00Z",
            PAUSE_WARNING % "2027-01-15T08:02:00Z",
        ]


def test_score_402_pauses_the_trace_exporter(
    score_runtime: tuple[Recorder, Runtime], caplog: pytest.LogCaptureFixture
) -> None:
    recorder, runtime = score_runtime
    recorder.queue_response(402, {"Retry-After": "60"}, b"")

    with caplog.at_level(logging.WARNING, logger="llmobs"):
        llmobs.score(name="quality", value=1.0, trace_id="a" * 32)
        runtime.flush()
        assert score_posts(recorder) == 1

        # The paused score queue drops this one without a request.
        llmobs.score(name="quality", value=0.5, trace_id="a" * 32)
        runtime.flush()
        assert score_posts(recorder) == 1

        runtime.tracer.start_span("traced").end()
        runtime.flush()
        assert trace_posts(recorder) == 0

        # The 402 gets no non-retryable-status warning, only the pause warning.
        warnings = llmobs_warnings(caplog)
        assert len(warnings) == 1
        assert warnings[0].startswith("llmobs: the organization reached its monthly units cap")
