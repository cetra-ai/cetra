"""The REST client for the llmobs API (spec 10 § Datasets and experiments, plan J4.1).

Unlike tracing and scoring, these calls raise on failure: they are explicit calls
the user makes to get data (plan M4 § Two kinds of failure).
"""

import time
import urllib.parse
from collections.abc import Callable, Mapping, Sequence
from typing import TYPE_CHECKING, Any, Final

import requests

from llmobs._config import ConfigError, resolve_config
from llmobs._retry import retry_delay

if TYPE_CHECKING:
    from llmobs._datasets import Datasets

_PROJECT_PATH: Final = "/api/v1/projects/current"
_TIMEOUT_S: Final = 30
# Retries after the first attempt: 4 attempts in all (D-J-16).
_MAX_RETRIES: Final = 3
_RETRYABLE_METHODS: Final = frozenset({"GET", "DELETE"})
_JSON_TYPES: Final = frozenset({"application/problem+json", "application/json"})


class ApiError(Exception):
    """An error answer from the llmobs API (a status outside 2xx, after retries).

    ``status`` is 0 when no answer came (``code`` is ``"network"``, ``detail`` the
    exception type name). ``code`` is the problem's ``code``; ``errors`` its field
    errors as ``{"field": ..., "message": ...}`` dicts. ``path`` in the message is
    the route template (``/datasets/{dataset}/items``), never a name or id.
    """

    status: int
    code: str | None
    detail: str | None
    errors: tuple[Mapping[str, object], ...]

    def __init__(
        self,
        *,
        method: str,
        path: str,
        status: int,
        code: str | None,
        detail: str | None,
        errors: Sequence[Mapping[str, object]],
    ) -> None:
        self.status = status
        self.code = code
        self.detail = detail
        self.errors = tuple(errors)
        message = f"{method} {path}: {status}"
        if code is not None:
            message += f" {code}"
        if detail is not None:
            message += f": {detail}"
        super().__init__(message)


class Client:
    """REST client for datasets (spec 10 § Datasets and experiments).

    Configuration resolves as for ``init()``: the argument, then ``LLMOBS_API_KEY``
    and ``LLMOBS_BASE_URL``, then the default URL. Raises ``ConfigError`` without a key.
    """

    def __init__(self, *, api_key: str | None = None, base_url: str | None = None) -> None:
        config = resolve_config(api_key=api_key, base_url=base_url, enabled=True)
        if config.api_key is None:
            raise ConfigError("api_key: required")
        self._api_key: str = config.api_key
        self._base_url: str = config.base_url
        self._session = requests.Session()
        # Tests replace this attribute to record the retry delays instead of sleeping.
        self._sleep: Callable[[float], None] = time.sleep
        # Deferred: _datasets imports this module for _segment.
        from llmobs._datasets import Datasets

        self._datasets: Datasets = Datasets(self)

    def __repr__(self) -> str:
        return f"Client(base_url={self._base_url!r})"

    @property
    def datasets(self) -> "Datasets":
        """Dataset calls: ``create``, ``get``, ``list`` and ``delete``."""
        return self._datasets

    def _request(
        self,
        method: str,
        template: str,
        path: str,
        *,
        params: Mapping[str, str | int] | None = None,
        body: object | None = None,
        retry: bool | None = None,
    ) -> Any:  # noqa: ANN401  # the decoded JSON; callers read the documented keys
        """One API call, retried only when ``retry``; the decoded JSON body, or ``None`` for 204.

        ``template`` is the route with its placeholders (used in ``ApiError``); ``path``
        is the filled path with its segments already encoded.
        """
        retrying = method in _RETRYABLE_METHODS if retry is None else retry
        url = f"{self._base_url}{_PROJECT_PATH}{path}"
        headers = {"Authorization": f"Bearer {self._api_key}"}
        if body is not None:
            headers["Content-Type"] = "application/json"
        attempt = 0
        while True:
            try:
                response = self._session.request(
                    method,
                    url,
                    params=dict(params) if params is not None else None,
                    json=body,
                    headers=headers,
                    timeout=_TIMEOUT_S,
                )
            except requests.RequestException as error:
                if retrying and attempt < _MAX_RETRIES:
                    self._sleep(retry_delay(None, attempt))
                    attempt += 1
                    continue
                raise ApiError(
                    method=method,
                    path=template,
                    status=0,
                    code="network",
                    detail=type(error).__name__,
                    errors=(),
                ) from None
            if 200 <= response.status_code < 300:
                return _decode(response, method, template)
            retryable = response.status_code == 429 or response.status_code >= 500
            if retrying and retryable and attempt < _MAX_RETRIES:
                self._sleep(retry_delay(response, attempt))
                attempt += 1
                continue
            raise _api_error(method, template, response)


def _segment(value: str) -> str:
    """A path segment: ``value`` percent-encoded with no safe characters."""
    return urllib.parse.quote(value, safe="")


def _decode(response: requests.Response, method: str, template: str) -> Any:  # noqa: ANN401  # the decoded JSON body
    """The JSON body of a 2xx answer, or ``None`` when it has none (204).

    A body that is not JSON is an ``ApiError`` with ``code=None`` (the answer's status).
    """
    if not response.content:
        return None
    try:
        return response.json()
    except ValueError:
        raise ApiError(
            method=method,
            path=template,
            status=response.status_code,
            code=None,
            detail="response is not JSON",
            errors=(),
        ) from None


def _api_error(method: str, template: str, response: requests.Response) -> ApiError:
    """The ``ApiError`` of an error answer; the problem fields when the body is JSON."""
    code: str | None = None
    detail: str | None = None
    errors: list[Mapping[str, object]] = []
    media_type = response.headers.get("Content-Type", "").split(";", 1)[0].strip().lower()
    if media_type in _JSON_TYPES:
        try:
            problem = response.json()
        except ValueError:
            problem = None
        if isinstance(problem, dict):
            raw_code = problem.get("code")
            raw_detail = problem.get("detail")
            raw_errors = problem.get("errors")
            code = raw_code if isinstance(raw_code, str) else None
            detail = raw_detail if isinstance(raw_detail, str) else None
            if isinstance(raw_errors, list):
                errors = [entry for entry in raw_errors if isinstance(entry, dict)]
    return ApiError(
        method=method,
        path=template,
        status=response.status_code,
        code=code,
        detail=detail,
        errors=errors,
    )
