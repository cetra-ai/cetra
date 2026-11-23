"""Start a tracer provider aimed at the capture receiver, then record library versions."""

from __future__ import annotations

import importlib.metadata
import json
import os

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


def finish(provider: TracerProvider) -> None:
    """Flush and shut down ``provider``, then write the library versions the runner asked for.

    A missing ``CETRA_CAPTURE_LIBRARIES`` or ``CETRA_CAPTURE_META`` raises ``KeyError``
    naming that variable.
    """
    provider.force_flush()
    provider.shutdown()
    names = os.environ["CETRA_CAPTURE_LIBRARIES"].split(",")
    meta_path = os.environ["CETRA_CAPTURE_META"]
    libraries = {name: importlib.metadata.version(name) for name in names if name}
    with open(meta_path, "w", encoding="utf-8") as meta_file:
        json.dump({"libraries": libraries}, meta_file)


def prefer_identity_otlp_compression() -> None:
    """Make later HTTP OTLP exporters send an identity body.

    Laminar constructs its trace and log exporters with gzip and does not read
    ``OTEL_EXPORTER_OTLP_COMPRESSION``. The receiver accepts only an identity body.
    """
    from collections.abc import Callable
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
