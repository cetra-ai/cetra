"""Tests for the REST client: auth, retries and error answers (plan J4.1)."""

import json
from collections.abc import Callable
from typing import Protocol

import pytest

from llmobs import ApiError, Client, ConfigError, Dataset

TEST_API_KEY = "sk-llmobs-test"
DEAD_URL = "http://127.0.0.1:9"  # nothing listens here: connection refused at once
DATASET_ID = "0190a000-0000-7000-8000-000000000001"
JSON_HEADERS = {"Content-Type": "application/json"}
PROBLEM_HEADERS = {"Content-Type": "application/problem+json"}


class Recorder(Protocol):
    """The surface of the ``http_recorder`` fixture used by the tests."""

    url: str
    requests: list[tuple[str, str, dict[str, str], bytes]]

    def queue_response(self, status: int, headers: dict[str, str], body: bytes) -> None: ...

    def respond_with(
        self, handler: Callable[[str, str, bytes], tuple[int, dict[str, str], bytes]]
    ) -> None: ...


@pytest.fixture
def api(http_recorder: Recorder) -> tuple[Client, list[float]]:
    """A client on the recorder whose retry sleeps are recorded instead of taken."""
    client = Client(api_key=TEST_API_KEY, base_url=http_recorder.url)
    sleeps: list[float] = []
    client._sleep = sleeps.append
    return client, sleeps


def test_client_needs_a_key(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("LLMOBS_API_KEY", raising=False)
    with pytest.raises(ConfigError) as excinfo:
        Client()
    assert str(excinfo.value) == "api_key: required"


def test_repr_hides_the_key(http_recorder: Recorder) -> None:
    client = Client(api_key=TEST_API_KEY, base_url=http_recorder.url)
    assert TEST_API_KEY not in repr(client)
    assert repr(client) == f"Client(base_url={http_recorder.url!r})"


def test_request_sends_the_bearer_and_json(
    api: tuple[Client, list[float]], http_recorder: Recorder
) -> None:
    client, _ = api
    http_recorder.queue_response(201, JSON_HEADERS, _dataset_body("faq"))
    client.datasets.create("faq")
    method, path, headers, body = http_recorder.requests[0]
    assert (method, path) == ("POST", "/api/v1/projects/current/datasets")
    assert headers["Authorization"] == f"Bearer {TEST_API_KEY}"
    assert headers["Content-Type"] == "application/json"
    assert json.loads(body) == {"name": "faq"}


def test_retries_5xx_then_succeeds(
    api: tuple[Client, list[float]], http_recorder: Recorder
) -> None:
    client, sleeps = api
    http_recorder.queue_response(503, {}, b"")
    http_recorder.queue_response(429, {"Retry-After": "2"}, b"")
    http_recorder.queue_response(200, JSON_HEADERS, b'{"data": [], "next_cursor": null}')
    assert client.datasets.list() == []
    assert sleeps == [1.0, 2.0]
    assert len(http_recorder.requests) == 3


def test_gives_up_after_three_retries(
    api: tuple[Client, list[float]], http_recorder: Recorder
) -> None:
    client, sleeps = api
    for _ in range(4):
        http_recorder.queue_response(500, {}, b"")
    with pytest.raises(ApiError) as excinfo:
        client.datasets.get("faq")
    assert excinfo.value.status == 500
    assert sleeps == [1.0, 2.0, 4.0]
    assert len(http_recorder.requests) == 4


def test_create_is_not_retried(api: tuple[Client, list[float]], http_recorder: Recorder) -> None:
    client, sleeps = api
    http_recorder.queue_response(503, {}, b"")
    with pytest.raises(ApiError) as excinfo:
        client.datasets.create("faq")
    assert excinfo.value.status == 503
    assert len(http_recorder.requests) == 1
    assert sleeps == []


def test_create_network_error_is_not_retried(api: tuple[Client, list[float]]) -> None:
    _, sleeps = api
    dead = Client(api_key=TEST_API_KEY, base_url=DEAD_URL)
    dead._sleep = sleeps.append
    with pytest.raises(ApiError) as excinfo:
        dead.datasets.create("faq")
    assert (excinfo.value.status, excinfo.value.code) == (0, "network")
    assert sleeps == []


def test_add_items_is_not_retried(api: tuple[Client, list[float]], http_recorder: Recorder) -> None:
    client, sleeps = api
    dataset = Dataset(
        id=DATASET_ID, name="faq", description=None, version=0, items=(), _client=client
    )
    http_recorder.queue_response(503, {}, b"")
    with pytest.raises(ApiError):
        dataset.add_items([{"input": 1}])
    assert len(http_recorder.requests) == 1
    assert sleeps == []


def test_problem_becomes_api_error(
    api: tuple[Client, list[float]], http_recorder: Recorder
) -> None:
    client, _ = api
    problem = {"code": "conflict", "detail": "a dataset with this name exists", "errors": []}
    http_recorder.queue_response(409, PROBLEM_HEADERS, json.dumps(problem).encode())
    with pytest.raises(ApiError) as excinfo:
        client.datasets.create("faq")
    error = excinfo.value
    assert error.status == 409
    assert error.code == "conflict"
    assert error.errors == ()
    assert str(error) == "POST /datasets: 409 conflict: a dataset with this name exists"


def test_402_is_an_error_not_a_pause(
    api: tuple[Client, list[float]], http_recorder: Recorder
) -> None:
    client, sleeps = api
    http_recorder.queue_response(402, {"Retry-After": "60"}, b"")
    with pytest.raises(ApiError) as excinfo:
        client.datasets.get("faq")
    assert excinfo.value.status == 402
    assert len(http_recorder.requests) == 1
    assert sleeps == []


def test_network_error_is_retried_then_raised(api: tuple[Client, list[float]]) -> None:
    _, sleeps = api
    dead = Client(api_key=TEST_API_KEY, base_url=DEAD_URL)
    dead._sleep = sleeps.append
    with pytest.raises(ApiError) as excinfo:
        dead.datasets.get("faq")
    assert (excinfo.value.status, excinfo.value.code) == (0, "network")
    assert excinfo.value.detail is not None
    assert sleeps == [1.0, 2.0, 4.0]


def _dataset_body(name: str) -> bytes:
    body = {
        "id": DATASET_ID,
        "name": name,
        "description": None,
        "current_version": 0,
        "item_count": 0,
        "experiment_count": 0,
        "created_at": 0,
        "updated_at": 0,
    }
    return json.dumps(body).encode()


def test_a_2xx_body_that_is_not_json_is_an_api_error(
    api: tuple[Client, list[float]], http_recorder: Recorder
) -> None:
    client, _ = api
    http_recorder.queue_response(200, {"Content-Type": "text/html"}, b"<html>ok</html>")
    with pytest.raises(ApiError) as excinfo:
        client.datasets.get("faq")
    error = excinfo.value
    assert (error.status, error.code, error.detail) == (200, None, "response is not JSON")
