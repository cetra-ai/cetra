"""Start a tracer provider aimed at the capture receiver, then record library versions."""

from __future__ import annotations

import importlib.metadata
import json
import os
import signal
import socket
import subprocess
import time
import urllib.error
import urllib.request
from collections.abc import Callable

import httpx
import httpx2
from cetra._config import resolve_config
from cetra._instrument import enable_for, resolve
from cetra._runtime import Runtime, install
from opentelemetry import trace
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import SimpleSpanProcessor


def setup() -> TracerProvider:
    """Install a provider that exports each span as it ends.

    The service name comes from ``OTEL_SERVICE_NAME``. The exporter reads its endpoint,
    protocol, and compression from the environment the runner sets.
    """
    provider = TracerProvider(resource=Resource.create({}))
    provider.add_span_processor(SimpleSpanProcessor(OTLPSpanExporter()))
    trace.set_tracer_provider(provider)
    return provider


def mock_url(path: str = "") -> str:
    """Return the mock base URL plus ``path``.

    A missing ``CETRA_CAPTURE_MOCK_URL`` raises ``KeyError`` naming that variable.
    """
    return os.environ["CETRA_CAPTURE_MOCK_URL"] + path


class RealHostTransport(httpx2.BaseTransport):
    """Sends each request to the capture mock, keeping its path, query and headers (D-IN-3).

    The client keeps the real base URL, so the instrumentation records the real host,
    and the request's ``Host`` header still names it for the mock's check.
    """

    def __init__(self) -> None:
        self._mock = httpx2.URL(mock_url())
        self._inner = httpx2.HTTPTransport()

    def handle_request(self, request: httpx2.Request) -> httpx2.Response:
        request.url = request.url.copy_with(
            scheme=self._mock.scheme, host=self._mock.host, port=self._mock.port
        )
        return self._inner.handle_request(request)

    def close(self) -> None:
        self._inner.close()


def real_host_client() -> httpx2.Client:
    """An ``httpx2.Client`` whose requests reach the mock under their real host."""
    return httpx2.Client(transport=RealHostTransport())


class RealHostAsyncTransport(httpx2.AsyncBaseTransport):
    """``RealHostTransport`` for async clients (LiveKit's OpenAI plugin, M6 captures A, D-IN-3)."""

    def __init__(self) -> None:
        self._mock = httpx2.URL(mock_url())
        self._inner = httpx2.AsyncHTTPTransport()

    async def handle_async_request(self, request: httpx2.Request) -> httpx2.Response:
        request.url = request.url.copy_with(
            scheme=self._mock.scheme, host=self._mock.host, port=self._mock.port
        )
        return await self._inner.handle_async_request(request)

    async def aclose(self) -> None:
        await self._inner.aclose()


def real_host_async_client() -> httpx2.AsyncClient:
    """An ``httpx2.AsyncClient`` whose requests reach the mock under their real host."""
    return httpx2.AsyncClient(transport=RealHostAsyncTransport())


class RealHostHttpxTransport(httpx.BaseTransport):
    """``RealHostTransport`` for SDKs built on ``httpx`` rather than ``httpx2`` (D-IN-3)."""

    def __init__(self) -> None:
        self._mock = httpx.URL(mock_url())
        self._inner = httpx.HTTPTransport()

    def handle_request(self, request: httpx.Request) -> httpx.Response:
        request.url = request.url.copy_with(
            scheme=self._mock.scheme, host=self._mock.host, port=self._mock.port
        )
        return self._inner.handle_request(request)

    def close(self) -> None:
        self._inner.close()


def real_host_httpx_client() -> httpx.Client:
    """An ``httpx.Client`` whose requests reach the mock under their real host."""
    return httpx.Client(transport=RealHostHttpxTransport())


def cetra_runtime(instrument: list[str]) -> TracerProvider:
    """Start cetra's SDK runtime with ``instrument`` on, exporting to the capture receiver.

    This uses cetra's private test seam, as ``sdks/python/tests/conftest.py`` does. The
    exporter is the harness's (uncompressed, no API key), so the spans reach the receiver
    while ``CetraSpanProcessor`` stamps them as the SDK does. ``base_url`` is never
    contacted.
    """
    config = resolve_config(
        api_key="sk-cetra-capture",
        base_url="http://127.0.0.1:9",
        instrument=instrument,
        env={},
    )
    classes = resolve(config.instrument)
    runtime = Runtime.create(config, exporter=OTLPSpanExporter())
    install(runtime)
    enable_for(runtime, classes)
    return runtime.provider


def receiver_base_url() -> str:
    """Return the receiver origin, without the ``/v1/traces`` path.

    A missing ``CETRA_CAPTURE_OTLP_ENDPOINT`` raises ``KeyError`` naming that variable.
    An endpoint that does not end in ``/v1/traces`` raises ``ValueError``.
    """
    endpoint = os.environ["CETRA_CAPTURE_OTLP_ENDPOINT"]
    suffix = "/v1/traces"
    if not endpoint.endswith(suffix):
        raise ValueError("CETRA_CAPTURE_OTLP_ENDPOINT has no /v1/traces suffix")
    return endpoint[: -len(suffix)]


def importlib_versions(names: list[str]) -> dict[str, str]:
    """Return the installed version of each non-empty name in ``names``."""
    return {name: importlib.metadata.version(name) for name in names if name}


def _write_meta(libraries: dict[str, str]) -> None:
    """Write ``{"libraries": libraries}`` to the file ``CETRA_CAPTURE_META`` names.

    A missing ``CETRA_CAPTURE_META`` raises ``KeyError`` naming that variable.
    """
    meta_path = os.environ["CETRA_CAPTURE_META"]
    with open(meta_path, "w", encoding="utf-8") as meta_file:
        json.dump({"libraries": libraries}, meta_file)


def finish(provider: TracerProvider) -> None:
    """Flush and shut down ``provider``, then write the library versions the runner asked for.

    A missing ``CETRA_CAPTURE_LIBRARIES`` or ``CETRA_CAPTURE_META`` raises ``KeyError``
    naming that variable.
    """
    provider.force_flush()
    provider.shutdown()
    _write_meta(importlib_versions(os.environ["CETRA_CAPTURE_LIBRARIES"].split(",")))


def finish_tool(provider: TracerProvider | None, versions: dict[str, str]) -> None:
    """Flush and shut down ``provider``, then write exactly ``versions``.

    For a scenario that runs a pinned binary (a gateway) next to, or instead of, an
    instrumented program. ``provider`` is ``None`` when the program has no spans of its
    own. The runner requires every entry of the scenario's ``libraries`` to be a key of
    ``versions``.

    A missing ``CETRA_CAPTURE_META`` raises ``KeyError`` naming that variable.
    """
    if provider is not None:
        provider.force_flush()
        provider.shutdown()
    _write_meta(versions)


def free_port() -> int:
    """Return a TCP port that was free on ``127.0.0.1`` a moment ago, for a gateway to bind."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        port: int = sock.getsockname()[1]
    return port


def wait_http(
    url: str,
    timeout: float,
    ok: Callable[[int], bool] = lambda status: status == 200,
    proc: subprocess.Popen[bytes] | None = None,
) -> None:
    """Poll ``GET url`` every 0.25 s and return once ``ok(status)`` is true.

    An HTTP error status counts as a status (``HTTPError.code``); connection errors are
    retried. Raises ``TimeoutError`` naming the URL once ``timeout`` seconds have passed.
    With ``proc``, the gateway's process, it raises ``RuntimeError`` naming the URL and the
    exit code as soon as ``proc`` has exited, instead of polling a gateway that is gone.
    """
    deadline = time.monotonic() + timeout
    while True:
        code = proc.poll() if proc is not None else None
        if code is not None:
            raise RuntimeError(f"{url} was never ready: the gateway exited with code {code}")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"{url} did not answer a ready status within {timeout} s")
        status: int | None
        try:
            with urllib.request.urlopen(  # noqa: S310 - a loopback URL the scenario built
                url, timeout=min(remaining, 5.0)
            ) as response:
                status = response.status
        except urllib.error.HTTPError as error:
            status = error.code
        except OSError:
            status = None
        if status is not None and ok(status):
            return
        time.sleep(min(0.25, max(deadline - time.monotonic(), 0.0)))


def stop_gateway(proc: subprocess.Popen[bytes], wait: float) -> None:
    """Stop a gateway with SIGINT (it flushes its spans), killing it after ``wait`` seconds."""
    proc.send_signal(signal.SIGINT)
    try:
        proc.wait(timeout=wait)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()


def tool_binary(*parts: str) -> str:
    """Return the path of a pinned capture binary under ``CETRA_CAPTURE_TOOLS``.

    A missing binary raises ``FileNotFoundError`` naming the path, so a host without the
    binaries fails with a message that says how to fetch them. Call it first.

    A missing ``CETRA_CAPTURE_TOOLS`` raises ``KeyError`` naming that variable.
    """
    path = os.path.join(os.environ["CETRA_CAPTURE_TOOLS"], *parts)
    if not os.path.isfile(path):
        raise FileNotFoundError(
            f"{path} does not exist: run `just capture-tools` (Linux x86_64 only)"
        )
    return path


def prefer_identity_otlp_compression() -> None:
    """Make later HTTP OTLP exporters send an identity body.

    Laminar constructs its trace and log exporters with gzip and does not read
    ``OTEL_EXPORTER_OTLP_COMPRESSION``. The receiver accepts only an identity body.
    """
    from typing import cast

    from opentelemetry.exporter.otlp.proto.http import Compression
    from opentelemetry.exporter.otlp.proto.http._log_exporter import OTLPLogExporter
    from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter

    def wrap(exporter: type[OTLPSpanExporter] | type[OTLPLogExporter]) -> None:
        original = cast("Callable[..., None]", exporter.__init__)

        def init(self: object, *args: object, **kwargs: object) -> None:
            kwargs["compression"] = Compression.NoCompression
            original(self, *args, **kwargs)

        exporter.__init__ = init  # type: ignore[method-assign]

    wrap(OTLPSpanExporter)
    wrap(OTLPLogExporter)
