"""Tests for ``client.datasets`` and ``Dataset.add_items`` against a fake server (plan J4.1)."""

import json
from collections.abc import Callable
from typing import Any, Protocol

import pytest

from llmobs import ApiError, Client, Dataset

TEST_API_KEY = "sk-llmobs-test"
DATASET_ID = "0190a000-0000-7000-8000-000000000001"
PROJECT = "/api/v1/projects/current"
JSON_HEADERS = {"Content-Type": "application/json"}
PROBLEM_HEADERS = {"Content-Type": "application/problem+json"}

Response = tuple[int, dict[str, str], bytes]


class Recorder(Protocol):
    """The surface of the ``http_recorder`` fixture used by the tests."""

    url: str
    requests: list[tuple[str, str, dict[str, str], bytes]]

    def queue_response(self, status: int, headers: dict[str, str], body: bytes) -> None: ...

    def respond_with(self, handler: Callable[[str, str, bytes], Response]) -> None: ...


@pytest.fixture
def client(http_recorder: Recorder) -> Client:
    return Client(api_key=TEST_API_KEY, base_url=http_recorder.url)


def dataset_json(name: str = "faq", current_version: int = 0) -> dict[str, Any]:
    return {
        "id": DATASET_ID,
        "name": name,
        "description": None,
        "current_version": current_version,
        "item_count": 0,
        "experiment_count": 0,
        "created_at": 0,
        "updated_at": 0,
    }


def item_json(item_id: str) -> dict[str, Any]:
    return {
        "id": item_id,
        "revision_id": f"rev-{item_id}",
        "version": 1,
        "input": {"question": item_id},
        "expected_output": None,
        "metadata": {},
        "source_trace_id": None,
        "source_span_id": None,
        "created_at": 0,
    }


def encode(document: object) -> bytes:
    return json.dumps(document).encode()


def test_create_posts_the_fields_and_returns_version_0(
    client: Client, http_recorder: Recorder
) -> None:
    http_recorder.queue_response(201, JSON_HEADERS, encode(dataset_json()))
    dataset = client.datasets.create("faq", description="FAQ", input_schema={"type": "object"})
    assert (dataset.id, dataset.name, dataset.version, dataset.items) == (
        DATASET_ID,
        "faq",
        0,
        (),
    )
    _, path, _, body = http_recorder.requests[0]
    assert path == f"{PROJECT}/datasets"
    assert json.loads(body) == {
        "name": "faq",
        "description": "FAQ",
        "input_schema": {"type": "object"},
    }


def test_get_pages_items_and_pins_the_version(client: Client, http_recorder: Recorder) -> None:
    def answer(method: str, path: str, body: bytes) -> Response:
        if "/items" not in path:
            return 200, JSON_HEADERS, encode(dataset_json(current_version=3))
        if "cursor=c1" in path:
            return 200, JSON_HEADERS, encode({"data": [item_json("i3")], "next_cursor": None})
        page = {"data": [item_json("i1"), item_json("i2")], "next_cursor": "c1"}
        return 200, JSON_HEADERS, encode(page)

    http_recorder.respond_with(answer)
    dataset = client.datasets.get("faq")
    assert dataset.version == 3
    assert [item.id for item in dataset.items] == ["i1", "i2", "i3"]
    assert dataset.items[0].input == {"question": "i1"}
    paths = [path for _, path, _, _ in http_recorder.requests]
    assert paths == [
        f"{PROJECT}/datasets/faq",
        f"{PROJECT}/datasets/{DATASET_ID}/items?version=3&limit=1000",
        f"{PROJECT}/datasets/{DATASET_ID}/items?version=3&limit=1000&cursor=c1",
    ]


def test_get_encodes_a_name(client: Client, http_recorder: Recorder) -> None:
    def answer(method: str, path: str, body: bytes) -> Response:
        if "/items" in path:
            return 200, JSON_HEADERS, encode({"data": [], "next_cursor": None})
        return 200, JSON_HEADERS, encode(dataset_json(name="faq v1/x"))

    http_recorder.respond_with(answer)
    client.datasets.get("faq v1/x")
    _, path, _, _ = http_recorder.requests[0]
    assert path == f"{PROJECT}/datasets/faq%20v1%2Fx"


def test_add_items_batches_by_1000(client: Client, http_recorder: Recorder) -> None:
    posted: list[int] = []

    def answer(method: str, path: str, body: bytes) -> Response:
        if method == "POST":
            posted.append(len(json.loads(body)["items"]))
            return 201, JSON_HEADERS, encode({"version": 1, "item_ids": []})
        if "/items" in path:
            return 200, JSON_HEADERS, encode({"data": [], "next_cursor": None})
        return 200, JSON_HEADERS, encode(dataset_json(current_version=1))

    http_recorder.respond_with(answer)
    dataset = Dataset(
        id=DATASET_ID, name="faq", description=None, version=0, items=(), _client=client
    )
    result = dataset.add_items({"input": n} for n in range(2500))
    assert posted == [1000, 1000, 500]
    # The batches are followed by one re-read: the dataset, then its items.
    assert [method for method, _, _, _ in http_recorder.requests[-2:]] == ["GET", "GET"]
    assert result.version == 1


def test_add_items_rejects_unknown_keys(client: Client, http_recorder: Recorder) -> None:
    dataset = Dataset(
        id=DATASET_ID, name="faq", description=None, version=0, items=(), _client=client
    )
    with pytest.raises(ValueError, match="unknown key"):
        dataset.add_items([{"input": 1, "answer": 2}])
    assert http_recorder.requests == []


def test_list_pages(client: Client, http_recorder: Recorder) -> None:
    def answer(method: str, path: str, body: bytes) -> Response:
        if "cursor=c1" in path:
            page: dict[str, Any] = {
                "data": [dataset_json(name="returns") | {"item_count": 4}],
                "next_cursor": None,
            }
        else:
            page = {"data": [dataset_json(name="faq") | {"item_count": 2}], "next_cursor": "c1"}
        return 200, JSON_HEADERS, encode(page)

    http_recorder.respond_with(answer)
    infos = client.datasets.list()
    assert [(info.name, info.item_count) for info in infos] == [("faq", 2), ("returns", 4)]
    paths = [path for _, path, _, _ in http_recorder.requests]
    assert paths == [f"{PROJECT}/datasets?limit=1000", f"{PROJECT}/datasets?limit=1000&cursor=c1"]


def test_delete_sends_delete(client: Client, http_recorder: Recorder) -> None:
    http_recorder.queue_response(204, {}, b"")
    client.datasets.delete("faq")
    method, path, _, _ = http_recorder.requests[0]
    assert (method, path) == ("DELETE", f"{PROJECT}/datasets/faq")


def test_get_404_raises(client: Client, http_recorder: Recorder) -> None:
    problem = {"code": "not_found", "detail": "no such dataset", "errors": []}
    http_recorder.queue_response(404, PROBLEM_HEADERS, encode(problem))
    with pytest.raises(ApiError) as excinfo:
        client.datasets.get("missing")
    assert excinfo.value.status == 404
