"""Tests for ``client.annotation_queues`` against a fake server (spec 10, D-AQ-8).

Replies are the committed fixtures under ``fixtures/api/static``, so the SDK and the
server agree on the shape. Batch replies that need specific counts are inline.
"""

import json
from collections.abc import Callable
from pathlib import Path
from typing import Any, Protocol

import pytest

from cetra import (
    AddedQueueItems,
    AnnotationQueue,
    ApiError,
    Client,
    QueueCounts,
    SkippedQueueItem,
)

TEST_API_KEY = "sk-cetra-test"
QUEUE_ID = "01900000-0000-7000-8000-000000000a01"
OTHER_QUEUE_ID = "01900000-0000-7000-8000-000000000a02"
DEFINITION_ID = "01900000-0000-7000-8000-000000000600"
PROJECT = "/api/v1/projects/current"
QUEUE_PATH = f"{PROJECT}/annotation-queues"
ITEMS_PATH = f"{QUEUE_PATH}/{QUEUE_ID}/items"
JSON_HEADERS = {"Content-Type": "application/json"}
PROBLEM_HEADERS = {"Content-Type": "application/problem+json"}
STATIC = Path(__file__).resolve().parents[3] / "fixtures" / "api" / "static"

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


def fixture_bytes(name: str) -> bytes:
    return (STATIC / name).read_bytes()


def fixture_json(name: str) -> dict[str, Any]:
    document: dict[str, Any] = json.loads(fixture_bytes(name))
    return document


def encode(document: object) -> bytes:
    return json.dumps(document).encode()


def test_create_sends_the_body_and_maps_the_queue(
    client: Client, http_recorder: Recorder
) -> None:
    http_recorder.queue_response(201, JSON_HEADERS, fixture_bytes("annotation-queue.json"))
    queue = client.annotation_queues.create(
        "Relevance review", target="trace", definition_ids=[DEFINITION_ID]
    )
    method, path, _, body = http_recorder.requests[0]
    assert (method, path) == ("POST", QUEUE_PATH)
    assert json.loads(body) == {
        "name": "Relevance review",
        "target": "trace",
        "definition_ids": [DEFINITION_ID],
    }
    assert queue == AnnotationQueue(
        id=QUEUE_ID,
        name="Relevance review",
        description="Support bot traces that the relevance judge scored low.",
        instructions=(
            "Score each answer against the question it answers. "
            "Skip the traces that have no question."
        ),
        target="trace",
        definition_ids=(DEFINITION_ID,),
        version=2,
        created_by="01900000-0000-7000-8000-000000000100",
        created_at="2026-01-01T09:00:00.000000000Z",
        updated_at="2026-01-01T10:30:00.000000000Z",
        counts=QueueCounts(pending=12, completed=4, skipped=1),
    )
    assert isinstance(queue.definition_ids, tuple)


def test_create_sends_the_optional_fields_only_when_given(
    client: Client, http_recorder: Recorder
) -> None:
    http_recorder.queue_response(201, JSON_HEADERS, fixture_bytes("annotation-queue.json"))
    client.annotation_queues.create(
        "Relevance review",
        target="trace",
        definition_ids=[DEFINITION_ID],
        description="Low scores",
    )
    _, _, _, body = http_recorder.requests[0]
    assert json.loads(body) == {
        "name": "Relevance review",
        "target": "trace",
        "definition_ids": [DEFINITION_ID],
        "description": "Low scores",
    }


def test_create_checks_its_arguments_before_any_request(
    client: Client, http_recorder: Recorder
) -> None:
    bad_target: Any = "event"
    with pytest.raises(ValueError, match="target"):
        client.annotation_queues.create("x", target=bad_target, definition_ids=[DEFINITION_ID])
    with pytest.raises(ValueError, match="definition_ids"):
        client.annotation_queues.create("x", target="trace", definition_ids=[])
    with pytest.raises(TypeError, match="not a string"):
        client.annotation_queues.create("x", target="trace", definition_ids=DEFINITION_ID)
    assert http_recorder.requests == []


def test_list_follows_cursors(client: Client, http_recorder: Recorder) -> None:
    first = fixture_json("annotation-queues.json") | {"next_cursor": "c1"}
    second_queue = fixture_json("annotation-queue.json") | {
        "id": OTHER_QUEUE_ID,
        "name": "Returns review",
    }
    second = {"data": [second_queue], "next_cursor": None}

    def answer(method: str, path: str, body: bytes) -> Response:
        if "cursor=c1" in path:
            return 200, JSON_HEADERS, encode(second)
        return 200, JSON_HEADERS, encode(first)

    http_recorder.respond_with(answer)
    queues = client.annotation_queues.list()
    assert [(queue.id, queue.name) for queue in queues] == [
        (QUEUE_ID, "Relevance review"),
        (OTHER_QUEUE_ID, "Returns review"),
    ]
    paths = [path for _, path, _, _ in http_recorder.requests]
    assert paths == [f"{QUEUE_PATH}?limit=1000", f"{QUEUE_PATH}?limit=1000&cursor=c1"]


def test_get_update_delete(client: Client, http_recorder: Recorder) -> None:
    queue_reply = fixture_bytes("annotation-queue.json")
    http_recorder.queue_response(200, JSON_HEADERS, queue_reply)
    http_recorder.queue_response(200, JSON_HEADERS, queue_reply)
    http_recorder.queue_response(204, {}, b"")

    assert client.annotation_queues.get(QUEUE_ID).id == QUEUE_ID
    client.annotation_queues.update(QUEUE_ID, version=2, name="Renamed", description="")
    client.annotation_queues.delete(QUEUE_ID)  # a 204 answer: no body, no error

    get_request, update_request, delete_request = http_recorder.requests
    assert (get_request[0], get_request[1]) == ("GET", f"{QUEUE_PATH}/{QUEUE_ID}")
    assert (update_request[0], update_request[1]) == ("PATCH", f"{QUEUE_PATH}/{QUEUE_ID}")
    assert json.loads(update_request[3]) == {
        "version": 2,
        "name": "Renamed",
        "description": "",
    }
    assert (delete_request[0], delete_request[1]) == ("DELETE", f"{QUEUE_PATH}/{QUEUE_ID}")


def test_add_items_shapes(client: Client, http_recorder: Recorder) -> None:
    added_reply = fixture_bytes("annotation-queue-items-added.json")
    for _ in range(3):
        http_recorder.queue_response(200, JSON_HEADERS, added_reply)
    trace_id = "4bf92f3577b34da6a3ce929d0e0e4736"
    span_id = "00f067aa0ba902b7"
    session_id = "session-7"

    client.annotation_queues.add_items(QUEUE_ID, trace_ids=[trace_id])
    client.annotation_queues.add_items(QUEUE_ID, spans=[(trace_id, span_id)])
    client.annotation_queues.add_items(QUEUE_ID, session_ids=[session_id])

    bodies = [json.loads(body) for _, _, _, body in http_recorder.requests]
    assert [method for method, _, _, _ in http_recorder.requests] == ["POST"] * 3
    assert [path for _, path, _, _ in http_recorder.requests] == [ITEMS_PATH] * 3
    assert bodies == [
        {"items": [{"trace_id": trace_id}]},
        {"items": [{"trace_id": trace_id, "span_id": span_id}]},
        {"items": [{"session_id": session_id}]},
    ]


def test_add_items_needs_exactly_one_kind(client: Client, http_recorder: Recorder) -> None:
    with pytest.raises(ValueError, match="exactly one"):
        client.annotation_queues.add_items(QUEUE_ID)
    with pytest.raises(ValueError, match="exactly one"):
        client.annotation_queues.add_items(QUEUE_ID, trace_ids=["a"], session_ids=["b"])
    with pytest.raises(ValueError, match="exactly one"):
        client.annotation_queues.add_items(
            QUEUE_ID, trace_ids=["a"], spans=[("a", "b")], session_ids=["c"]
        )
    assert http_recorder.requests == []


def test_add_items_checks_types_before_any_request(
    client: Client, http_recorder: Recorder
) -> None:
    with pytest.raises(TypeError, match="trace_ids must be a sequence"):
        client.annotation_queues.add_items(QUEUE_ID, trace_ids="4bf92f3577b34da6a3ce929d0e0e4736")
    with pytest.raises(TypeError, match="session_ids must be a sequence"):
        client.annotation_queues.add_items(QUEUE_ID, session_ids="session-7")
    bad_pairs: Any = [("4bf92f3577b34da6a3ce929d0e0e4736", "00f067aa0ba902b7"), ("t", 5)]
    with pytest.raises(TypeError, match=r"spans\[1\]"):
        client.annotation_queues.add_items(QUEUE_ID, spans=bad_pairs)
    assert http_recorder.requests == []


def test_add_items_batches_and_offsets_skipped(client: Client, http_recorder: Recorder) -> None:
    trace_ids = [f"{number:032x}" for number in range(1, 1501)]

    def answer(method: str, path: str, body: bytes) -> Response:
        if len(json.loads(body)["items"]) == 1000:
            return 200, JSON_HEADERS, encode({"added": 1000, "existing": 0, "skipped": []})
        skipped = [{"index": 3, "message": "unknown trace"}]
        return 200, JSON_HEADERS, encode({"added": 499, "existing": 1, "skipped": skipped})

    http_recorder.respond_with(answer)
    result = client.annotation_queues.add_items(QUEUE_ID, trace_ids=trace_ids)
    sizes = [len(json.loads(body)["items"]) for _, _, _, body in http_recorder.requests]
    assert sizes == [1000, 500]
    assert result == AddedQueueItems(
        added=1499,
        existing=1,
        skipped=(SkippedQueueItem(index=1003, message="unknown trace"),),
    )


def test_add_items_empty_sends_nothing(client: Client, http_recorder: Recorder) -> None:
    assert client.annotation_queues.add_items(QUEUE_ID, trace_ids=[]) == AddedQueueItems(
        added=0, existing=0, skipped=()
    )
    assert http_recorder.requests == []


def test_items_filters_by_status(client: Client, http_recorder: Recorder) -> None:
    raw_items = fixture_json("annotation-queue-items.json")["data"]
    http_recorder.queue_response(200, JSON_HEADERS, fixture_bytes("annotation-queue-items.json"))
    items = client.annotation_queues.items(QUEUE_ID, status="pending")
    _, path, _, _ = http_recorder.requests[0]
    assert path == f"{ITEMS_PATH}?status=pending&limit=1000"
    assert [(item.id, item.trace_id, item.status) for item in items] == [
        (raw["id"], raw.get("trace_id"), raw["status"]) for raw in raw_items
    ]


def test_errors_are_api_errors(client: Client, http_recorder: Recorder) -> None:
    plan_limit = {"code": "plan_limit", "detail": "the plan allows 1 queue", "errors": []}
    http_recorder.queue_response(403, PROBLEM_HEADERS, encode(plan_limit))
    with pytest.raises(ApiError) as refused:
        client.annotation_queues.create("x", target="trace", definition_ids=[DEFINITION_ID])
    assert (refused.value.status, refused.value.code) == (403, "plan_limit")

    stale = {"code": "stale_version", "detail": "the stored version is 3", "errors": []}
    http_recorder.queue_response(409, PROBLEM_HEADERS, encode(stale))
    with pytest.raises(ApiError) as conflict:
        client.annotation_queues.update(QUEUE_ID, version=1, name="x")
    assert conflict.value.status == 409
