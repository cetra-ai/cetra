"""The pause after a ``402`` (plan p-entitlements M3 § P11, D-P-34).

A ``402`` means the organization reached its monthly units cap. The
``Retry-After`` header says when sending may resume. The SDK drops telemetry
until then, at most one hour at a time, and logs one warning per pause. The
trace exporter and the score queue share one ``QuotaState``.
"""

import threading
import time
from collections.abc import Callable, Sequence
from datetime import UTC, datetime

import requests
from opentelemetry.sdk.trace import ReadableSpan
from opentelemetry.sdk.trace.export import SpanExporter, SpanExportResult

from cetra._log import logger

_MAX_PAUSE_S = 3600

_PAUSE_WARNING = (
    "cetra: the organization reached its monthly units cap; telemetry is dropped until %s "
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
        self._lock = threading.Lock()
        self.until = float("-inf")

    def paused(self) -> bool:
        return self._clock() < self.until

    def on_402(self, retry_after: str | None) -> None:
        """Pause for ``Retry-After`` seconds, at most one hour; warn once per pause start.

        A missing, non-digit or zero value pauses for one hour. The warning
        shows ``now + Retry-After`` (not capped), as the plan and the
        TypeScript SDK do.
        """
        seconds = _retry_seconds(retry_after)
        pause = min(seconds, _MAX_PAUSE_S)
        with self._lock:
            was_paused = self.paused()
            self.until = self._clock() + pause
            if not was_paused:
                reset = _reset_text(self._wall() + seconds, fallback=self._wall() + pause)
                logger.warning(_PAUSE_WARNING, reset)


def _retry_seconds(retry_after: str | None) -> int:
    """``Retry-After`` as a positive number of seconds, else one hour.

    Only ASCII digits count, so ``"+5"`` and ``" 5"`` are not accepted.
    """
    if retry_after is not None and retry_after.isascii() and retry_after.isdigit():
        try:
            seconds = int(retry_after)
        except ValueError:  # more digits than Python converts to an int
            return _MAX_PAUSE_S
        if seconds > 0:
            return seconds
    return _MAX_PAUSE_S


def _reset_text(epoch: float, fallback: float) -> str:
    """The UTC instant as ``YYYY-MM-DDTHH:MM:SSZ``; ``fallback`` if ``epoch`` is out of range."""
    try:
        instant = datetime.fromtimestamp(epoch, tz=UTC)
    except (OverflowError, ValueError, OSError):
        instant = datetime.fromtimestamp(fallback, tz=UTC)
    return instant.strftime("%Y-%m-%dT%H:%M:%SZ")


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
        result = self._inner.export(spans)
        if result is not SpanExportResult.SUCCESS and self._state.paused():
            # The failure is the 402 that just started the pause; drop the batch on purpose.
            return SpanExportResult.SUCCESS
        return result

    def shutdown(self) -> None:
        self._inner.shutdown()

    def force_flush(self, timeout_millis: int = 30_000) -> bool:
        return self._inner.force_flush(timeout_millis)
