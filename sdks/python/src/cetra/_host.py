"""The OpenAI client host stamp (D-IN-18, spec 10 § Instrumentors).

OpenInference writes no endpoint on OpenAI spans, so the server cannot tell
OpenRouter or Novita from OpenAI. While an OpenAI resource method runs, ``HOST``
holds the client's base-URL host, and ``CetraSpanProcessor`` copies it into
``server.address`` on spans that start meanwhile.
"""

import functools
import importlib
from collections.abc import Callable
from contextvars import ContextVar
from typing import Any

from cetra._log import logger

HOST: ContextVar[str | None] = ContextVar("cetra_openai_host", default=None)
# Set by instrument() and cleared by uninstrument(): the methods are wrapped once.
_installed = False

# (module, class, method, is_async)
_METHODS = (
    ("openai.resources.chat.completions", "Completions", "create", False),
    ("openai.resources.chat.completions", "AsyncCompletions", "create", True),
    ("openai.resources.responses", "Responses", "create", False),
    ("openai.resources.responses", "AsyncResponses", "create", True),
    ("openai.resources.embeddings", "Embeddings", "create", False),
    ("openai.resources.embeddings", "AsyncEmbeddings", "create", True),
)


def _host_of(resource: object) -> str | None:
    """The base-URL host of the client behind ``resource``, lowercased; never raises."""
    try:
        client: Any = getattr(resource, "_client", None)
        host = client.base_url.host
    except Exception:  # noqa: BLE001  # the stamp never raises into the app (spec 10 rule 4)
        return None
    return str(host).lower() if host else None


def _sync_wrapper(original: Callable[..., Any]) -> Callable[..., Any]:
    @functools.wraps(original)
    def wrapper(self: object, *args: object, **kwargs: object) -> object:
        token = HOST.set(_host_of(self))
        try:
            return original(self, *args, **kwargs)
        finally:
            HOST.reset(token)

    return wrapper


def _async_wrapper(original: Callable[..., Any]) -> Callable[..., Any]:
    @functools.wraps(original)
    async def wrapper(self: object, *args: object, **kwargs: object) -> object:
        token = HOST.set(_host_of(self))
        try:
            return await original(self, *args, **kwargs)
        finally:
            HOST.reset(token)

    return wrapper


class HostStamp:
    """Wraps the OpenAI resource methods; ``uninstrument`` restores them."""

    def __init__(self) -> None:
        self._wrapped: list[tuple[type, str, Any]] = []

    def instrument(self) -> None:
        global _installed
        if _installed:
            return
        _installed = True
        for module_name, class_name, name, is_async in _METHODS:
            try:
                cls = getattr(importlib.import_module(module_name), class_name)
                original = getattr(cls, name)
            except (ImportError, AttributeError):
                logger.debug("cetra: host stamp skipped %s.%s", module_name, class_name)
                continue
            wrapper = _async_wrapper(original) if is_async else _sync_wrapper(original)
            self._wrapped.append((cls, name, original))
            setattr(cls, name, wrapper)

    def uninstrument(self) -> None:
        global _installed
        for cls, name, original in reversed(self._wrapped):
            setattr(cls, name, original)
        self._wrapped.clear()
        _installed = False
