"""Prompts from the registry (D-V-14, D-V-15): ``get_prompt``, ``compile``, ``client.prompts``.

The cache is one per process. A fetch failure never raises by itself: ``get_prompt`` returns a
stale entry, else the fallback, else raises ``PromptNotAvailable``.
"""

import os
import threading
import time
import weakref
from collections import OrderedDict
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from types import MappingProxyType
from typing import TYPE_CHECKING, Any, Final, Literal
from urllib.parse import quote

import requests
from opentelemetry import trace as trace_api

from cetra import _runtime
from cetra._client import ApiError, _segment
from cetra._config import Config
from cetra._log import logger
from cetra._template import PromptCompileError, chat_names, render, render_chat, variables

if TYPE_CHECKING:
    from cetra._client import Client

_DEFAULT_LABEL: Final = "production"
_TIMEOUT_S: Final = 5
# The first attempt and two retries (D-V-14).
_MAX_ATTEMPTS: Final = 3
_BACKOFF_S: Final = (0.5, 1.0)
_RETRY_AFTER_CAP_S: Final = 5.0
_MAX_ENTRIES: Final = 1000
_WARN_EVERY_S: Final = 60.0
_PAGE_LIMIT: Final = 1000
_EMPTY_CONFIG: Final[Mapping[str, Any]] = MappingProxyType({})

_Key = tuple[str, str, str]


@dataclass(frozen=True, slots=True)
class Prompt:
    """A prompt version from the registry, or a fallback (D-V-14)."""

    name: str
    type: Literal["text", "chat"]
    template: str | tuple[Mapping[str, str], ...]
    version: int | None  # None for a fallback
    config: Mapping[str, Any]  # {} for a fallback
    labels: tuple[str, ...]
    variables: tuple[str, ...]
    placeholders: tuple[str, ...]
    is_fallback: bool

    def compile(self, **values: object) -> str | list[dict[str, str]]:
        """Fill the template: a string for a text prompt, a message list for a chat prompt.

        Values are ``str``, ``int``, ``float`` or ``bool`` for variables (``true``/``false``
        for booleans, an integral float as an integer) and lists of ``{role, content}``
        mappings for placeholders. Any other value is a ``TypeError``. A missing value or an
        invalid template raises ``PromptCompileError``. On success the current span is linked
        to this prompt (D-V-15).
        """
        scalars, lists = _split_values(values)
        try:
            if isinstance(self.template, str):
                result: str | list[dict[str, str]] = render(self.template, scalars)
            else:
                result = render_chat(self.template, scalars, lists)
        except PromptCompileError as error:
            raise _named(error, self.name) from None
        _link_span(self)
        return result


class PromptNotAvailable(Exception):  # noqa: N818  # public name (D-V-14)
    """No prompt could be fetched and no fallback was given (D-V-14)."""

    def __init__(self, name: str, reason: str) -> None:
        self.name = name
        self.reason = reason
        super().__init__(f"prompt {name} is not available: {reason}")


@dataclass(frozen=True, slots=True)
class _Fetched:
    prompt: Prompt
    etag: str | None


@dataclass(frozen=True, slots=True)
class _NotModified:
    pass


@dataclass(frozen=True, slots=True)
class _Failed:
    reason: str


_Outcome = _Fetched | _NotModified | _Failed


@dataclass(slots=True)
class _Entry:
    prompt: Prompt
    etag: str | None
    fetched_at: float


def _spawn_daemon(run: Callable[[], None]) -> None:
    threading.Thread(target=run, name="cetra-prompt-refresh", daemon=True).start()


class _PromptCache:
    """The process cache: an LRU of prompts, one refresh per key, and the warning clock.

    ``now``, ``spawn`` and ``sleep`` are injectable: tests run refreshes inline and advance a
    fake clock.
    """

    def __init__(
        self,
        *,
        now: Callable[[], float] = time.monotonic,
        spawn: Callable[[Callable[[], None]], None] = _spawn_daemon,
        sleep: Callable[[float], None] = time.sleep,
    ) -> None:
        self.now = now
        self.spawn = spawn
        self.sleep = sleep
        self._lock = threading.Lock()
        self._entries: OrderedDict[_Key, _Entry] = OrderedDict()
        self._in_flight: set[_Key] = set()
        self._last_warning: float | None = None
        if hasattr(os, "register_at_fork"):
            self._register_at_fork()

    def get(self, key: _Key) -> _Entry | None:
        """The entry for ``key``, marked as the most recently used."""
        with self._lock:
            entry = self._entries.get(key)
            if entry is not None:
                self._entries.move_to_end(key)
            return entry

    def store(self, key: _Key, prompt: Prompt, etag: str | None) -> None:
        """Insert or replace an entry; the oldest entry goes once there are too many."""
        with self._lock:
            self._entries[key] = _Entry(prompt, etag, self.now())
            self._entries.move_to_end(key)
            while len(self._entries) > _MAX_ENTRIES:
                self._entries.popitem(last=False)

    def touch(self, key: _Key) -> None:
        """A 304: the stored prompt is still current, so its age restarts."""
        with self._lock:
            entry = self._entries.get(key)
            if entry is not None:
                entry.fetched_at = self.now()

    def refresh_later(self, key: _Key, run: Callable[[], None]) -> None:
        """Run ``run`` through ``spawn`` unless a refresh of ``key`` is already in flight."""
        with self._lock:
            if key in self._in_flight:
                return
            self._in_flight.add(key)

        def guarded() -> None:
            try:
                run()
            except Exception as error:  # noqa: BLE001  # a dead refresh must not reach the app
                logger.debug("cetra: prompt refresh raised (%s)", type(error).__name__)
            finally:
                with self._lock:
                    self._in_flight.discard(key)

        try:
            self.spawn(guarded)
        except Exception as error:  # noqa: BLE001  # never raise into the app (spec 10 rule 4)
            with self._lock:
                self._in_flight.discard(key)
            logger.debug("cetra: prompt refresh not started (%s)", type(error).__name__)

    def note_refresh_failure(self, name: str, reason: str) -> None:
        """Warn about a failed refresh at most once a minute, for the whole cache."""
        with self._lock:
            now = self.now()
            warn = self._last_warning is None or now - self._last_warning >= _WARN_EVERY_S
            if warn:
                self._last_warning = now
        if warn:
            logger.warning("cetra: prompt %s refresh failed (%s)", name, reason)

    def _register_at_fork(self) -> None:
        """Re-create the lock and forget in-flight refreshes in a forked child (D-F-15)."""
        ref = weakref.ref(self)

        def after_in_child() -> None:
            child = ref()
            if child is not None:
                child._reinit()

        os.register_at_fork(after_in_child=after_in_child)

    def _reinit(self) -> None:
        self._lock = threading.Lock()
        self._in_flight = set()


_cache_lock = threading.Lock()
_cache_instance: _PromptCache | None = None


def _cache() -> _PromptCache:
    """The process cache, created on first use."""
    global _cache_instance
    with _cache_lock:
        if _cache_instance is None:
            _cache_instance = _PromptCache()
        return _cache_instance


def _reset_cache() -> None:
    """Forget the process cache. Called by ``_runtime.reset()``; tests only."""
    global _cache_instance
    with _cache_lock:
        _cache_instance = None


def get_prompt(
    name: str,
    *,
    label: str | None = None,
    version: int | None = None,
    fallback: str | Sequence[Mapping[str, str]] | None = None,
    cache_ttl_s: float = 60.0,
) -> Prompt:
    """Fetch a prompt by label (default ``production``) or version, cached (D-V-14).

    A label is fresh for ``cache_ttl_s`` seconds, then served stale while one refresh runs; a
    version never expires. When no prompt can be fetched, the stale entry is returned, else
    ``fallback``, else ``PromptNotAvailable`` is raised. Programming errors raise ``ValueError``.
    Before ``init()``, or with the SDK disabled, the call behaves like a failed fetch.
    """
    if not name:
        raise ValueError("get_prompt: name is required")
    _check_target(label, version)
    if cache_ttl_s < 0:
        raise ValueError("get_prompt: cache_ttl_s must not be negative")
    runtime = _runtime.current()
    config = runtime.config if runtime is not None else None
    if config is None or config.api_key is None:
        return _unavailable(name, fallback, "sdk disabled")
    cache = _cache()
    query = _query(label, version)
    target = f"v:{query['version']}" if version is not None else f"l:{query['label']}"
    key: _Key = (config.base_url, name, target)
    entry = cache.get(key)
    if entry is not None:
        if version is not None:
            # Versions never change, whatever cache_ttl_s is (D-V-14).
            return entry.prompt
        if cache_ttl_s > 0:
            if cache.now() - entry.fetched_at < cache_ttl_s:
                return entry.prompt
            _start_refresh(cache, key, config, name, label, entry)
            return entry.prompt
    outcome = _fetch(config, name, label, version, None, sleep=cache.sleep)
    if isinstance(outcome, _Fetched):
        if version is not None or cache_ttl_s > 0:
            cache.store(key, outcome.prompt, outcome.etag)
        return outcome.prompt
    # A miss sends no ETag, so a 304 cannot come back here.
    reason = outcome.reason if isinstance(outcome, _Failed) else "invalid response"
    return _unavailable(name, fallback, reason)


def _check_target(label: str | None, version: int | None) -> None:
    if label is not None and version is not None:
        raise ValueError("pass a label or a version, not both")
    if version is not None and version < 1:
        raise ValueError("version must be at least 1")


def _query(label: str | None, version: int | None) -> dict[str, str | int]:
    """The resolve query: the version, else the label (``production`` by default)."""
    if version is not None:
        return {"version": version}
    return {"label": label or _DEFAULT_LABEL}


def _start_refresh(
    cache: _PromptCache,
    key: _Key,
    config: Config,
    name: str,
    label: str | None,
    entry: _Entry,
) -> None:
    def run() -> None:
        _refresh(cache, key, config, name, label, entry)

    cache.refresh_later(key, run)


def _refresh(
    cache: _PromptCache,
    key: _Key,
    config: Config,
    name: str,
    label: str | None,
    entry: _Entry,
) -> None:
    """One conditional fetch of a stale label: replace, restart the age, or keep and warn."""
    outcome = _fetch(config, name, label, None, entry.etag, sleep=cache.sleep)
    if isinstance(outcome, _Fetched):
        cache.store(key, outcome.prompt, outcome.etag)
    elif isinstance(outcome, _NotModified):
        cache.touch(key)
    else:
        cache.note_refresh_failure(name, outcome.reason)


def _unavailable(
    name: str, fallback: str | Sequence[Mapping[str, str]] | None, reason: str
) -> Prompt:
    if fallback is None:
        raise PromptNotAvailable(name, reason)
    try:
        return _make_prompt(
            name,
            fallback,
            version=None,
            config=_EMPTY_CONFIG,
            labels=(),
            is_fallback=True,
        )
    except PromptCompileError as error:
        raise _named(error, name) from None


def _fetch(
    config: Config,
    name: str,
    label: str | None,
    version: int | None,
    etag: str | None,
    *,
    sleep: Callable[[float], None],
) -> _Outcome:
    """The resolve request with the retry policy of D-V-14; never raises."""
    api_key = config.api_key
    if api_key is None:
        return _Failed("sdk disabled")
    segment = quote(name, safe="")
    url = f"{config.base_url}/api/v1/projects/current/prompts/{segment}/resolve"
    headers = {"Authorization": f"Bearer {api_key}"}
    if etag is not None:
        headers["If-None-Match"] = etag
    query = _query(label, version)
    attempt = 0
    while True:
        response: requests.Response | None
        try:
            response = requests.get(url, params=query, headers=headers, timeout=_TIMEOUT_S)
        except requests.RequestException:
            response = None
        if response is None:
            reason = "network"
        else:
            status = response.status_code
            if status == 200:
                return _parse_answer(response)
            if status == 304:
                return _NotModified()
            if status == 404:
                return _Failed("not found")
            reason = f"status {status}"
            if status != 429 and status < 500:
                return _Failed(reason)
        if attempt + 1 >= _MAX_ATTEMPTS:
            return _Failed(reason)
        sleep(_retry_wait(response, attempt))
        attempt += 1


def _parse_answer(response: requests.Response) -> _Outcome:
    try:
        body: object = response.json()
    except ValueError:
        return _Failed("invalid response")
    prompt = _parse_prompt(body)
    if prompt is None:
        return _Failed("invalid response")
    return _Fetched(prompt, response.headers.get("ETag"))


def _retry_wait(response: requests.Response | None, attempt: int) -> float:
    """``Retry-After`` seconds when it is ASCII digits, capped at 5 s; else 0.5 s, then 1 s."""
    if response is not None:
        header = response.headers.get("Retry-After")
        if header is not None:
            value = header.strip()
            if value.isascii() and value.isdigit():
                try:
                    seconds = int(value)
                except ValueError:  # more digits than int() takes: far above the cap
                    return _RETRY_AFTER_CAP_S
                return float(min(seconds, _RETRY_AFTER_CAP_S))
    return _BACKOFF_S[attempt]


def _parse_prompt(body: object) -> Prompt | None:
    """A prompt from a PromptVersion body, or ``None`` when the shape is not one."""
    if not isinstance(body, dict):
        return None
    name = body.get("name")
    kind = body.get("type")
    version = body.get("version")
    template = body.get("template")
    config = body.get("config", {})
    labels = body.get("labels", [])
    if not isinstance(name, str) or name == "":
        return None
    if isinstance(version, bool) or not isinstance(version, int) or version < 1:
        return None
    if not isinstance(config, dict):
        return None
    if not isinstance(labels, list) or not all(isinstance(label, str) for label in labels):
        return None
    source: str | list[Any]
    if isinstance(template, str):
        if kind != "text":
            return None
        source = template
    elif kind == "chat" and isinstance(template, list) and all(_is_flat(e) for e in template):
        source = template
    else:
        return None
    try:
        return _make_prompt(
            name,
            source,
            version=version,
            config=MappingProxyType(dict(config)),
            labels=tuple(labels),
            is_fallback=False,
        )
    except PromptCompileError:
        return None


def _is_flat(entry: object) -> bool:
    """A chat entry: a mapping whose values are all strings."""
    return isinstance(entry, dict) and all(isinstance(value, str) for value in entry.values())


def _make_prompt(
    name: str,
    template: str | Sequence[Mapping[str, str]],
    *,
    version: int | None,
    config: Mapping[str, Any],
    labels: tuple[str, ...],
    is_fallback: bool,
) -> Prompt:
    """A prompt with its names worked out from the template. Raises PromptCompileError."""
    if isinstance(template, str):
        return Prompt(
            name=name,
            type="text",
            template=template,
            version=version,
            config=config,
            labels=labels,
            variables=tuple(variables(template)),
            placeholders=(),
            is_fallback=is_fallback,
        )
    entries = tuple(MappingProxyType(dict(entry)) for entry in template)
    names, placeholder_names = chat_names(entries)
    return Prompt(
        name=name,
        type="chat",
        template=entries,
        version=version,
        config=config,
        labels=labels,
        variables=tuple(names),
        placeholders=tuple(placeholder_names),
        is_fallback=is_fallback,
    )


def _named(error: PromptCompileError, prompt: str) -> PromptCompileError:
    """The same compile error, naming the prompt (its message never holds template text)."""
    return PromptCompileError(
        error.kind,
        name=error.name,
        reason=error.reason,
        line=error.line,
        column=error.column,
        prompt=prompt,
    )


def _split_values(
    values: Mapping[str, object],
) -> tuple[dict[str, str], dict[str, list[Mapping[str, str]]]]:
    """Variables as text, placeholders as message lists. Any other type is a ``TypeError``."""
    scalars: dict[str, str] = {}
    lists: dict[str, list[Mapping[str, str]]] = {}
    for key, value in values.items():
        if isinstance(value, str):
            scalars[key] = value
        elif isinstance(value, bool):
            scalars[key] = "true" if value else "false"
        elif isinstance(value, int):
            scalars[key] = str(value)
        elif isinstance(value, float):
            scalars[key] = str(int(value)) if value.is_integer() else repr(value)
        elif isinstance(value, (list, tuple)) and all(_is_message(item) for item in value):
            lists[key] = [dict(item) for item in value]
        else:
            raise TypeError(f"prompt value {key}: unsupported type")
    return scalars, lists


def _is_message(item: object) -> bool:
    return (
        isinstance(item, Mapping)
        and isinstance(item.get("role"), str)
        and isinstance(item.get("content"), str)
    )


def _link_span(prompt: Prompt) -> None:
    """Stamp the prompt on the current recording span (D-V-15); never raises."""
    try:
        span = trace_api.get_current_span()
        if span.is_recording():
            span.set_attribute("cetra.prompt.name", prompt.name)
            if prompt.version is not None:
                span.set_attribute("cetra.prompt.version", prompt.version)
    except Exception as error:  # noqa: BLE001  # never raise into the app (spec 10 rule 4)
        logger.debug("cetra: could not link the prompt (%s)", type(error).__name__)


def _expect_prompt(method: str, template: str, raw: object) -> Prompt:
    """The prompt of a 2xx body; ``ApiError`` when the body has no prompt shape."""
    prompt = _parse_prompt(raw)
    if prompt is None:
        raise ApiError(
            method=method,
            path=template,
            status=200,
            code="invalid_response",
            detail="invalid response",
            errors=(),
        )
    return prompt


class Prompts:
    """``client.prompts``: prompt calls over the REST client. Nothing is cached here."""

    def __init__(self, client: "Client") -> None:
        self._client = client

    def get(self, name: str, *, label: str | None = None, version: int | None = None) -> Prompt:
        """The prompt at ``label`` (default ``production``) or ``version``, read from the server."""
        _check_target(label, version)
        template = "/prompts/{prompt}/resolve"
        raw = self._client._request(
            "GET",
            template,
            f"/prompts/{_segment(name)}/resolve",
            params=_query(label, version),
        )
        return _expect_prompt("GET", template, raw)

    def list(self) -> list[str]:
        """The names of every prompt of the project."""
        names: list[str] = []
        cursor: str | None = None
        while True:
            params: dict[str, str | int] = {"limit": _PAGE_LIMIT}
            if cursor is not None:
                params["cursor"] = cursor
            page = self._client._request("GET", "/prompts", "/prompts", params=params)
            names.extend(entry["name"] for entry in page["data"])
            cursor = page.get("next_cursor")
            if cursor is None:
                return names

    def create(
        self,
        name: str,
        *,
        type: Literal["text", "chat"],  # the API field name
        template: str | Sequence[Mapping[str, str]],
        config: Mapping[str, Any] | None = None,
        labels: Sequence[str] = (),
        commit_message: str | None = None,
        description: str | None = None,
        tags: Sequence[str] = (),
    ) -> Prompt:
        """Create a prompt with its first version. Sent once; never retried."""
        body: dict[str, Any] = {
            "name": name,
            "type": type,
            "template": _wire_template(template),
        }
        if config is not None:
            body["config"] = dict(config)
        if labels:
            body["labels"] = list(labels)
        if commit_message is not None:
            body["commit_message"] = commit_message
        if description is not None:
            body["description"] = description
        if tags:
            body["tags"] = list(tags)
        template_path = "/prompts"
        raw = self._client._request("POST", template_path, template_path, body=body, retry=False)
        return _expect_prompt("POST", template_path, raw)

    def create_version(
        self,
        name: str,
        *,
        template: str | Sequence[Mapping[str, str]],
        config: Mapping[str, Any] | None = None,
        labels: Sequence[str] = (),
        commit_message: str | None = None,
    ) -> Prompt:
        """Add a version to a prompt. Sent once; never retried."""
        body: dict[str, Any] = {"template": _wire_template(template)}
        if config is not None:
            body["config"] = dict(config)
        if labels:
            body["labels"] = list(labels)
        if commit_message is not None:
            body["commit_message"] = commit_message
        template_path = "/prompts/{prompt}/versions"
        raw = self._client._request(
            "POST",
            template_path,
            f"/prompts/{_segment(name)}/versions",
            body=body,
            retry=False,
        )
        return _expect_prompt("POST", template_path, raw)

    def set_label(self, name: str, label: str, version: int) -> None:
        """Move ``label`` to ``version``. Sent once; never retried."""
        self._client._request(
            "PUT",
            "/prompts/{prompt}/labels/{label}",
            f"/prompts/{_segment(name)}/labels/{_segment(label)}",
            body={"version": version},
            retry=False,
        )


def _wire_template(template: str | Sequence[Mapping[str, str]]) -> str | list[dict[str, str]]:
    if isinstance(template, str):
        return template
    return [dict(entry) for entry in template]
