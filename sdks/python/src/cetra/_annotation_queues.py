"""Annotation queues over the REST client (spec 10 § Datasets and experiments, D-AQ-8).

``client.annotation_queues`` creates, reads, lists, updates and deletes queues, and
adds traces, spans or sessions to them. The reviewer actions are web only (D-AQ-11).
"""

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, Literal

from cetra._client import _segment

if TYPE_CHECKING:
    from cetra._client import Client

_PAGE_LIMIT = 1000
# Targets per add request (D-AQ-8: 1 to 1,000 per call).
_BATCH_LIMIT = 1000
_TARGETS = frozenset({"trace", "span", "session"})
_ONE_KIND = "give exactly one of trace_ids, spans or session_ids"

QueueTarget = Literal["trace", "span", "session"]
QueueItemStatus = Literal["pending", "completed", "skipped"]


@dataclass(frozen=True, slots=True)
class QueueCounts:
    """Item counts of a queue, by status."""

    pending: int
    completed: int
    skipped: int


@dataclass(frozen=True, slots=True)
class AnnotationQueue:
    """An annotation queue with its counts."""

    id: str
    name: str
    description: str | None
    instructions: str | None
    target: QueueTarget
    definition_ids: tuple[str, ...]
    version: int
    created_by: str | None
    created_at: str  # RFC 3339
    updated_at: str  # RFC 3339
    counts: QueueCounts


@dataclass(frozen=True, slots=True)
class AnnotationQueueItem:
    """One target of a queue."""

    id: str
    queue_id: str
    trace_id: str | None
    span_id: str | None
    session_id: str | None
    target_time: str  # RFC 3339
    status: QueueItemStatus
    skip_reason: str | None
    leased_by_user_id: str | None
    lease_expires_at: str | None  # RFC 3339
    done_by_user_id: str | None
    done_at: str | None  # RFC 3339
    added_by: Literal["user", "pat", "key"]
    added_by_id: str
    created_at: str  # RFC 3339


@dataclass(frozen=True, slots=True)
class SkippedQueueItem:
    """A target that was not added, and why. ``index`` is its position in the caller's list."""

    index: int
    message: str


@dataclass(frozen=True, slots=True)
class AddedQueueItems:
    """The answer of ``add_items``: targets added, targets already in the queue, and skips."""

    added: int
    existing: int
    skipped: tuple[SkippedQueueItem, ...]


class AnnotationQueues:
    """``client.annotation_queues``: annotation queue calls over the REST client."""

    def __init__(self, client: "Client") -> None:
        self._client = client

    def get(self, queue_id: str) -> AnnotationQueue:
        """One queue with its counts."""
        raw = self._client._request(
            "GET", "/annotation-queues/{queue}", f"/annotation-queues/{_segment(queue_id)}"
        )
        return _to_queue(raw)

    def create(
        self,
        name: str,
        *,
        target: QueueTarget,
        definition_ids: Sequence[str],
        description: str | None = None,
        instructions: str | None = None,
    ) -> AnnotationQueue:
        """Create a queue at version 1. Sent once; never retried."""
        if target not in _TARGETS:
            raise ValueError("target must be 'trace', 'span' or 'session'")
        ids = _definition_ids(definition_ids)
        body: dict[str, Any] = {"name": name, "target": target, "definition_ids": ids}
        if description is not None:
            body["description"] = description
        if instructions is not None:
            body["instructions"] = instructions
        raw = self._client._request("POST", "/annotation-queues", "/annotation-queues", body=body)
        return _to_queue(raw)

    def update(
        self,
        queue_id: str,
        *,
        version: int,
        name: str | None = None,
        description: str | None = None,
        instructions: str | None = None,
        definition_ids: Sequence[str] | None = None,
    ) -> AnnotationQueue:
        """Change the fields given. ``version`` is the version read; a stale one is a 409.

        ``""`` clears ``description`` or ``instructions``. Sent once; never retried.
        """
        body: dict[str, Any] = {"version": version}
        if name is not None:
            body["name"] = name
        if description is not None:
            body["description"] = description
        if instructions is not None:
            body["instructions"] = instructions
        if definition_ids is not None:
            body["definition_ids"] = _definition_ids(definition_ids)
        raw = self._client._request(
            "PATCH",
            "/annotation-queues/{queue}",
            f"/annotation-queues/{_segment(queue_id)}",
            body=body,
        )
        return _to_queue(raw)

    def delete(self, queue_id: str) -> None:
        """Delete a queue and its items. The score definitions and scores stay."""
        self._client._request(
            "DELETE", "/annotation-queues/{queue}", f"/annotation-queues/{_segment(queue_id)}"
        )

    def add_items(
        self,
        queue_id: str,
        *,
        trace_ids: Sequence[str] | None = None,
        spans: Sequence[tuple[str, str]] | None = None,
        session_ids: Sequence[str] | None = None,
    ) -> AddedQueueItems:
        """Add traces, spans or sessions: exactly one of the three keyword arguments.

        Batches of 1,000 are sent in order; the server skips a target it cannot add and
        says why. ``skipped[].index`` is the position in the list the caller passed.
        A target already in the queue counts as ``existing`` and keeps its state.
        Every check runs before the first request. Retried, since adding is idempotent.
        """
        rows = _target_rows(trace_ids=trace_ids, spans=spans, session_ids=session_ids)
        if not rows:
            return AddedQueueItems(added=0, existing=0, skipped=())
        path = f"/annotation-queues/{_segment(queue_id)}/items"
        added = 0
        existing = 0
        skipped: list[SkippedQueueItem] = []
        for start in range(0, len(rows), _BATCH_LIMIT):
            raw = self._client._request(
                "POST",
                "/annotation-queues/{queue}/items",
                path,
                body={"items": rows[start : start + _BATCH_LIMIT]},
                retry=True,
            )
            added += raw["added"]
            existing += raw["existing"]
            skipped.extend(
                SkippedQueueItem(index=entry["index"] + start, message=entry["message"])
                for entry in raw["skipped"]
            )
        return AddedQueueItems(added=added, existing=existing, skipped=tuple(skipped))

    def items(
        self, queue_id: str, *, status: QueueItemStatus | None = None
    ) -> list[AnnotationQueueItem]:
        """Every item of a queue, or only those with ``status``, page by page."""
        path = f"/annotation-queues/{_segment(queue_id)}/items"
        found: list[AnnotationQueueItem] = []
        cursor: str | None = None
        while True:
            params: dict[str, str | int] = {}
            if status is not None:
                params["status"] = status
            params["limit"] = _PAGE_LIMIT
            if cursor is not None:
                params["cursor"] = cursor
            page = self._client._request(
                "GET", "/annotation-queues/{queue}/items", path, params=params
            )
            found.extend(_to_item(raw) for raw in page["data"])
            cursor = page.get("next_cursor")
            if cursor is None:
                return found

    def list(self) -> list[AnnotationQueue]:
        """Every queue of the project, with its counts, page by page."""
        queues: list[AnnotationQueue] = []
        cursor: str | None = None
        while True:
            params: dict[str, str | int] = {"limit": _PAGE_LIMIT}
            if cursor is not None:
                params["cursor"] = cursor
            page = self._client._request(
                "GET", "/annotation-queues", "/annotation-queues", params=params
            )
            queues.extend(_to_queue(raw) for raw in page["data"])
            cursor = page.get("next_cursor")
            if cursor is None:
                return queues


def _definition_ids(values: Sequence[str]) -> list[str]:
    """The score definition ids of a queue, in review order; at least one."""
    if isinstance(values, str):
        raise TypeError("definition_ids must be a sequence of ids, not a string")
    ids = list(values)
    if not ids:
        raise ValueError("definition_ids: give at least one score definition")
    return ids


def _target_rows(
    *,
    trace_ids: Sequence[str] | None,
    spans: Sequence[tuple[str, str]] | None,
    session_ids: Sequence[str] | None,
) -> list[dict[str, str]]:
    """The JSON items of one add call. Errors name the argument or the index, never a value."""
    if trace_ids is not None:
        if spans is not None or session_ids is not None:
            raise ValueError(_ONE_KIND)
        if isinstance(trace_ids, str):
            raise TypeError("trace_ids must be a sequence of ids, not a string")
        return [{"trace_id": trace_id} for trace_id in trace_ids]
    if session_ids is not None:
        if spans is not None:
            raise ValueError(_ONE_KIND)
        if isinstance(session_ids, str):
            raise TypeError("session_ids must be a sequence of ids, not a string")
        return [{"session_id": session_id} for session_id in session_ids]
    if spans is None:
        raise ValueError(_ONE_KIND)
    if isinstance(spans, str):
        raise TypeError("spans must be a sequence of (trace_id, span_id) pairs, not a string")
    return [_span_row(index, pair) for index, pair in enumerate(spans)]


def _span_row(index: int, pair: object) -> dict[str, str]:
    if (
        not isinstance(pair, (tuple, list))
        or len(pair) != 2
        or not all(isinstance(part, str) for part in pair)
    ):
        raise TypeError(f"spans[{index}]: must be a (trace_id, span_id) pair of strings")
    trace_id, span_id = pair
    return {"trace_id": trace_id, "span_id": span_id}


def _to_queue(raw: Mapping[str, Any]) -> AnnotationQueue:
    counts = raw["counts"]
    return AnnotationQueue(
        id=raw["id"],
        name=raw["name"],
        description=raw.get("description"),
        instructions=raw.get("instructions"),
        target=raw["target"],
        definition_ids=tuple(raw["definition_ids"]),
        version=raw["version"],
        created_by=raw.get("created_by"),
        created_at=raw["created_at"],
        updated_at=raw["updated_at"],
        counts=QueueCounts(
            pending=counts["pending"],
            completed=counts["completed"],
            skipped=counts["skipped"],
        ),
    )


def _to_item(raw: Mapping[str, Any]) -> AnnotationQueueItem:
    return AnnotationQueueItem(
        id=raw["id"],
        queue_id=raw["queue_id"],
        trace_id=raw.get("trace_id"),
        span_id=raw.get("span_id"),
        session_id=raw.get("session_id"),
        target_time=raw["target_time"],
        status=raw["status"],
        skip_reason=raw.get("skip_reason"),
        leased_by_user_id=raw.get("leased_by_user_id"),
        lease_expires_at=raw.get("lease_expires_at"),
        done_by_user_id=raw.get("done_by_user_id"),
        done_at=raw.get("done_at"),
        added_by=raw["added_by"],
        added_by_id=raw["added_by_id"],
        created_at=raw["created_at"],
    )
