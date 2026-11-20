"""``get_prompt``, the cache, ``Prompt.compile``, prompt links and ``client.prompts``.

Covers V2.2 and D-V-14, D-V-15.
"""

import json
import logging
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from types import MappingProxyType
from typing import Protocol

import pytest
from opentelemetry.sdk.trace import ReadableSpan
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter

import cetra
from cetra import ApiError, Client, PromptCompileError, PromptNotAvailable, _prompts, get_prompt
from cetra._prompts import Prompt, _PromptCache
from cetra._runtime import Runtime

TEST_API_KEY = "sk-cetra-test"
PROMPT_PATH = "/api/v1/projects/current/prompts/support-reply/resolve"
JSON_HEADERS = {"Content-Type": "application/json"}
CHAT: list[Mapping[str, str]] = [
    {"role": "system", "content": "You help {{ product }} users."},
    {"type": "placeholder", "name": "history"},
    {"role": "user", "content": "{{ question }}"},
]
Memory = tuple[InMemorySpanExporter, Callable[..., Runtime]]
Spans = Callable[[InMemorySpanExporter], list[ReadableSpan]]


class HttpRecorderLike(Protocol):
    """The surface of the ``http_recorder`` fixture used here."""

    url: str
    requests: list[tuple[str, str, dict[str, str], bytes]]

    def queue_response(self, status: int, headers: dict[str, str], body: bytes) -> None: ...

    def respond_with(
        self, handler: Callable[[str, str, bytes], tuple[int, dict[str, str], bytes]]
    ) -> None: ...


def prompt_body(version: int = 1, template: object = CHAT) -> bytes:
    """A PromptVersion body as the resolve route returns it."""
    return json.dumps(
        {
            "name": "support-reply",
            "type": "chat",
            "version": version,
            "template": template,
            "config": {"model": "gpt-4o"},
            "labels": ["production"],
            "variables": ["product", "question"],
            "placeholders": ["history"],
        }
    ).encode()


@dataclass
class Seams:
    """The fake clock and the refresh runner the prompt cache is built with."""

    now: float = 1000.0
    sleeps: list[float] = field(default_factory=list)
    spawned: list[Callable[[], None]] = field(default_factory=list)
    run_spawns: bool = True

    def spawn(self, run: Callable[[], None]) -> None:
        if self.run_spawns:
            run()
        else:
            self.spawned.append(run)


@pytest.fixture
def seams() -> Seams:
    fake = Seams()
    _prompts._cache_instance = _PromptCache(
        now=lambda: fake.now, spawn=fake.spawn, sleep=fake.sleeps.append
    )
    return fake


@pytest.fixture
def served(memory: Memory, http_recorder: HttpRecorderLike) -> HttpRecorderLike:
    """The runtime, pointed at the recorder."""
    _, start = memory
    start(base_url=http_recorder.url)
    return http_recorder


def make_prompt(name: str = "p", version: int | None = 1) -> Prompt:
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


def test_get_prompt_fetches_and_caches(served: HttpRecorderLike, seams: Seams) -> None:
    served.queue_response(200, {**JSON_HEADERS, "ETag": '"v1"'}, prompt_body())
    first = get_prompt("support-reply")
    second = get_prompt("support-reply")
    assert (first.name, first.version, first.type, first.is_fallback) == (
        "support-reply",
        1,
        "chat",
        False,
    )
    assert second.version == 1
    assert len(served.requests) == 1
    method, path, headers, _ = served.requests[0]
    assert method == "GET"
    assert path == PROMPT_PATH + "?label=production"
    assert headers["Authorization"] == f"Bearer {TEST_API_KEY}"


def test_version_entries_never_expire(served: HttpRecorderLike, seams: Seams) -> None:
    served.queue_response(200, JSON_HEADERS, prompt_body(version=2))
    get_prompt("support-reply", version=2)
    seams.now += 3600
    assert get_prompt("support-reply", version=2).version == 2
    assert len(served.requests) == 1
    assert served.requests[0][1] == PROMPT_PATH + "?version=2"


def test_stale_label_is_served_and_revalidated(served: HttpRecorderLike, seams: Seams) -> None:
    served.queue_response(200, {**JSON_HEADERS, "ETag": '"v1"'}, prompt_body(version=1))
    get_prompt("support-reply")
    seams.now += 61
    served.queue_response(304, {"ETag": '"v1"'}, b"")
    # The stale prompt is returned; the refresh runs inline and answers 304.
    assert get_prompt("support-reply").version == 1
    assert len(served.requests) == 2
    assert served.requests[1][2]["If-None-Match"] == '"v1"'
    seams.now += 30  # the 304 restarted the age
    assert get_prompt("support-reply").version == 1
    assert len(served.requests) == 2
    seams.now += 61
    served.queue_response(200, {**JSON_HEADERS, "ETag": '"v2"'}, prompt_body(version=2))
    assert get_prompt("support-reply").version == 1  # stale value, refreshed underneath
    assert served.requests[2][2]["If-None-Match"] == '"v1"'
    assert get_prompt("support-reply").version == 2
    assert len(served.requests) == 3


def test_one_refresh_per_key(served: HttpRecorderLike, seams: Seams) -> None:
    seams.run_spawns = False
    served.queue_response(200, JSON_HEADERS, prompt_body())
    get_prompt("support-reply")
    seams.now += 61
    assert get_prompt("support-reply").version == 1
    assert get_prompt("support-reply").version == 1
    assert len(seams.spawned) == 1
    assert len(served.requests) == 1


def test_ttl_zero_always_fetches(served: HttpRecorderLike, seams: Seams) -> None:
    served.queue_response(200, JSON_HEADERS, prompt_body())
    served.queue_response(200, JSON_HEADERS, prompt_body())
    get_prompt("support-reply", cache_ttl_s=0)
    get_prompt("support-reply", cache_ttl_s=0)
    assert len(served.requests) == 2
    # A version entry is served from the cache whatever cache_ttl_s is (D-V-14).
    served.queue_response(200, JSON_HEADERS, prompt_body(version=1))
    get_prompt("support-reply", version=1, cache_ttl_s=0)
    get_prompt("support-reply", version=1, cache_ttl_s=0)
    assert len(served.requests) == 3


def test_retries_then_succeeds(served: HttpRecorderLike, seams: Seams) -> None:
    served.queue_response(503, {}, b"")
    served.queue_response(503, {"Retry-After": "30"}, b"")
    served.queue_response(200, JSON_HEADERS, prompt_body())
    assert get_prompt("support-reply").version == 1
    assert seams.sleeps == [0.5, 5.0]  # Retry-After 30 is capped to 5 s
    assert len(served.requests) == 3


def test_gives_up_after_three_attempts(served: HttpRecorderLike, seams: Seams) -> None:
    served.respond_with(lambda method, path, body: (503, {}, b""))
    with pytest.raises(PromptNotAvailable) as info:
        get_prompt("support-reply")
    assert info.value.reason == "status 503"
    assert len(served.requests) == 3
    assert seams.sleeps == [0.5, 1.0]


def test_not_found_uses_fallback(served: HttpRecorderLike, seams: Seams) -> None:
    served.queue_response(404, {}, b"")
    prompt = get_prompt("support-reply", fallback="Hi {{ name }}")
    assert prompt.is_fallback is True
    assert prompt.version is None
    assert prompt.type == "text"
    assert prompt.variables == ("name",)
    assert dict(prompt.config) == {}
    assert len(served.requests) == 1  # 404 is not retried


def test_not_found_without_fallback_raises(served: HttpRecorderLike, seams: Seams) -> None:
    served.queue_response(404, {}, b"")
    with pytest.raises(PromptNotAvailable) as info:
        get_prompt("support-reply")
    assert info.value.name == "support-reply"
    assert info.value.reason == "not found"
    assert str(info.value) == "prompt support-reply is not available: not found"
    assert len(served.requests) == 1


def test_invalid_body_is_not_available(served: HttpRecorderLike, seams: Seams) -> None:
    served.queue_response(200, JSON_HEADERS, b'{"name": "support-reply"}')
    with pytest.raises(PromptNotAvailable) as info:
        get_prompt("support-reply")
    assert info.value.reason == "invalid response"


def test_failed_refresh_keeps_stale_and_warns_once(
    served: HttpRecorderLike, seams: Seams, caplog: pytest.LogCaptureFixture
) -> None:
    served.queue_response(200, {**JSON_HEADERS, "ETag": '"v1"'}, prompt_body())
    get_prompt("support-reply")
    served.respond_with(lambda method, path, body: (500, {}, b""))
    seams.now += 61
    with caplog.at_level(logging.WARNING, logger="cetra"):
        assert get_prompt("support-reply").version == 1
        seams.now += 1
        assert get_prompt("support-reply").version == 1
    warnings = [
        record.getMessage()
        for record in caplog.records
        if record.name == "cetra" and record.levelno == logging.WARNING
    ]
    assert len(warnings) == 1
    assert "support-reply" in warnings[0]
    assert "status 500" in warnings[0]
    assert "{{" not in warnings[0]
    seams.now += 61  # the next minute: one more warning
    with caplog.at_level(logging.WARNING, logger="cetra"):
        assert get_prompt("support-reply").version == 1
    warnings = [
        record.getMessage()
        for record in caplog.records
        if record.name == "cetra" and record.levelno == logging.WARNING
    ]
    assert len(warnings) == 2


def test_disabled_sdk(seams: Seams) -> None:
    prompt = get_prompt("support-reply", fallback="Hi {{ name }}")
    assert prompt.is_fallback is True
    assert prompt.compile(name="Ada") == "Hi Ada"
    with pytest.raises(PromptNotAvailable) as info:
        get_prompt("support-reply")
    assert info.value.reason == "sdk disabled"
    assert info.value.name == "support-reply"


def test_compile_links_the_current_span(
    memory: Memory, served: HttpRecorderLike, seams: Seams, spans: Spans
) -> None:
    exporter, _ = memory
    served.queue_response(200, JSON_HEADERS, prompt_body(version=1))
    prompt = get_prompt("support-reply")
    served.queue_response(404, {}, b"")
    fallback = get_prompt("missing", fallback="You help {{ product }}.")
    with cetra.span("compile"):
        messages = prompt.compile(product="cetra", question="hi", history=[])
    with cetra.span("fallback"):
        text = fallback.compile(product="cetra")
    assert messages == [
        {"role": "system", "content": "You help cetra users."},
        {"role": "user", "content": "hi"},
    ]
    assert text == "You help cetra."
    by_name = {span.name: dict(span.attributes or {}) for span in spans(exporter)}
    assert by_name["compile"]["cetra.prompt.name"] == "support-reply"
    assert by_name["compile"]["cetra.prompt.version"] == 1
    assert by_name["fallback"]["cetra.prompt.name"] == "missing"
    assert "cetra.prompt.version" not in by_name["fallback"]


def test_compile_value_types(seams: Seams) -> None:
    prompt = get_prompt("t", fallback="{{ a }}|{{ b }}|{{ c }}|{{ d }}")
    assert prompt.compile(a=True, b=1.0, c=2.5, d=7) == "true|1|2.5|7"
    with pytest.raises(TypeError):
        prompt.compile(a=True, b=1, c=2, d=object())
    with pytest.raises(PromptCompileError) as info:
        prompt.compile(a="x", b="y", c="z")
    assert info.value.kind == "missing_variable"
    assert info.value.name == "d"
    assert str(info.value) == "prompt t: missing variable d"


def test_invalid_fallback_is_a_programming_error(seams: Seams) -> None:
    with pytest.raises(PromptCompileError) as info:
        get_prompt("x", fallback="{{ bad name }}")
    assert info.value.kind == "invalid"
    assert info.value.reason == "not_a_name"
    assert str(info.value) == "prompt x: invalid template at line 1, column 1: not_a_name"


def test_programming_errors(seams: Seams) -> None:
    with pytest.raises(ValueError, match="name is required"):
        get_prompt("")
    with pytest.raises(ValueError, match="not both"):
        get_prompt("x", label="staging", version=1)
    with pytest.raises(ValueError, match="at least 1"):
        get_prompt("x", version=0)
    with pytest.raises(ValueError, match="must not be negative"):
        get_prompt("x", cache_ttl_s=-1)


def test_lru_bound(seams: Seams) -> None:
    cache = _prompts._cache()
    keys = [("http://cetra.test", "p", f"v:{number}") for number in range(1, 1002)]
    for key in keys:
        cache.store(key, make_prompt(version=1), None)
    assert cache.get(keys[0]) is None  # the oldest entry was evicted
    assert cache.get(keys[1]) is not None
    assert cache.get(keys[-1]) is not None


def test_client_prompts(http_recorder: HttpRecorderLike) -> None:
    client = Client(api_key=TEST_API_KEY, base_url=http_recorder.url)
    http_recorder.queue_response(201, JSON_HEADERS, prompt_body(version=1))
    created = client.prompts.create(
        "support-reply",
        type="chat",
        template=CHAT,
        config={"model": "gpt-4o"},
        labels=["production"],
        commit_message="First version",
    )
    method, path, headers, body = http_recorder.requests[0]
    assert (method, path) == ("POST", "/api/v1/projects/current/prompts")
    assert headers["Authorization"] == f"Bearer {TEST_API_KEY}"
    assert json.loads(body) == {
        "name": "support-reply",
        "type": "chat",
        "template": CHAT,
        "config": {"model": "gpt-4o"},
        "labels": ["production"],
        "commit_message": "First version",
    }
    assert created.version == 1

    http_recorder.queue_response(201, JSON_HEADERS, prompt_body(version=2))
    client.prompts.create_version("support-reply", template=CHAT, labels=["staging"])
    method, path, _, body = http_recorder.requests[1]
    assert (method, path) == ("POST", "/api/v1/projects/current/prompts/support-reply/versions")
    assert json.loads(body) == {"template": CHAT, "labels": ["staging"]}

    client.prompts.set_label("support-reply", "production", 2)
    method, path, _, body = http_recorder.requests[2]
    assert (method, path) == (
        "PUT",
        "/api/v1/projects/current/prompts/support-reply/labels/production",
    )
    assert json.loads(body) == {"version": 2}

    http_recorder.queue_response(200, JSON_HEADERS, prompt_body(version=2))
    assert client.prompts.get("support-reply").version == 2
    assert http_recorder.requests[3][1] == PROMPT_PATH + "?label=production"
    http_recorder.queue_response(200, JSON_HEADERS, prompt_body(version=1))
    assert client.prompts.get("support-reply", version=1).version == 1
    assert http_recorder.requests[4][1] == PROMPT_PATH + "?version=1"

    http_recorder.queue_response(
        200, JSON_HEADERS, b'{"data": [{"name": "support-reply"}], "next_cursor": null}'
    )
    assert client.prompts.list() == ["support-reply"]
    assert http_recorder.requests[5][1] == "/api/v1/projects/current/prompts?limit=1000"

    http_recorder.queue_response(200, JSON_HEADERS, b'{"name": "support-reply"}')
    with pytest.raises(ApiError) as info:
        client.prompts.get("support-reply")
    assert info.value.status == 200
    assert info.value.code == "invalid_response"


def test_logs_have_no_template(
    served: HttpRecorderLike, seams: Seams, caplog: pytest.LogCaptureFixture
) -> None:
    with caplog.at_level(logging.DEBUG, logger="cetra"):
        served.queue_response(200, {**JSON_HEADERS, "ETag": '"v1"'}, prompt_body())
        get_prompt("support-reply")
        served.respond_with(lambda method, path, body: (500, {}, b""))
        seams.now += 61
        get_prompt("support-reply")
    text = " ".join(record.getMessage() for record in caplog.records if record.name == "cetra")
    assert "cetra: prompt support-reply refresh failed (status 500)" in text
    assert "You help" not in text
    assert "{{" not in text
    assert "gpt-4o" not in text
    assert TEST_API_KEY not in text


def test_prompt_names_are_exported_and_the_sub_client_is_not() -> None:
    assert "Prompts" not in cetra.__all__
    assert {"Prompt", "PromptCompileError", "PromptNotAvailable", "get_prompt"} <= set(
        cetra.__all__
    )
