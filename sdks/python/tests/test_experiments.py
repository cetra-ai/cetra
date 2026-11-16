"""Tests for ``evaluate`` and ``aevaluate`` against a fake API (plan J4.2)."""

import asyncio
import json
import logging
import threading
import time
from collections.abc import Callable
from typing import Any
from urllib.parse import parse_qs

import pytest
from conftest import TEST_API_KEY, HttpRecorder
from opentelemetry.sdk.trace import ReadableSpan
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter
from opentelemetry.trace import StatusCode

import llmobs
from llmobs import ApiError, Client, ConfigError, Dataset, Score
from llmobs import _experiments as experiments
from llmobs._datasets import DatasetItem
from llmobs._experiments import ExperimentResult, Run, _error_text
from llmobs._runtime import Runtime

Memory = tuple[InMemorySpanExporter, Callable[..., Runtime]]
Spans = Callable[[InMemorySpanExporter], list[ReadableSpan]]
Response = tuple[int, dict[str, str], bytes]

PROJECT = "/api/v1/projects/current"
EXPERIMENT_ID = "e1"
DATASET_ID = "d1"
JSON_HEADERS = {"Content-Type": "application/json"}
PROBLEM_HEADERS = {"Content-Type": "application/problem+json"}
SCORE_PAUSE = (
    "llmobs: the monthly units cap paused score uploads; experiment scores are being dropped"
)
SAMPLE_RATE = "llmobs: sample_rate is below 1; some experiment runs will have no trace and no score"


def make_items(count: int) -> list[dict[str, Any]]:
    return [
        {
            "id": f"item-{index}",
            "input": index,
            "expected_output": index * 2,
            "metadata": {},
            "version": 2,
            "source_trace_id": None,
            "source_span_id": None,
        }
        for index in range(count)
    ]


def _json(status: int, value: object) -> Response:
    return status, JSON_HEADERS, json.dumps(value).encode()


def _problem(status: int) -> Response:
    body = {"code": "refused", "detail": "fake", "errors": []}
    return status, PROBLEM_HEADERS, json.dumps(body).encode()


class FakeApi:
    """Answers the experiment routes from an in-memory item list (pages of 1,000)."""

    def __init__(self, items: list[dict[str, Any]]) -> None:
        self.items = items
        self._queued: dict[str, list[int]] = {}

    def queue(self, route: str, *statuses: int) -> None:
        """Answer the next requests of ``route`` with these statuses before the default."""
        self._queued.setdefault(route, []).extend(statuses)

    def _status(self, route: str, default: int) -> int:
        pending = self._queued.get(route)
        return pending.pop(0) if pending else default

    def _page(self, offset: int) -> tuple[list[dict[str, Any]], str | None]:
        end = offset + 1000
        cursor = str(end) if end < len(self.items) else None
        return self.items[offset:end], cursor

    def __call__(self, method: str, path: str, body: bytes) -> Response:
        route, _, query = path.partition("?")
        params = parse_qs(query)
        if method == "POST" and route == "/api/v1/scores":
            return _json(200, {"score_ids": []})
        if method == "GET" and route == f"{PROJECT}/datasets/faq":
            return _json(200, {"id": DATASET_ID, "name": "faq", "current_version": 2})
        if method == "POST" and route == f"{PROJECT}/experiments":
            status = self._status("create", 201)
            if status >= 300:
                return _problem(status)
            request = json.loads(body)
            page, cursor = self._page(0)
            experiment = {
                "id": EXPERIMENT_ID,
                "name": request["name"],
                "dataset_id": DATASET_ID,
                "dataset_version": request.get("dataset_version", 2),
                "repetitions": request["repetitions"],
                "status": "running",
            }
            return _json(201, {"experiment": experiment, "items": page, "next_cursor": cursor})
        if method == "GET" and route == f"{PROJECT}/datasets/{DATASET_ID}/items":
            status = self._status("pages", 200)
            if status >= 300:
                return _problem(status)
            page, cursor = self._page(int(params.get("cursor", ["0"])[0]))
            return _json(200, {"data": page, "next_cursor": cursor})
        if method == "POST" and route == f"{PROJECT}/experiments/{EXPERIMENT_ID}/items":
            status = self._status("items", 200)
            if status >= 300:
                return _problem(status)
            return _json(200, {"stored": len(json.loads(body)["results"]), "skipped": 0})
        if method == "POST" and route == f"{PROJECT}/experiments/{EXPERIMENT_ID}/finish":
            status = self._status("finish", 200)
            if status >= 300:
                return _problem(status)
            return _json(200, {"id": EXPERIMENT_ID, "status": "completed"})
        return _problem(404)


def sent(recorder: HttpRecorder, method: str, route: str) -> list[bytes]:
    """The bodies of the recorded requests of ``method`` whose path ends with ``route``."""
    return [
        body
        for sent_method, path, _headers, body in recorder.requests
        if sent_method == method and path.partition("?")[0].endswith(route)
    ]


def double(value: int) -> int:
    return value * 2


def exact(output: object, expected: object) -> bool:
    return output == expected


async def async_double(value: int) -> int:
    await asyncio.sleep(0)
    return value * 2


async def async_exact(output: object, expected: object) -> bool:
    return output == expected


def positional_only(output: object, /) -> bool:
    return True


@pytest.fixture
def fake(http_recorder: HttpRecorder) -> FakeApi:
    api = FakeApi(make_items(3))
    http_recorder.respond_with(api)
    return api


def test_evaluate_needs_init(http_recorder: HttpRecorder, fake: FakeApi) -> None:
    with pytest.raises(ConfigError, match=r"llmobs\.init"):
        llmobs.evaluate(dataset="faq", task=double)
    assert http_recorder.requests == []


async def _async_task(value: int) -> int:
    return value


@pytest.mark.parametrize(
    "options",
    [
        {"concurrency": 0},
        {"concurrency": 65},
        {"repetitions": 21},
        {"experiment_name": ""},
        {"scorers": [lambda output: True]},
        {"scorers": [exact, exact]},
        {"task": _async_task},
        {"scorers": [positional_only]},
    ],
)
def test_bad_arguments_raise_before_any_request(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi, options: dict[str, Any]
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    arguments: dict[str, Any] = {"dataset": "faq", "task": double, **options}
    with pytest.raises((ValueError, TypeError)):
        llmobs.evaluate(**arguments)
    assert http_recorder.requests == []


def test_create_body_and_default_name(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    llmobs.evaluate(dataset="faq", task=double, metadata={"model": "m"}, repetitions=2)
    body = json.loads(sent(http_recorder, "POST", "/experiments")[0])
    assert body["name"].startswith("faq ")
    assert body["dataset"] == "faq"
    assert body["repetitions"] == 2
    assert body["metadata"] == {"model": "m"}
    assert "dataset_version" not in body


def test_dataset_object_sends_its_id_and_version(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    client = Client(api_key=TEST_API_KEY, base_url=http_recorder.url)
    pinned = Dataset(
        id=DATASET_ID, name="faq", description=None, version=2, items=(), _client=client
    )
    llmobs.evaluate(dataset=pinned, task=double)
    body = json.loads(sent(http_recorder, "POST", "/experiments")[0])
    assert body["dataset"] == DATASET_ID
    assert body["dataset_version"] == 2
    assert body["name"].startswith("faq ")


def test_items_are_paged_with_the_pinned_version(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(1001)
    result = llmobs.evaluate(dataset="faq", task=double)
    queries = [
        parse_qs(path.partition("?")[2])
        for method, path, _h, _b in http_recorder.requests
        if method == "GET" and "/items" in path
    ]
    assert queries == [{"version": ["2"], "limit": ["1000"], "cursor": ["1000"]}]
    assert len(result.runs) == 1001


def test_each_run_is_its_own_trace_with_the_linkage(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi, spans: Spans
) -> None:
    exporter, start = memory
    start(base_url=http_recorder.url, environment="prod")
    fake.items = make_items(2)

    def traced(value: int) -> int:
        with llmobs.span("inner"):
            pass
        return value

    result = llmobs.evaluate(
        dataset="faq", task=traced, experiment_name="exp", repetitions=2, concurrency=1
    )
    finished = spans(exporter)
    roots = [span for span in finished if span.name == "exp"]
    inner = [span for span in finished if span.name == "inner"]
    assert len(roots) == 4
    assert len(inner) == 4
    assert len({span.context.trace_id for span in roots}) == 4
    for root in roots:
        assert root.parent is None
    root_ids = {root.context.span_id for root in roots}
    for child in inner:
        assert child.parent is not None
        assert child.parent.span_id in root_ids
    for span in finished:
        assert span.attributes is not None
        assert span.attributes["llmobs.experiment.id"] == EXPERIMENT_ID
        assert span.attributes["llmobs.environment"] == "experiment"
    item_ids = {span.attributes["llmobs.dataset_item.id"] for span in roots if span.attributes}
    assert item_ids == {"item-0", "item-1"}
    assert [run.repetition for run in result.runs] == [0, 1, 0, 1]


def test_caller_context_does_not_leak(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi, spans: Spans
) -> None:
    exporter, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(1)
    with llmobs.context(session_id="caller-session"), llmobs.span("caller"):
        llmobs.evaluate(dataset="faq", task=double, experiment_name="exp")
    roots = [span for span in spans(exporter) if span.name == "exp"]
    assert len(roots) == 1
    assert roots[0].parent is None
    assert roots[0].attributes is not None
    assert "llmobs.session.id" not in roots[0].attributes


def test_task_with_two_parameters_receives_the_item(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)

    def with_item(value: int, item: DatasetItem) -> str:
        return item.id

    result = llmobs.evaluate(dataset="faq", task=with_item)
    assert [run.output for run in result.runs] == ["item-0", "item-1", "item-2"]


def test_task_error_is_recorded_per_run(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi, spans: Spans
) -> None:
    exporter, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(2)
    scored: list[object] = []

    def failing(value: int) -> int:
        if value == 1:
            raise KeyError("x")
        return value

    def record(output: object) -> None:
        scored.append(output)

    result = llmobs.evaluate(dataset="faq", task=failing, scorers=[record])
    assert result.error_count == 1
    failed = [run for run in result.runs if run.error is not None]
    assert failed[0].error == "KeyError: 'x'"
    assert failed[0].output is None
    assert scored == [0]
    error_spans = [span for span in spans(exporter) if span.status.status_code == StatusCode.ERROR]
    assert len(error_spans) == 1


def test_scorer_arguments_are_chosen_by_name(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(1)
    seen: list[dict[str, object]] = []

    def uses_all(**kwargs: object) -> None:
        seen.append(kwargs)

    def with_trace(trace_id: object, item: object) -> None:
        seen.append({"trace_id": trace_id, "item": item})

    result = llmobs.evaluate(dataset="faq", task=double, scorers=[exact, uses_all, with_trace])
    assert result.runs[0].scores["exact"].value is True
    assert set(seen[0]) == {"input", "output", "expected", "metadata", "trace_id", "item"}
    assert seen[0]["output"] == 0
    assert seen[1]["trace_id"] == result.runs[0].trace_id


def test_scorer_return_types_and_score_bodies(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(2)

    def boolean_score(output: object) -> bool:
        return True

    def numeric_score(output: object) -> float:
        return 0.5

    def categorical_score(output: object) -> str:
        return "good"

    def rich_score(output: object) -> Score:
        return Score(value=1, reason="r", metadata={"k": "v"})

    def nothing(output: object) -> None:
        return None

    scorers = [boolean_score, numeric_score, categorical_score, rich_score, nothing]
    result = llmobs.evaluate(dataset="faq", task=double, scorers=scorers)
    first = result.runs[0].scores
    assert first["boolean_score"] == Score(value=True, data_type="boolean")
    assert first["numeric_score"] == Score(value=0.5, data_type="numeric")
    assert first["categorical_score"] == Score(value="good", data_type="categorical")
    assert first["rich_score"].reason == "r"
    assert "nothing" not in first

    bodies = [
        item
        for raw in sent(http_recorder, "POST", "/api/v1/scores")
        for item in json.loads(raw)["scores"]
    ]
    assert len(bodies) == 2 * 4
    for item in bodies:
        assert item["target"]["type"] == "trace"
        assert item["experiment_id"] == EXPERIMENT_ID
        assert item["source"] == "sdk"
        assert item["dataset_item_id"] in {"item-0", "item-1"}
    values = {item["name"]: item["value"] for item in bodies if item["dataset_item_id"] == "item-0"}
    assert values["boolean_score"] == {"data_type": "boolean", "value": True}
    assert values["categorical_score"] == {"data_type": "categorical", "value": {"label": "good"}}


def test_scorer_errors_are_counted_and_logged_once(
    memory: Memory,
    http_recorder: HttpRecorder,
    fake: FakeApi,
    caplog: pytest.LogCaptureFixture,
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(2)

    def bad(output: object) -> float:
        raise RuntimeError("boom")

    def not_finite(output: object) -> float:
        return float("nan")

    with caplog.at_level(logging.WARNING, logger="llmobs"):
        result = llmobs.evaluate(
            dataset="faq", task=double, scorers=[bad, not_finite], repetitions=2
        )
    assert result.scorer_errors == {"bad": 4, "not_finite": 4}
    messages = [record.getMessage() for record in caplog.records if record.name == "llmobs"]
    assert messages.count("llmobs: scorer 0 failed (RuntimeError)") == 1
    assert messages.count("llmobs: scorer 1 failed (ValueError)") == 1


def test_results_are_batched_by_500(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(1200)
    llmobs.evaluate(dataset="faq", task=double)
    posts = sent(http_recorder, "POST", "/items")
    sizes = [len(json.loads(body)["results"]) for body in posts]
    assert sizes == [500, 500, 200]
    repetitions = [row["repetition"] for body in posts for row in json.loads(body)["results"]]
    assert set(repetitions) == {0}
    methods = [
        method for method, path, _h, _b in http_recorder.requests if path.endswith("/finish")
    ]
    assert len(methods) == 1
    last_items = max(
        i for i, (_m, p, _h, _b) in enumerate(http_recorder.requests) if p.endswith("/items")
    )
    finish = next(
        i for i, (_m, p, _h, _b) in enumerate(http_recorder.requests) if p.endswith("/finish")
    )
    assert finish > last_items


def test_results_are_batched_by_bytes(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(300)

    def big(value: int) -> str:
        return "x" * 200_000

    llmobs.evaluate(dataset="faq", task=big)
    posts = sent(http_recorder, "POST", "/items")
    sizes = [len(json.loads(body)["results"]) for body in posts]
    assert sizes[0] == 167  # 32 MiB / 200,002 bytes per row
    assert sum(sizes) == 300


def test_repetitions_are_zero_based(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(1)
    llmobs.evaluate(dataset="faq", task=double, repetitions=3, concurrency=1)
    rows = [
        row for body in sent(http_recorder, "POST", "/items") for row in json.loads(body)["results"]
    ]
    assert sorted(row["repetition"] for row in rows) == [0, 1, 2]


def test_create_is_not_retried(memory: Memory, http_recorder: HttpRecorder, fake: FakeApi) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.queue("create", 503)
    with pytest.raises(ApiError) as raised:
        llmobs.evaluate(dataset="faq", task=double)
    assert raised.value.status == 503
    assert len(sent(http_recorder, "POST", "/experiments")) == 1
    assert sent(http_recorder, "POST", "/items") == []


def test_finish_is_retried(memory: Memory, http_recorder: HttpRecorder, fake: FakeApi) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.queue("finish", 503)
    sleeps: list[float] = []
    client = Client(api_key=TEST_API_KEY, base_url=http_recorder.url)
    client._sleep = sleeps.append
    llmobs.evaluate(dataset="faq", task=double, client=client)
    assert len(sent(http_recorder, "POST", "/finish")) == 2
    assert sleeps == [1.0]


def test_score_pause_warns_and_keeps_running(
    memory: Memory,
    http_recorder: HttpRecorder,
    fake: FakeApi,
    caplog: pytest.LogCaptureFixture,
) -> None:
    _, start = memory
    runtime = start(base_url=http_recorder.url)
    runtime.scores._quota.on_402("60")
    with caplog.at_level(logging.WARNING, logger="llmobs"):
        result = llmobs.evaluate(dataset="faq", task=double, scorers=[exact])
    messages = [record.getMessage() for record in caplog.records if record.name == "llmobs"]
    assert messages.count(SCORE_PAUSE) == 1
    assert len(sent(http_recorder, "POST", "/items")) == 1
    assert len(sent(http_recorder, "POST", "/finish")) == 1
    assert sent(http_recorder, "POST", "/api/v1/scores") == []
    assert result.runs[0].scores["exact"].value is True


def test_output_is_truncated(memory: Memory, http_recorder: HttpRecorder, fake: FakeApi) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(1)

    def huge(value: int) -> str:
        return "x" * 300_000

    llmobs.evaluate(dataset="faq", task=huge)
    row = json.loads(sent(http_recorder, "POST", "/items")[0])["results"][0]
    output = row["output"]
    # The first 260,000 bytes of the JSON text: the opening quote and 259,999 x.
    assert output[:260_000] == '"' + "x" * 259_999
    assert output.endswith("…[llmobs: truncated, original 300002 bytes]")


def test_upload_failure_finishes_with_an_error_and_raises(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(501)
    fake.queue("items", 200, 422)
    with pytest.raises(ApiError) as raised:
        llmobs.evaluate(dataset="faq", task=double)
    assert raised.value.status == 422
    finishes = sent(http_recorder, "POST", "/finish")
    assert len(finishes) == 1
    assert json.loads(finishes[0]) == {"error": "results upload failed"}


def test_finish_failure_raises(memory: Memory, http_recorder: HttpRecorder, fake: FakeApi) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.queue("finish", 409)
    with pytest.raises(ApiError) as raised:
        llmobs.evaluate(dataset="faq", task=double)
    assert raised.value.status == 409


def test_summary_and_assert_min() -> None:
    runs = (
        Run(
            dataset_item_id="i1",
            repetition=0,
            input=1,
            expected_output=2,
            output=2,
            error=None,
            trace_id="a" * 32,
            latency_ns=5,
            scores={"exact": Score(value=True, data_type="boolean")},
        ),
        Run(
            dataset_item_id="i2",
            repetition=0,
            input=2,
            expected_output=4,
            output=None,
            error="KeyError: 'x'",
            trace_id=None,
            latency_ns=6,
            scores={
                "exact": Score(value=False, data_type="boolean"),
                "label": Score(value="good", data_type="categorical"),
            },
        ),
    )
    result = ExperimentResult(
        experiment_id="e1",
        experiment_name="exp",
        dataset_id="d1",
        dataset_version=2,
        runs=runs,
        scorer_errors={"exact": 1},
        scorer_names=("exact", "label", "missing"),
    )
    assert result.error_count == 1
    assert result.summary() == "\n".join(
        [
            "Experiment exp (e1) on dataset d1 v2: 2 runs, 1 errors",
            "  exact: mean 0.500 (2 values)",
            "  exact: 1 errors",
            "  label: good 1",
            "  missing: no values",
        ]
    )
    result.assert_min("exact", 0.5)
    with pytest.raises(AssertionError, match=r"exact: mean 0\.500 is below 0\.9"):
        result.assert_min("exact", 0.9)
    with pytest.raises(AssertionError, match="missing: no numeric or boolean values"):
        result.assert_min("missing", 0)


def test_aevaluate_runs_async_tasks_and_scorers(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi, spans: Spans
) -> None:
    exporter, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(2)
    result = asyncio.run(
        llmobs.aevaluate(
            dataset="faq",
            task=async_double,
            scorers=[async_exact],
            experiment_name="exp",
            concurrency=2,
        )
    )
    assert len(result.runs) == 2
    assert all(run.scores["async_exact"].value is True for run in result.runs)
    assert len(sent(http_recorder, "POST", "/items")) == 1
    assert len(sent(http_recorder, "POST", "/finish")) == 1
    roots = [span for span in spans(exporter) if span.name == "exp"]
    assert len(roots) == 2
    for root in roots:
        assert root.attributes is not None
        assert root.attributes["llmobs.environment"] == "experiment"


def test_sample_rate_below_one_warns_once(
    memory: Memory,
    http_recorder: HttpRecorder,
    fake: FakeApi,
    caplog: pytest.LogCaptureFixture,
) -> None:
    _, start = memory
    start(base_url=http_recorder.url, sample_rate=0.5)
    with caplog.at_level(logging.WARNING, logger="llmobs"):
        llmobs.evaluate(dataset="faq", task=double)
        llmobs.evaluate(dataset="faq", task=double)
    messages = [record.getMessage() for record in caplog.records if record.name == "llmobs"]
    assert messages.count(SAMPLE_RATE) == 1


def test_no_payload_in_logs(
    memory: Memory,
    http_recorder: HttpRecorder,
    fake: FakeApi,
    caplog: pytest.LogCaptureFixture,
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(1)

    def leaky_task(value: int) -> int:
        raise ValueError("secret-canary")

    def leaky_scorer(output: object) -> float:
        raise ValueError("secret-canary-2")

    with caplog.at_level(logging.DEBUG, logger="llmobs"):
        result = llmobs.evaluate(dataset="faq", task=leaky_task, scorers=[leaky_scorer])
    assert result.error_count == 1
    assert all("secret-canary" not in record.getMessage() for record in caplog.records)


def test_version_0_dataset_omits_dataset_version(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    client = Client(api_key=TEST_API_KEY, base_url=http_recorder.url)
    empty = Dataset(
        id=DATASET_ID, name="faq", description=None, version=0, items=(), _client=client
    )
    llmobs.evaluate(dataset=empty, task=double)
    body = json.loads(sent(http_recorder, "POST", "/experiments")[0])
    assert "dataset_version" not in body


def test_sync_scorer_returning_an_awaitable_raises_type_error(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(1)

    def returns_coroutine(output: object) -> object:
        return async_exact(output, output)

    with pytest.raises(TypeError, match="use aevaluate"):
        llmobs.evaluate(dataset="faq", task=double, scorers=[returns_coroutine])
    finishes = [json.loads(body) for body in sent(http_recorder, "POST", "/finish")]
    assert finishes == [{"error": "TypeError"}]


def test_summary_shows_mixed_numeric_and_label_values() -> None:
    def run(item_id: str, score: Score) -> Run:
        return Run(
            dataset_item_id=item_id,
            repetition=0,
            input=1,
            expected_output=1,
            output=1,
            error=None,
            trace_id=None,
            latency_ns=1,
            scores={"mixed": score},
        )

    result = ExperimentResult(
        experiment_id="e1",
        experiment_name="exp",
        dataset_id="d1",
        dataset_version=2,
        runs=(
            run("i1", Score(value=0.5, data_type="numeric")),
            run("i2", Score(value="good", data_type="categorical")),
        ),
        scorer_errors={},
        scorer_names=("mixed",),
    )
    assert result.summary().splitlines()[1:] == [
        "  mixed: mean 0.500 (1 values)",
        "  mixed: good 1",
    ]


def test_error_text_falls_back_to_the_type_name() -> None:
    class UnprintableError(Exception):
        def __str__(self) -> str:
            raise RuntimeError("no text")

    assert _error_text(UnprintableError()) == "UnprintableError"
    assert _error_text(KeyError("x")) == "KeyError: 'x'"


def test_failing_item_page_finishes_with_an_error_and_raises(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(1001)
    fake.queue("pages", 422)
    with pytest.raises(ApiError) as raised:
        llmobs.evaluate(dataset="faq", task=double)
    assert raised.value.status == 422
    finishes = [json.loads(body) for body in sent(http_recorder, "POST", "/finish")]
    assert finishes == [{"error": "ApiError"}]
    assert sent(http_recorder, "POST", "/items") == []


def test_unexpected_worker_error_finishes_with_an_error_and_raises(
    memory: Memory,
    http_recorder: HttpRecorder,
    fake: FakeApi,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(3)

    def broken_add(self: object, row: object, size: int) -> None:
        raise RuntimeError("unexpected")

    monkeypatch.setattr(experiments._Uploader, "add", broken_add)
    with pytest.raises(RuntimeError, match="unexpected"):
        llmobs.evaluate(dataset="faq", task=double, concurrency=2)
    finishes = [json.loads(body) for body in sent(http_recorder, "POST", "/finish")]
    assert finishes == [{"error": "RuntimeError"}]


def test_aevaluate_unexpected_worker_error_finishes_and_raises(
    memory: Memory,
    http_recorder: HttpRecorder,
    fake: FakeApi,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(3)

    def broken_add(self: object, row: object, size: int) -> None:
        raise RuntimeError("unexpected")

    monkeypatch.setattr(experiments._Uploader, "add", broken_add)
    with pytest.raises(RuntimeError, match="unexpected"):
        asyncio.run(llmobs.aevaluate(dataset="faq", task=async_double, concurrency=2))
    finishes = [json.loads(body) for body in sent(http_recorder, "POST", "/finish")]
    assert finishes == [{"error": "RuntimeError"}]


def test_at_most_concurrency_runs_are_in_flight(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(8)
    lock = threading.Lock()
    state = {"now": 0, "peak": 0}

    def slow(value: int) -> int:
        with lock:
            state["now"] += 1
            state["peak"] = max(state["peak"], state["now"])
        time.sleep(0.02)
        with lock:
            state["now"] -= 1
        return value

    result = llmobs.evaluate(dataset="faq", task=slow, concurrency=2)
    assert len(result.runs) == 8
    assert 1 <= state["peak"] <= 2


def test_aevaluate_at_most_concurrency_runs_are_in_flight(
    memory: Memory, http_recorder: HttpRecorder, fake: FakeApi
) -> None:
    _, start = memory
    start(base_url=http_recorder.url)
    fake.items = make_items(8)
    state = {"now": 0, "peak": 0}

    async def slow(value: int) -> int:
        state["now"] += 1
        state["peak"] = max(state["peak"], state["now"])
        await asyncio.sleep(0.02)
        state["now"] -= 1
        return value

    result = asyncio.run(llmobs.aevaluate(dataset="faq", task=slow, concurrency=2))
    assert len(result.runs) == 8
    assert 1 <= state["peak"] <= 2
