"""The pause after a ``402`` (plan p-entitlements M3 § P11, D-P-34).

A ``402`` means the organization reached its monthly units cap. The
``Retry-After`` header says when sending may resume. The SDK drops telemetry
until then, at most one hour at a time, and logs one warning per pause. The
trace exporter and the score queue share one ``QuotaState``.
"""

import time
from collections.abc import Callable, Sequence
from datetime import UTC, datetime

import requests
from opentelemetry.sdk.trace import ReadableSpan
from opentelemetry.sdk.trace.export import SpanExporter, SpanExportResult

from llmobs._log import logger

_MAX_PAUSE_S = 3600

_PAUSE_WARNING = (
    "llmobs: the organization reached its monthly units cap; telemetry is dropped until %s "
    "(retrying at most every hour)"
)


class QuotaState:
    """The pause shared by the trace exporter and the score queue.

    ``clock`` is monotonic and decides when the pause ends; ``wall`` gives
    the UTC time shown in the warning. Both are injected by the tests.
    """

    def __init__(
        self,
        clock: Callable[[], float] = time.monotonic,
        wall: Callable[[], float] = time.time,
    ) -> None:
        self._clock = clock
        self._wall = wall
        self.until = float("-inf")

    def paused(self) -> bool:
        return self._clock() < self.until

    def on_402(self, retry_after: str | None) -> None:
        """Pause for ``Retry-After`` seconds, at most one hour; warn once per pause start.

        A missing, non-integer or non-positive value pauses for one hour.
        """
        seconds = _retry_seconds(retry_after)
        pause = min(seconds, _MAX_PAUSE_S)
        was_paused = self.paused()
        self.until = self._clock() + pause
        if not was_paused:
            reset = datetime.fromtimestamp(self._wall() + pause, tz=UTC)
            logger.warning(_PAUSE_WARNING, reset.strftime("%Y-%m-%dT%H:%M:%SZ"))


def _retry_seconds(retry_after: str | None) -> int:
    """``Retry-After`` as a positive number of seconds, else one hour."""
    if retry_after is not None:
        try:
            seconds = int(retry_after)
        except ValueError:
            return _MAX_PAUSE_S
        if seconds > 0:
            return seconds
    return _MAX_PAUSE_S


def quota_session(state: QuotaState) -> requests.Session:
    """A session for the OTLP exporter that records every ``402`` on ``state``."""

    def record(response: requests.Response, *args: object, **kwargs: object) -> None:
        if response.status_code == 402:
            state.on_402(response.headers.get("Retry-After"))

    session = requests.Session()
    session.hooks["response"].append(record)
    return session


class QuotaPauseExporter(SpanExporter):
    """Drops spans without calling the inner exporter while ``state`` is paused."""

    def __init__(self, state: QuotaState, inner: SpanExporter) -> None:
        self._state = state
        self._inner = inner

    def export(self, spans: Sequence[ReadableSpan]) -> SpanExportResult:
        if self._state.paused():
            return SpanExportResult.SUCCESS
        return self._inner.export(spans)

    def shutdown(self) -> None:
        self._inner.shutdown()

    def force_flush(self, timeout_millis: int = 30_000) -> bool:
        return self._inner.force_flush(timeout_millis)
