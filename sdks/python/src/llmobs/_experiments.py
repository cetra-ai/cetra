"""``evaluate`` and ``aevaluate``: experiments run on this machine (spec 10, D-J-16).

Each (item, repetition) run is one trace under an experiment scope, with the
scorers' results queued as scores and the rows uploaded in batches; see
``docs/plans/j-datasets/04-m4-sdks.md`` § ``evaluate`` flow.
"""

import asyncio
import inspect
import json
import math
import threading
import time
from collections import Counter
from collections.abc import Callable, Mapping, Sequence
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any, Final

from llmobs._client import ApiError, Client, _segment
from llmobs._config import ConfigError
from llmobs._context import experiment_scope
from llmobs._datasets import Dataset, DatasetItem, _to_item
from llmobs._log import logger, warn_once
from llmobs._runtime import Runtime, current, flush
from llmobs._scores import DataType, _value_matches, queue_experiment_score
from llmobs._serialize import to_jsonable
from llmobs._spans import Span, span

_SCORER_ARGUMENTS: Final = ("input", "output", "expected", "metadata", "trace_id", "item")
_NUMERIC_TYPES: Final = ("numeric", "boolean")
_MAX_CONCURRENCY: Final = 64
_MAX_REPETITIONS: Final = 20
_MAX_NAME: Final = 200
_MAX_ERROR: Final = 4096
_ROWS_PER_BATCH: Final = 500
_BATCH_BYTES: Final = 32 * 1024 * 1024
_OUTPUT_LIMIT: Final = 262_144
_OUTPUT_CUT: Final = 260_000
_UPLOAD_FAILED: Final = "results upload failed"

_NO_RUNTIME: Final = "evaluate needs llmobs.init() with an API key"
_SCORE_PAUSE: Final = (
    "llmobs: the monthly units cap paused score uploads; experiment scores are being dropped"
)
_SAMPLE_RATE: Final = (
    "llmobs: sample_rate is below 1; some experiment runs will have no trace and no score"
)
_SCORER_FAILED: Final = "llmobs: scorer %d failed (%s)"
_TRUNCATED: Final = "…[llmobs: truncated, original %d bytes]"


@dataclass(frozen=True, slots=True, kw_only=True)
class Score:
    """A scorer's rich return value (spec 10)."""

    value: bool | int | float | str
    reason: str | None = None
    data_type: DataType | None = None
    metadata: Mapping[str, object] | None = None


@dataclass(frozen=True, slots=True)
class Run:
    """One task run of an experiment."""

    dataset_item_id: str
    repetition: int  # 0-based: 0 .. repetitions - 1
    input: Any
    expected_output: Any
    output: Any
    error: str | None
    trace_id: str | None
    latency_ns: int
    scores: Mapping[str, Score]


@dataclass(frozen=True, slots=True)
class ExperimentResult:
    """The outcome of an experiment, built from the local runs and scores."""

    experiment_id: str
    experiment_name: str
    dataset_id: str
    dataset_version: int
    runs: tuple[Run, ...]
    scorer_errors: Mapping[str, int]
    scorer_names: tuple[str, ...] = ()  # the scorers in the order given

    @property
    def error_count(self) -> int:
        """Runs whose task raised."""
        return sum(1 for run in self.runs if run.error is not None)

    def _names(self) -> tuple[str, ...]:
        names = list(self.scorer_names)
        for run in self.runs:
            for name in run.scores:
                if name not in names:
                    names.append(name)
        return tuple(names)

    def _values(self, name: str) -> list[Score]:
        return [run.scores[name] for run in self.runs if name in run.scores]

    def summary(self) -> str:
        """The experiment's text summary: one line per scorer (spec 10)."""
        lines = [
            f"Experiment {self.experiment_name} ({self.experiment_id}) on dataset "
            f"{self.dataset_id} v{self.dataset_version}: {len(self.runs)} runs, "
            f"{self.error_count} errors"
        ]
        for name in self._names():
            values = self._values(name)
            numeric = [value for value in values if value.data_type in _NUMERIC_TYPES]
            labels = [value for value in values if value.data_type not in _NUMERIC_TYPES]
            if not values:
                lines.append(f"  {name}: no values")
            if numeric:
                lines.append(f"  {name}: mean {_mean(numeric):.3f} ({len(numeric)} values)")
            if labels:
                counts = Counter(str(value.value) for value in labels)
                ordered = sorted(counts.items(), key=lambda pair: (-pair[1], pair[0]))
                text = ", ".join(f"{label} {count}" for label, count in ordered)
                lines.append(f"  {name}: {text}")
            errors = self.scorer_errors.get(name, 0)
            if errors > 0:
                lines.append(f"  {name}: {errors} errors")
        return "\n".join(lines)

    def assert_min(self, name: str, threshold: float) -> None:
        """Raise ``AssertionError`` when a numeric or boolean scorer's mean is too low."""
        values = [value for value in self._values(name) if value.data_type in _NUMERIC_TYPES]
        if not values:
            raise AssertionError(f"{name}: no numeric or boolean values")
        mean = _mean(values)
        if mean < threshold:
            raise AssertionError(f"{name}: mean {mean:.3f} is below {threshold}")


def _mean(values: Sequence[Score]) -> float:
    return sum(float(value.value) for value in values) / len(values)


@dataclass(frozen=True, slots=True)
class _Scorer:
    name: str
    index: int
    call: Callable[..., Any]
    arguments: tuple[str, ...]
    is_async: bool


@dataclass(slots=True)
class _Plan:
    """What ``_prepare`` resolved: the experiment, its items and the scorers."""

    runtime: Runtime
    client: Client
    experiment_id: str
    experiment_name: str
    dataset_id: str
    dataset_version: int
    items: list[DatasetItem]
    repetitions: int
    concurrency: int
    task: Callable[..., Any]
    task_is_async: bool
    task_takes_item: bool
    scorers: tuple[_Scorer, ...]
    _lock: threading.Lock = field(default_factory=threading.Lock, repr=False)
    _scorer_errors: dict[str, int] = field(default_factory=dict, repr=False)
    _warned: set[int] = field(default_factory=set, repr=False)

    def scorer_failed(self, scorer: _Scorer, error_type: str) -> None:
        """Count one failed run of ``scorer``; warn once per scorer (index only)."""
        with self._lock:
            self._scorer_errors[scorer.name] = self._scorer_errors.get(scorer.name, 0) + 1
            first = scorer.index not in self._warned
            self._warned.add(scorer.index)
        if first:
            logger.warning(_SCORER_FAILED, scorer.index, error_type)

    def scorer_error_counts(self) -> dict[str, int]:
        with self._lock:
            return dict(self._scorer_errors)


class _Uploader:
    """Buffers result rows and posts them: at most 500 rows and 32 MiB of outputs per batch."""

    def __init__(self, plan: _Plan) -> None:
        self._client = plan.client
        self._path = f"/experiments/{_segment(plan.experiment_id)}/items"
        self._lock = threading.Lock()
        self._rows: list[dict[str, Any]] = []
        self._bytes = 0
        self.error: ApiError | None = None

    @property
    def failed(self) -> bool:
        return self.error is not None

    def add(self, row: dict[str, Any], size: int) -> None:
        with self._lock:
            if self.error is not None:
                return
            if self._rows and self._bytes + size > _BATCH_BYTES:
                self._post_locked()
                if self.error is not None:
                    return
            self._rows.append(row)
            self._bytes += size
            if len(self._rows) >= _ROWS_PER_BATCH:
                self._post_locked()

    def drain(self) -> None:
        with self._lock:
            if self.error is None and self._rows:
                self._post_locked()

    def _post_locked(self) -> None:
        # The lock is held across the request's retries and their sleeps, so no other
        # batch is queued meanwhile. Accepted for the MVP: one sender at a time keeps
        # the upserts ordered.
        rows, self._rows, self._bytes = self._rows, [], 0
        try:
            self._client._request(
                "POST",
                "/experiments/{experiment}/items",
                self._path,
                body={"results": rows},
                retry=True,
            )
        except ApiError as error:
            self.error = error


def _bounded(name: str, value: int, low: int, high: int) -> None:
    if isinstance(value, bool) or not isinstance(value, int) or not low <= value <= high:
        raise ValueError(f"{name}: must be an integer from {low} to {high}")


def _scorer_arguments(scorer: Callable[..., Any], index: int) -> tuple[str, ...]:
    try:
        parameters = tuple(inspect.signature(scorer).parameters.values())
    except (TypeError, ValueError):
        raise ValueError(f"scorers[{index}]: the signature cannot be read") from None
    for parameter in parameters:
        if parameter.kind is parameter.POSITIONAL_ONLY:
            raise ValueError(
                f"scorers[{index}]: positional-only parameter {parameter.name!r} is not supported"
            )
    for parameter in parameters:
        if (
            parameter.kind not in (parameter.VAR_POSITIONAL, parameter.VAR_KEYWORD)
            and parameter.name not in _SCORER_ARGUMENTS
            and parameter.default is parameter.empty
        ):
            raise ValueError(
                f"scorers[{index}]: parameter {parameter.name!r} is not one of "
                + ", ".join(_SCORER_ARGUMENTS)
            )
    if any(parameter.kind is parameter.VAR_KEYWORD for parameter in parameters):
        return _SCORER_ARGUMENTS
    return tuple(parameter.name for parameter in parameters if parameter.name in _SCORER_ARGUMENTS)


def _build_scorers(scorers: Sequence[Callable[..., Any]], *, is_async: bool) -> tuple[_Scorer, ...]:
    built: list[_Scorer] = []
    names: set[str] = set()
    for index, scorer in enumerate(scorers):
        name = getattr(scorer, "__name__", None)
        if not isinstance(name, str) or name in ("", "<lambda>"):
            raise ValueError(
                f"scorers[{index}]: a scorer needs a function name "
                "(no lambda, no functools.partial)"
            )
        if name in names:
            raise ValueError(f"scorers[{index}]: duplicate scorer name {name}")
        names.add(name)
        is_coroutine = inspect.iscoroutinefunction(scorer)
        if is_coroutine and not is_async:
            raise TypeError("use aevaluate for async tasks and scorers")
        built.append(
            _Scorer(
                name=name,
                index=index,
                call=scorer,
                arguments=_scorer_arguments(scorer, index),
                is_async=is_coroutine,
            )
        )
    return tuple(built)


def _takes_item(task: Callable[..., Any]) -> bool:
    try:
        parameters = inspect.signature(task).parameters.values()
    except (TypeError, ValueError):
        return False
    positional = [
        parameter
        for parameter in parameters
        if parameter.kind in (parameter.POSITIONAL_ONLY, parameter.POSITIONAL_OR_KEYWORD)
    ]
    return len(positional) >= 2


def _prepare(
    *,
    dataset: str | Dataset,
    task: Callable[..., Any],
    scorers: Sequence[Callable[..., Any]],
    experiment_name: str | None,
    description: str | None,
    metadata: Mapping[str, object] | None,
    dataset_version: int | None,
    concurrency: int,
    repetitions: int,
    client: Client | None,
    is_async: bool,
) -> _Plan:
    """The checks, the client, the create call and the items of the pinned version."""
    runtime = current()
    if runtime is None:
        raise ConfigError(_NO_RUNTIME)
    _bounded("concurrency", concurrency, 1, _MAX_CONCURRENCY)
    _bounded("repetitions", repetitions, 1, _MAX_REPETITIONS)
    if experiment_name is not None and not 1 <= len(experiment_name) <= _MAX_NAME:
        raise ValueError("experiment_name: must be 1 to 200 characters")
    if not isinstance(dataset, (str, Dataset)):
        raise TypeError("dataset: a name, an id or a Dataset")
    if inspect.iscoroutinefunction(task) and not is_async:
        raise TypeError("use aevaluate for async tasks and scorers")
    built_scorers = _build_scorers(scorers, is_async=is_async)
    if client is None:
        client = Client(api_key=runtime.config.api_key, base_url=runtime.config.base_url)

    dataset_name: str | None
    pinned_request: int | None
    if isinstance(dataset, Dataset):
        dataset_ref = dataset.id
        dataset_name = dataset.name
        pinned_request = dataset_version
        # A dataset at version 0 has no items yet: the server then uses its current version.
        if pinned_request is None and dataset.version != 0:
            pinned_request = dataset.version
    else:
        dataset_ref = dataset
        pinned_request = dataset_version
        dataset_name = None
        if experiment_name is None:
            raw = client._request("GET", "/datasets/{dataset}", f"/datasets/{_segment(dataset)}")
            dataset_name = raw["name"]
    name = experiment_name
    if name is None:
        stamp = datetime.now(UTC).strftime("%Y-%m-%d %H:%M:%S")
        name = f"{dataset_name} {stamp}"

    body: dict[str, Any] = {"name": name, "dataset": dataset_ref, "repetitions": repetitions}
    if pinned_request is not None:
        body["dataset_version"] = pinned_request
    if description is not None:
        body["description"] = description
    if metadata is not None:
        body["metadata"] = to_jsonable(dict(metadata))
    created = client._request("POST", "/experiments", "/experiments", body=body, retry=False)
    experiment = created["experiment"]
    try:
        items = _read_items(client, created)
        if runtime.config.sample_rate < 1:
            warn_once("evaluate-sample-rate", _SAMPLE_RATE)
        return _Plan(
            runtime=runtime,
            client=client,
            experiment_id=experiment["id"],
            experiment_name=experiment["name"],
            dataset_id=experiment["dataset_id"],
            dataset_version=experiment["dataset_version"],
            items=items,
            repetitions=repetitions,
            concurrency=concurrency,
            task=task,
            task_is_async=inspect.iscoroutinefunction(task),
            task_takes_item=_takes_item(task),
            scorers=built_scorers,
        )
    except BaseException as error:
        _finish_failed(client, experiment["id"], error)
        raise


def _read_items(client: Client, created: Mapping[str, Any]) -> list[DatasetItem]:
    """The first page of the create answer, then the rest of the pinned version (D-J-10)."""
    experiment = created["experiment"]
    items = [_to_item(raw) for raw in created["items"]]
    cursor = created.get("next_cursor")
    items_path = f"/datasets/{_segment(experiment['dataset_id'])}/items"
    while cursor is not None:
        page = client._request(
            "GET",
            "/datasets/{dataset}/items",
            items_path,
            params={"version": experiment["dataset_version"], "limit": 1000, "cursor": cursor},
        )
        items.extend(_to_item(raw) for raw in page["data"])
        cursor = page.get("next_cursor")
    return items


def _task_args(plan: _Plan, item: DatasetItem) -> tuple[object, ...]:
    return (item.input, item) if plan.task_takes_item else (item.input,)


def _scorer_kwargs(
    scorer: _Scorer, item: DatasetItem, output: object, trace_id: str | None
) -> dict[str, object]:
    values: dict[str, object] = {
        "input": item.input,
        "output": output,
        "expected": item.expected_output,
        "metadata": item.metadata,
        "trace_id": trace_id,
        "item": item,
    }
    return {name: values[name] for name in scorer.arguments}


def _default_type(value: bool | int | float | str) -> DataType:
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, str):
        return "categorical"
    return "numeric"


def _checked(score: Score) -> Score:
    """A ``Score`` with its data type resolved; ``ValueError`` when invalid."""
    value = score.value
    if not isinstance(value, (bool, int, float, str)):
        raise ValueError("scorer value: unsupported type")
    if isinstance(value, float) and not math.isfinite(value):
        raise ValueError("scorer value: not finite")
    data_type = score.data_type if score.data_type is not None else _default_type(value)
    if not _value_matches(value, data_type):
        raise ValueError("scorer value: data_type mismatch")
    if score.reason is not None and not isinstance(score.reason, str):
        raise ValueError("scorer reason: not a string")
    if score.metadata is not None and not isinstance(score.metadata, Mapping):
        raise ValueError("scorer metadata: not a mapping")
    return Score(value=value, reason=score.reason, data_type=data_type, metadata=score.metadata)


def _coerce(raw: object) -> Score | None:
    """A scorer's return as a ``Score``; ``None`` means no score; invalid raises ``ValueError``."""
    if raw is None:
        return None
    if isinstance(raw, Score):
        return _checked(raw)
    if isinstance(raw, bool):
        return Score(value=raw, data_type="boolean")
    if isinstance(raw, (int, float)):
        return _checked(Score(value=raw, data_type="numeric"))
    if isinstance(raw, str):
        return Score(value=raw, data_type="categorical")
    raise ValueError("scorer return value: unsupported type")


def _dump(value: object) -> str:
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False, allow_nan=False)


def _output_field(value: object) -> tuple[object, int] | None:
    """The row's ``output`` and its JSON byte length; ``None`` for a ``None`` output (§ Output)."""
    if value is None:
        return None
    try:
        jsonable: object = to_jsonable(value)
        text = _dump(jsonable)
    except Exception:  # noqa: BLE001  # a run never fails on an output it cannot encode
        jsonable = f"<unserializable {type(value).__name__}>"
        text = _dump(jsonable)
    data = text.encode("utf-8")
    if len(data) > _OUTPUT_LIMIT:
        cut = data[:_OUTPUT_CUT].decode("utf-8", "ignore")
        jsonable = cut + _TRUNCATED % len(data)
        text = _dump(jsonable)
    return jsonable, len(text.encode("utf-8"))


def _discard(awaitable: object) -> None:
    """Close an awaitable nobody awaits, so Python does not log a "never awaited" warning."""
    close = getattr(awaitable, "close", None)
    if callable(close):
        close()


def _error_text(error: Exception) -> str:
    """``<ExceptionTypeName>: <message>``, cut to 4,096 characters.

    The type name alone when the message cannot be converted to text.
    """
    try:
        text = f"{type(error).__name__}: {error}"
    except Exception:  # noqa: BLE001  # a message that cannot be converted must not fail the run
        return type(error).__name__
    return text[:_MAX_ERROR]


def _sampled_trace_id(handle: Span | None) -> str | None:
    if handle is None:
        return None
    context = handle._span_context()
    if context is None or not context.trace_flags.sampled:
        return None
    return handle.trace_id


def _queue(plan: _Plan, scorer: _Scorer, score: Score, item: DatasetItem, trace_id: str) -> None:
    if plan.runtime.scores.paused():
        warn_once("evaluate-score-pause", _SCORE_PAUSE)
    queue_experiment_score(
        plan.runtime,
        name=scorer.name,
        value=score.value,
        trace_id=trace_id,
        experiment_id=plan.experiment_id,
        dataset_item_id=item.id,
        reason=score.reason,
        data_type=score.data_type,
        metadata=score.metadata,
    )


def _record(
    item: DatasetItem,
    repetition: int,
    *,
    output: object,
    failure: Exception | None,
    latency_ns: int,
    trace_id: str | None,
    scores: dict[str, Score],
) -> tuple[Run, dict[str, Any], int]:
    """The local run, its upload row and the row's output byte length."""
    error = None if failure is None else _error_text(failure)
    row: dict[str, Any] = {
        "dataset_item_id": item.id,
        "repetition": repetition,
        "latency_ns": latency_ns,
    }
    size = 0
    if trace_id is not None:
        row["trace_id"] = trace_id
    if failure is None:
        field_output = _output_field(output)
        if field_output is not None:
            row["output"], size = field_output
    if error is not None:
        row["error"] = error
    run = Run(
        dataset_item_id=item.id,
        repetition=repetition,
        input=item.input,
        expected_output=item.expected_output,
        output=None if failure is not None else output,
        error=error,
        trace_id=trace_id,
        latency_ns=latency_ns,
        scores=scores,
    )
    return run, row, size


def _run_sync(plan: _Plan, item: DatasetItem, repetition: int) -> tuple[Run, dict[str, Any], int]:
    """One run of a sync task: scope, root span, task, scorers, score queue."""
    with experiment_scope(plan.experiment_id, item.id):
        output: object = None
        failure: Exception | None = None
        latency_ns = 0
        handle: Span | None = None
        try:
            with span(plan.experiment_name, kind="chain", input=item.input) as handle:
                started = time.perf_counter_ns()
                try:
                    output = plan.task(*_task_args(plan, item))
                finally:
                    latency_ns = time.perf_counter_ns() - started
                handle.set_output(output)
        except Exception as error:  # noqa: BLE001  # recorded on the run (D-J-10)
            failure = error
        trace_id = _sampled_trace_id(handle)
        scores: dict[str, Score] = {}
        if failure is None:
            for scorer in plan.scorers:
                try:
                    raw = scorer.call(**_scorer_kwargs(scorer, item, output, trace_id))
                except Exception as error:  # noqa: BLE001  # a scorer error is counted, not raised
                    plan.scorer_failed(scorer, type(error).__name__)
                    continue
                if inspect.isawaitable(raw):
                    _discard(raw)
                    raise TypeError(
                        f"scorers[{scorer.index}] returned an awaitable: "
                        "use aevaluate for async scorers"
                    )
                try:
                    score = _coerce(raw)
                except Exception as error:  # noqa: BLE001  # a scorer error is counted, not raised
                    plan.scorer_failed(scorer, type(error).__name__)
                    continue
                if score is None:
                    continue
                scores[scorer.name] = score
                if trace_id is not None:
                    _queue(plan, scorer, score, item, trace_id)
        return _record(
            item,
            repetition,
            output=output,
            failure=failure,
            latency_ns=latency_ns,
            trace_id=trace_id,
            scores=scores,
        )


async def _run_async(
    plan: _Plan, item: DatasetItem, repetition: int
) -> tuple[Run, dict[str, Any], int]:
    """One run inside ``aevaluate``: async or sync task and scorers."""
    with experiment_scope(plan.experiment_id, item.id):
        output: object = None
        failure: Exception | None = None
        latency_ns = 0
        handle: Span | None = None
        try:
            with span(plan.experiment_name, kind="chain", input=item.input) as handle:
                started = time.perf_counter_ns()
                try:
                    if plan.task_is_async:
                        output = await plan.task(*_task_args(plan, item))
                    else:
                        output = await asyncio.to_thread(plan.task, *_task_args(plan, item))
                finally:
                    latency_ns = time.perf_counter_ns() - started
                handle.set_output(output)
        except Exception as error:  # noqa: BLE001  # recorded on the run (D-J-10)
            failure = error
        trace_id = _sampled_trace_id(handle)
        scores: dict[str, Score] = {}
        if failure is None:
            for scorer in plan.scorers:
                try:
                    kwargs = _scorer_kwargs(scorer, item, output, trace_id)
                    if scorer.is_async:
                        raw = await scorer.call(**kwargs)
                    else:
                        raw = await asyncio.to_thread(scorer.call, **kwargs)
                    score = _coerce(raw)
                except Exception as error:  # noqa: BLE001  # a scorer error is counted, not raised
                    plan.scorer_failed(scorer, type(error).__name__)
                    continue
                if score is None:
                    continue
                scores[scorer.name] = score
                if trace_id is not None:
                    _queue(plan, scorer, score, item, trace_id)
        return _record(
            item,
            repetition,
            output=output,
            failure=failure,
            latency_ns=latency_ns,
            trace_id=trace_id,
            scores=scores,
        )


def _conclude(plan: _Plan, uploader: _Uploader, slots: Sequence[Run | None]) -> ExperimentResult:
    """Flush, then finish; an upload failure finishes with an error and re-raises it."""
    finish_path = f"/experiments/{_segment(plan.experiment_id)}/finish"
    if uploader.error is not None:
        flush()
        try:
            plan.client._request(
                "POST",
                "/experiments/{experiment}/finish",
                finish_path,
                body={"error": _UPLOAD_FAILED},
                retry=True,
            )
        except ApiError as error:
            logger.debug("llmobs: finish after a failed upload failed (%s)", type(error).__name__)
        raise uploader.error
    flush()
    plan.client._request(
        "POST", "/experiments/{experiment}/finish", finish_path, body={}, retry=True
    )
    return ExperimentResult(
        experiment_id=plan.experiment_id,
        experiment_name=plan.experiment_name,
        dataset_id=plan.dataset_id,
        dataset_version=plan.dataset_version,
        runs=tuple(run for run in slots if run is not None),
        scorer_errors=plan.scorer_error_counts(),
        scorer_names=tuple(scorer.name for scorer in plan.scorers),
    )


def _finish_failed(client: Client, experiment_id: str, error: BaseException) -> None:
    """Finish the experiment with the error's type name, best effort; the caller re-raises."""
    try:
        client._request(
            "POST",
            "/experiments/{experiment}/finish",
            f"/experiments/{_segment(experiment_id)}/finish",
            body={"error": type(error).__name__},
            retry=True,
        )
    except Exception as finish_error:  # noqa: BLE001  # best effort: the original error is re-raised
        logger.debug(
            "llmobs: finish after a failed experiment failed (%s)", type(finish_error).__name__
        )


def _run_pool(plan: _Plan, uploader: _Uploader, slots: list[Run | None]) -> None:
    """Every run on ``plan.concurrency`` threads. A failure stops new runs and waits."""
    stop = threading.Event()

    def work(index: int, item: DatasetItem, repetition: int) -> None:
        if stop.is_set() or uploader.failed:
            return
        try:
            run, row, size = _run_sync(plan, item, repetition)
            slots[index] = run
            uploader.add(row, size)
        except BaseException:
            stop.set()
            raise

    pool = ThreadPoolExecutor(max_workers=plan.concurrency, thread_name_prefix="llmobs-evaluate")
    try:
        futures = []
        index = 0
        for item in plan.items:
            for repetition in range(plan.repetitions):
                futures.append(pool.submit(work, index, item, repetition))
                index += 1
        for future in futures:
            future.result()
    except BaseException:
        pool.shutdown(wait=True, cancel_futures=True)
        raise
    pool.shutdown(wait=True)


async def _run_tasks(plan: _Plan, uploader: _Uploader, slots: list[Run | None]) -> None:
    """Every run as a task, at most ``plan.concurrency`` at once. A failure cancels the rest."""
    semaphore = asyncio.Semaphore(plan.concurrency)

    async def work(index: int, item: DatasetItem, repetition: int) -> None:
        async with semaphore:
            if uploader.failed:
                return
            run, row, size = await _run_async(plan, item, repetition)
            slots[index] = run
            await asyncio.to_thread(uploader.add, row, size)

    tasks: list[asyncio.Task[None]] = []
    index = 0
    for item in plan.items:
        for repetition in range(plan.repetitions):
            tasks.append(asyncio.ensure_future(work(index, item, repetition)))
            index += 1
    try:
        await asyncio.gather(*tasks)
    except BaseException:
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        raise


def evaluate(
    *,
    dataset: str | Dataset,
    task: Callable[..., Any],
    scorers: Sequence[Callable[..., Any]] = (),
    experiment_name: str | None = None,
    description: str | None = None,
    metadata: Mapping[str, object] | None = None,
    dataset_version: int | None = None,
    concurrency: int = 8,
    repetitions: int = 1,
    client: Client | None = None,
) -> ExperimentResult:
    """Run ``task`` on every item of a dataset version and score it (spec 06 § Protocol).

    Runs on this machine (D18): one trace per run, scores from ``scorers``, results
    stored on the server. Task and scorer exceptions are recorded per run. Raises
    ``ConfigError`` before ``init()``, ``ValueError``/``TypeError`` for bad
    arguments, ``ApiError`` when the API refuses a call. Any other failure after the
    experiment is created finishes it with the error's type name and re-raises it.
    """
    plan = _prepare(
        dataset=dataset,
        task=task,
        scorers=scorers,
        experiment_name=experiment_name,
        description=description,
        metadata=metadata,
        dataset_version=dataset_version,
        concurrency=concurrency,
        repetitions=repetitions,
        client=client,
        is_async=False,
    )
    uploader = _Uploader(plan)
    slots: list[Run | None] = [None] * (len(plan.items) * plan.repetitions)
    concluding = False
    try:
        _run_pool(plan, uploader, slots)
        uploader.drain()
        concluding = True
        return _conclude(plan, uploader, slots)
    except BaseException as error:
        if not concluding:
            _finish_failed(plan.client, plan.experiment_id, error)
        raise


async def aevaluate(
    *,
    dataset: str | Dataset,
    task: Callable[..., Any],
    scorers: Sequence[Callable[..., Any]] = (),
    experiment_name: str | None = None,
    description: str | None = None,
    metadata: Mapping[str, object] | None = None,
    dataset_version: int | None = None,
    concurrency: int = 8,
    repetitions: int = 1,
    client: Client | None = None,
) -> ExperimentResult:
    """``evaluate`` for async tasks and scorers; sync ones run in ``asyncio.to_thread``."""
    plan = await asyncio.to_thread(
        _prepare,
        dataset=dataset,
        task=task,
        scorers=scorers,
        experiment_name=experiment_name,
        description=description,
        metadata=metadata,
        dataset_version=dataset_version,
        concurrency=concurrency,
        repetitions=repetitions,
        client=client,
        is_async=True,
    )
    uploader = _Uploader(plan)
    slots: list[Run | None] = [None] * (len(plan.items) * plan.repetitions)
    concluding = False
    try:
        await _run_tasks(plan, uploader, slots)
        await asyncio.to_thread(uploader.drain)
        concluding = True
        return await asyncio.to_thread(_conclude, plan, uploader, slots)
    except BaseException as error:
        if not concluding:
            await asyncio.to_thread(_finish_failed, plan.client, plan.experiment_id, error)
        raise
