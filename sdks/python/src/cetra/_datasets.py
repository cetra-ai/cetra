"""Datasets over the REST client (spec 10 § Datasets and experiments, D-J-2, D-J-3).

``client.datasets`` creates, reads, lists and deletes datasets; ``Dataset.add_items``
adds items, one new version per batch of up to 1,000.
"""

from collections.abc import Iterable, Mapping
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

from cetra._client import _segment
from cetra._serialize import to_jsonable

if TYPE_CHECKING:
    from cetra._client import Client

_PAGE_LIMIT = 1000
_ITEM_KEYS = frozenset({"input", "expected_output", "metadata"})


@dataclass(frozen=True, slots=True)
class DatasetItem:
    """One item of a dataset version (D-J-2)."""

    id: str
    input: Any
    expected_output: Any  # None when absent
    metadata: Mapping[str, Any]
    version: int  # the version that introduced this revision
    source_trace_id: str | None
    source_span_id: str | None


@dataclass(frozen=True, slots=True)
class DatasetInfo:
    """A dataset without its items, as listed."""

    id: str
    name: str
    description: str | None
    current_version: int
    item_count: int


@dataclass(frozen=True, slots=True)
class Dataset:
    """One version of a dataset with all its items."""

    id: str
    name: str
    description: str | None
    version: int  # the version these items belong to
    items: tuple[DatasetItem, ...]
    _client: "Client" = field(repr=False, compare=False)

    def add_items(self, items: Iterable[Mapping[str, Any]]) -> "Dataset":
        """Add items; each batch of up to 1,000 makes one new version (D-J-3).

        Each mapping has ``input`` (required), ``expected_output`` and ``metadata``
        (a mapping; ``None`` counts as absent). Any other key is a ``ValueError``,
        raised before any request. Returns the dataset at its latest version.

        Batches are posted in order. If a later batch fails, the earlier batches
        are already stored (each made a version), and the error is raised.
        """
        rows = [_new_item(index, entry) for index, entry in enumerate(items)]
        if not rows:
            return self
        for start in range(0, len(rows), _PAGE_LIMIT):
            self._client._request(
                "POST",
                "/datasets/{dataset}/items",
                f"/datasets/{_segment(self.id)}/items",
                body={"items": rows[start : start + _PAGE_LIMIT]},
            )
        return self._client.datasets.get(self.id)


class Datasets:
    """``client.datasets``: dataset calls over the REST client."""

    def __init__(self, client: "Client") -> None:
        self._client = client

    def create(
        self,
        name: str,
        *,
        description: str | None = None,
        input_schema: Mapping[str, Any] | None = None,
        expected_schema: Mapping[str, Any] | None = None,
    ) -> Dataset:
        """Create a dataset at version 0 with no items. Sent once; never retried."""
        body: dict[str, Any] = {"name": name}
        if description is not None:
            body["description"] = description
        if input_schema is not None:
            body["input_schema"] = to_jsonable(input_schema)
        if expected_schema is not None:
            body["expected_schema"] = to_jsonable(expected_schema)
        raw = self._client._request("POST", "/datasets", "/datasets", body=body)
        return _to_dataset(raw, version=0, items=(), client=self._client)

    def get(self, name_or_id: str, *, version: int | None = None) -> Dataset:
        """A dataset at ``version`` (default: its current version), with all its items."""
        raw = self._client._request(
            "GET", "/datasets/{dataset}", f"/datasets/{_segment(name_or_id)}"
        )
        pinned = raw["current_version"] if version is None else version
        items = self._all_items(raw["id"], pinned)
        return _to_dataset(raw, version=pinned, items=items, client=self._client)

    def delete(self, name_or_id: str) -> None:
        """Delete a dataset and its items."""
        self._client._request("DELETE", "/datasets/{dataset}", f"/datasets/{_segment(name_or_id)}")

    def list(self) -> list[DatasetInfo]:
        """Every dataset of the project, without items."""
        infos: list[DatasetInfo] = []
        cursor: str | None = None
        while True:
            params: dict[str, str | int] = {"limit": _PAGE_LIMIT}
            if cursor is not None:
                params["cursor"] = cursor
            page = self._client._request("GET", "/datasets", "/datasets", params=params)
            infos.extend(_to_info(raw) for raw in page["data"])
            cursor = page.get("next_cursor")
            if cursor is None:
                return infos

    def _all_items(self, dataset_id: str, version: int) -> tuple[DatasetItem, ...]:
        """Every item of ``version``, page by page."""
        items: list[DatasetItem] = []
        cursor: str | None = None
        path = f"/datasets/{_segment(dataset_id)}/items"
        while True:
            params: dict[str, str | int] = {"version": version, "limit": _PAGE_LIMIT}
            if cursor is not None:
                params["cursor"] = cursor
            page = self._client._request("GET", "/datasets/{dataset}/items", path, params=params)
            items.extend(_to_item(raw) for raw in page["data"])
            cursor = page.get("next_cursor")
            if cursor is None:
                return tuple(items)


def _new_item(index: int, entry: Mapping[str, Any]) -> dict[str, Any]:
    """The JSON body of one item to add; ``ValueError`` names the item, never its values."""
    if not isinstance(entry, Mapping):
        raise ValueError(f"items[{index}]: must be a mapping")
    for key in entry:
        if key not in _ITEM_KEYS:
            raise ValueError(f"items[{index}]: unknown key {key!r}")
    if "input" not in entry:
        raise ValueError(f"items[{index}]: input is required")
    item: dict[str, Any] = {"input": to_jsonable(entry["input"])}
    if entry.get("expected_output") is not None:
        item["expected_output"] = to_jsonable(entry["expected_output"])
    metadata = entry.get("metadata")
    if metadata is not None:
        if not isinstance(metadata, Mapping):
            raise ValueError(f"items[{index}]: metadata must be a mapping")
        item["metadata"] = to_jsonable(dict(metadata))
    return item


def _to_dataset(
    raw: Mapping[str, Any],
    *,
    version: int,
    items: tuple[DatasetItem, ...],
    client: "Client",
) -> Dataset:
    return Dataset(
        id=raw["id"],
        name=raw["name"],
        description=raw.get("description"),
        version=version,
        items=items,
        _client=client,
    )


def _to_info(raw: Mapping[str, Any]) -> DatasetInfo:
    return DatasetInfo(
        id=raw["id"],
        name=raw["name"],
        description=raw.get("description"),
        current_version=raw["current_version"],
        item_count=raw["item_count"],
    )


def _to_item(raw: Mapping[str, Any]) -> DatasetItem:
    return DatasetItem(
        id=raw["id"],
        input=raw["input"],
        expected_output=raw.get("expected_output"),
        metadata=raw.get("metadata") or {},
        version=raw["version"],
        source_trace_id=raw.get("source_trace_id"),
        source_span_id=raw.get("source_span_id"),
    )
