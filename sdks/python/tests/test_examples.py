"""Execute the tested code blocks of the README and the docs site (plan F § P12, D-W-6).

Every ```python block preceded by a `<!-- test -->` line (README) or
`{/* test */}` line (site pages) runs through the `memory` pipeline:
`cetra.init` installs the in-memory runtime with the test API key and the score
queue pointed at the `http_recorder` fixture, so the documented examples (score
included) never reach the network. Everything except `init` is the real SDK, so
the examples keep working as the code moves.

A block preceded by `<!-- test:otel -->` or `{/* test:otel */}` is plain
OpenTelemetry: it runs as written with `OTEL_EXPORTER_OTLP_ENDPOINT` pointed at
the recorder, and passes when the recorder got a `POST /v1/traces`.
"""

import re
import sys
from collections.abc import Callable
from pathlib import Path
from types import ModuleType
from typing import Protocol

import pytest
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter

import cetra
from cetra._runtime import Runtime

README = Path(__file__).resolve().parents[1] / "README.md"
SITE = Path(__file__).resolve().parents[3] / "website" / "src" / "content" / "docs"
MARKERS = {
    "<!-- test -->": "sdk",
    "{/* test */}": "sdk",
    "<!-- test:otel -->": "otel",
    "{/* test:otel */}": "otel",
}

# One (kind, slug, code) triple per marked block; the slug is `<file stem>-<nearest heading>`.
_FENCE = re.compile(r"^```python\n(.*?)^```", re.DOTALL | re.MULTILINE)


def _sources() -> list[tuple[str, Path]]:
    sources = [("readme", README)]
    if SITE.is_dir():
        files = sorted([*SITE.rglob("*.md"), *SITE.rglob("*.mdx")])
        sources.extend((path.stem, path) for path in files)
    return sources


def _tested_blocks() -> list[tuple[str, str, str]]:
    found: list[tuple[str, str, str]] = []
    seen: dict[str, int] = {}
    for stem, path in _sources():
        text = path.read_text(encoding="utf-8")
        for match in _FENCE.finditer(text):
            before = text[: match.start()].splitlines()
            if not before or before[-1].strip() not in MARKERS:
                continue
            kind = MARKERS[before[-1].strip()]
            heading = next((line for line in reversed(before[:-1]) if line.startswith("#")), "")
            slug = f"{stem}-" + re.sub(r"[^a-z0-9]+", "-", heading.lower()).strip("-")
            seen[slug] = seen.get(slug, 0) + 1
            if seen[slug] > 1:
                slug = f"{slug}-{seen[slug]}"
            found.append((kind, slug, match.group(1)))
    return found


BLOCKS = _tested_blocks()
SDK_BLOCKS = [(slug, code) for kind, slug, code in BLOCKS if kind == "sdk"]
OTEL_BLOCKS = [(slug, code) for kind, slug, code in BLOCKS if kind == "otel"]


class Recorder(Protocol):
    """The surface of the ``http_recorder`` fixture used by the tests."""

    url: str
    requests: list[tuple[str, str, dict[str, str], bytes]]


class _ExampleSdk(ModuleType):
    """The real ``cetra`` module with ``init`` bound to the test pipeline."""

    def __init__(self, real: ModuleType, start: Callable[..., Runtime], url: str) -> None:
        super().__init__("cetra")
        # ``init`` is skipped: the override below must not be shadowed by a
        # setattr on the instance.
        for name in real.__all__:
            if name != "init":
                setattr(self, name, getattr(real, name))
        self.__all__ = list(real.__all__)
        self._start = start
        self._url = url

    def init(self, **options: object) -> None:
        """Install the in-memory runtime, replacing the example's endpoints."""
        options.pop("api_key", None)
        options.pop("base_url", None)
        self._start(api_key="sk-cetra-test", base_url=self._url, **options)


@pytest.mark.parametrize(("slug", "code"), SDK_BLOCKS, ids=[slug for slug, _ in SDK_BLOCKS])
def test_sdk_block(
    slug: str,
    code: str,
    memory: tuple[InMemorySpanExporter, Callable[..., Runtime]],
    http_recorder: Recorder,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    exporter, start = memory
    sdk = _ExampleSdk(cetra, start, http_recorder.url)
    monkeypatch.setitem(sys.modules, "cetra", sdk)
    exec(  # noqa: S102  # executing the documented example is the test
        compile(code, f"example [{slug}]", "exec"),
        {"cetra": sdk, "__name__": f"example_{slug.replace('-', '_')}"},
    )
    assert exporter.get_finished_spans(), "the example exported no spans"
    if slug == "readme-scores":
        # The score example must deliver through the recorder, never the network.
        assert any(
            method == "POST" and path == "/api/v1/scores"
            for method, path, _, _ in http_recorder.requests
        )


@pytest.mark.parametrize(("slug", "code"), OTEL_BLOCKS, ids=[slug for slug, _ in OTEL_BLOCKS])
def test_otel_block(
    slug: str,
    code: str,
    http_recorder: Recorder,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("OTEL_EXPORTER_OTLP_ENDPOINT", http_recorder.url)
    monkeypatch.setenv("CETRA_API_KEY", "sk-cetra-test")
    exec(  # noqa: S102  # executing the documented example is the test
        compile(code, f"example [{slug}]", "exec"),
        {"__name__": f"example_{slug.replace('-', '_')}"},
    )
    assert any(
        method == "POST" and path == "/v1/traces" for method, path, _, _ in http_recorder.requests
    ), "the example exported no trace"


def test_required_examples_are_marked() -> None:
    """The sections the plan pins must keep their tested blocks (§ P12)."""
    slugs = {slug for _, slug, _ in BLOCKS}
    assert {
        "readme-quick-start",
        "readme-manual-spans-and-llm-span",
        "readme-context-and-baggage",
        "readme-scores",
    } <= slugs
    if SITE.is_dir():
        assert {"quickstart-send-a-trace", "opentelemetry-python"} <= slugs
