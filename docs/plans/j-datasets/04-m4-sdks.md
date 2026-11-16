# J · M4 — SDKs: `Client`, datasets and `evaluate`

Implementation plan for milestone M4 of track J ([`00-design.md`](00-design.md)). It has 6 sub-phases: **J4.1–J4.3** (Python) and **J4.4–J4.6** (TypeScript). **Haiku** implements each one in one go, with no context beyond this plan and the files it names. Each sub-phase ends with `just check` green and one commit. Sonnet reviews the milestone.

The two chains are independent: J4.1 → J4.2 → J4.3 run in one worktree, J4.4 → J4.5 → J4.6 in another, at the same time. They touch only `sdks/python/`, `sdks/typescript/` and one `CHANGELOG.md` line each.

If this plan and the code disagree (a name moved, a function changed), STOP and report instead of guessing.

## Prerequisites

M3 is merged on `main` (the experiments API exists). Check:

```bash
grep -n '"/api/v1/projects/{project}/experiments/{id}/finish"' crates/server/openapi.json
```

```bash
grep -n '"experiment_id"' crates/server/openapi.json | head -3
```

The first must print one line; the second at least one (the `ScoreCreate` field of D-J-12). Otherwise STOP and report.

Check the request and response shapes this plan relies on against `crates/server/openapi.json` before J4.1 and J4.4: `CreateDatasetRequest`, `Dataset`, `DatasetItem`, `AddItemsRequest`, `AddItemsResponse`, `CreateExperimentRequest`, `CreateExperimentResponse`, `ExperimentResultsRequest` (key `results`), `ExperimentResultsResponse`, `FinishExperimentRequest`, `Experiment`, `ScoreCreate`. If a field this plan names is absent or renamed, STOP and report.

Node `26.13.0` is at `$HOME/.local/share/nvm/v26.13.0/bin`, not on the non-interactive `PATH`:

```bash
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
```

## Read first (every sub-phase)

- `AGENTS.md`, all of it.
- `docs/plans/j-datasets/00-design.md`: § Decisions D-J-2, D-J-3, D-J-4, D-J-5, D-J-10, D-J-11, D-J-12, D-J-16; § API contract.
- `docs/spec/10-sdk.md` § Datasets and experiments and the TypeScript API's datasets line (as amended by the J plan: no `server_evaluators`).
- `docs/plans/f-sdk/README.md` § Decisions, § Names, § Span attributes, § Score request, § Language differences; and the "Rules for every sub-phase" of `docs/plans/f-sdk/02-python.md` (Python) or `03-typescript.md` (TypeScript). Those rules apply here unchanged: docstrings and full types, never raise into the app from tracing code, log through the SDK logger with type names only, never log values.
- This file up to § J4.1, then your own sub-phase.

## Design (both SDKs)

### Two kinds of failure

- **Tracing and scoring** keep the F rule: they never raise into the app.
- **`Client` and `evaluate`** are explicit API calls the user makes to get data, so they **raise** on failure: `ConfigError` for a missing key or runtime, `ValueError` (Python) / `TypeError` or `RangeError` (TypeScript) for bad arguments, and a new `ApiError` for an HTTP error answer. `ApiError` carries `status`, `code` (the problem's `code`, or `None`/`undefined`), `detail` and `errors` (the problem's field errors). Its message is `"<METHOD> <path template>: <status> <code>"`, plus `": <detail>"` when there is one. The path template is the route with placeholders (`/datasets/{dataset}/items`), never a dataset name. The server's `detail` never holds item content (spec 09).
- A task or scorer exception is **not** a failure of `evaluate`: it is recorded on the run (D-J-10, spec 10 "task exceptions are recorded per item and do not abort the run").

### HTTP

- Every REST call goes to `<base_url>/api/v1/projects/current/<path>` with `Authorization: Bearer <api_key>` and, with a body, `Content-Type: application/json`. `current` is the key's project (spec 09). A `{dataset}` segment is the id, or the name percent-encoded with no safe characters (`urllib.parse.quote(name, safe="")` / `encodeURIComponent`).
- Timeout 30 s per request (lists of 1,000 items and batches of 500 results can be large).
- Retries (D-J-16): only **idempotent** calls are retried: every `GET`, every `DELETE`, `POST /experiments/{id}/items` (an upsert per item and repetition, D-J-10) and `POST /experiments/{id}/finish` (a repeated finish with the same outcome answers `200` with the experiment, D-J-10). For those, a network error, a timeout, `429` and `5xx` are retried 3 times (4 attempts) with the score queue's delays: the `Retry-After` header when it is a non-negative integer (capped at 30 s), else 1 s, 2 s, 4 s. **Creates and item writes are sent once**: `POST /datasets`, `POST /datasets/{dataset}/items` and `POST /experiments` raise `ApiError` on the first failure, network errors and timeouts included, because the server may have committed (a retry would make a second dataset version or a second experiment). `request` takes a `retry: bool` argument (Python keyword-only, TypeScript an option field) that the callers set; it defaults to `True` for `GET` and `DELETE` and to `False` otherwise. Every other status raises `ApiError` at once. `402` is an `ApiError` too: the REST routes of J are not units (D-J-18), so the D-P-34 pause does not apply to them.
- `404` on `GET /datasets/{dataset}` raises `ApiError` with status 404; there is no `None` return.
- The key needs the `read` and `write` scopes (D-J-5), plus `ingest` for the spans and scores. A 403 surfaces as `ApiError`; the README says which scopes to give the key.

### `evaluate` flow (spec 06 § Protocol, as amended)

1. **Checks before any request.** A running runtime (`cetra.init()` was called with a key and not shut down), else `ConfigError("evaluate needs cetra.init() with an API key")`. `concurrency` 1–64, `repetitions` 1–20, `experiment_name` 1–200 characters when given, else `ValueError`/`RangeError` naming the argument. Scorer names: each scorer's name (below) is non-empty, not `<lambda>`, and unique, else `ValueError`/`TypeError` naming `scorers[i]`. Python `evaluate` refuses a coroutine function as task or scorer (`TypeError`: "use aevaluate for async tasks and scorers").
2. **Client.** The `client` argument, else a `Client` built from the runtime's `api_key` and `base_url`.
3. **Create.** `POST /experiments` with `{name, dataset, dataset_version?, description?, metadata?, repetitions}`. `name` defaults to `"<dataset name> <UTC time as YYYY-MM-DD HH:MM:SS>"`. `dataset` is the argument when it is a string (name or id); for a `Dataset` object it is its `id`, and `dataset_version` is the object's `version` unless the caller passed `dataset_version`. The answer is flat (D-J-10, `CreateExperimentResponse`): `{experiment, items: [DatasetItem], next_cursor}`, with `experiment` holding `dataset_id` and the pinned `dataset_version`, `items` the first page (up to 1,000) of the pinned version and `next_cursor` the cursor of the next page or `null`. There is no nested `items.data`. Sent once (no retry).
4. **Items.** Starting from the create answer's top-level `next_cursor`, while it is not null, `GET /datasets/{dataset_id}/items?version=<pinned>&limit=1000&cursor=<cursor>`. All items are held in memory (at most 20,000 runs, D-J-10).
5. **Runs.** One run per (item, repetition), items in the server's order, repetitions inner, at most `concurrency` at once. `repetition` is **0-based**: `0..repetitions` (`0` to `repetitions - 1`), which is what the server checks (D-J-10). A run:
   - enters the **experiment scope** (§ Experiment scope) for `(experiment_id, item.id)`, which also detaches from any span of the caller, so the root span is a new trace;
   - opens a span named `experiment_name`, kind `chain`, input `item.input`;
   - calls the task (§ Task), measuring wall time with a monotonic clock in nanoseconds (`time.perf_counter_ns()` / `performance.now()` × 1e6, rounded) around the call only;
   - on success sets the span output to the task output; on an exception records it on the span (the span helpers already do: Python `span.__exit__` with the exception, TypeScript `recordError`), and the run's `error` is `"<ExceptionTypeName>: <message>"` cut to 4,096 characters (Unicode code points / UTF-16 code units in TypeScript; append nothing);
   - reads the root span's `trace_id` (32 hex) from the handle: `None`/`undefined` when the span was not sampled or the runtime was a no-op;
   - after the span ended, and only when the task succeeded, runs every scorer in the order given (§ Scorers) and queues one score per scorer value **when the run has a trace id** (a run without one keeps its scores in the local result only);
   - builds the result row `{dataset_item_id, repetition, trace_id?, output?, error?, latency_ns}` (§ Output).
6. **Results.** Rows are posted with `POST /experiments/{id}/items` `{results: [...]}` in batches of at most 500 rows **and** at most 32 MiB of serialized outputs (the sum over the batch of the UTF-8 byte length of each row's JSON-encoded `output`, after § Output's truncation; D-J-10). A batch is sent as soon as 500 rows are waiting or adding the next row would pass 32 MiB, and once at the end. A single row never exceeds 32 MiB (its output is at most 256 KiB). Posts are retried (an upsert), from one sender at a time (Python: under a lock, from the worker that filled the batch; TypeScript: awaited by the run loop). An `ApiError` from a post stops new runs, waits for the runs in flight, calls finish with `{error: "results upload failed"}` (its own failure ignored and logged at debug level) and re-raises the first `ApiError`.
7. **Flush and finish.** `cetra.flush()` (spans and the score queue), then `POST /experiments/{id}/finish` with `{}`. Finish is retried like a `GET`: a retry after a lost answer gets `200` because a repeated finish with the same outcome is idempotent (D-J-10). An `ApiError` from finish is raised.
8. **Answer.** An `ExperimentResult` (§ Result) built from the local rows and scores; the server is not read back.

**402 from the score queue.** Scores go through the existing score queue, which keeps its D-P-34 pause: after a `402` it drops scores for `Retry-After` and warns once. That pause covers scores only; `evaluate` keeps running the tasks, posting results and finishing, and logs one more warning (`warn_once`, key `"evaluate-score-pause"`): `"cetra: the monthly units cap paused score uploads; experiment scores are being dropped"`, emitted when a run queues a score while the queue is paused (Python: `QuotaState` of the runtime's queue; TypeScript: the queue's pause state). The local `ExperimentResult` still holds every score.

When `sample_rate < 1` the root spans of some runs are dropped and those runs have no trace id and send no score. `evaluate` logs one warning (`warn_once`, key `"evaluate-sample-rate"`): `"cetra: sample_rate is below 1; some experiment runs will have no trace and no score"`.

### Experiment scope

Every span started inside a run must carry the experiment linkage, including spans of instrumentors (the OpenAI call inside the task), so the trace is excluded from the default lists (spec 05 tests `metadata['cetra.experiment_id']` on the trace) and its environment is `experiment`.

- The context values (`ContextValues`, the object under the SDK's one context key) gain a field `experiment`: `None`/`undefined`, or a pair `(experiment_id, dataset_item_id)` (two UUID strings). `context()` / `withContext()` copy it from the outer values unchanged (they cannot set it).
- The scope is entered by a private helper that builds the context **from the empty OTel context** (Python `opentelemetry.context.Context()`, TypeScript `ROOT_CONTEXT`) with `ContextValues(experiment=(experiment_id, item_id))` and nothing else: no session, user, tags or metadata of the caller, no active span, no baggage.
- The span processor's stamp, when `values.experiment` is set: sets `cetra.experiment.id` and `cetra.dataset_item.id` (the attribute names of spec 02 § Context fields; the server moves them into `metadata['cetra.experiment_id']` / `metadata['cetra.dataset_item_id']`), and sets `cetra.environment` to `experiment` **instead of** the configured environment. `cetra.release` stays as configured. Spans outside a scope are stamped exactly as today.

### Task

- Python: the task is called with `item.input` alone when its signature has exactly one positional parameter (or one plus `*args`/`**kwargs`), and with `(item.input, item)` when it has two or more positional parameters (`inspect.signature`; a signature that cannot be read is called with one argument). `item` is the `DatasetItem`.
- TypeScript: `task(input, item)` always; a function that declares one parameter ignores the second.

### Scorers

- **Name.** Python: `scorer.__name__` (a `functools.partial` has none: `ValueError`). TypeScript: `scorer.name`.
- **Arguments.** Python: keyword arguments, chosen by the scorer's parameter names among `input`, `output`, `expected`, `metadata`, `trace_id`, `item` (`expected` is `item.expected_output`, `metadata` is `item.metadata`, `trace_id` may be `None`). A scorer with `**kwargs` receives all six. A required parameter outside the six is a `ValueError` at step 1, naming `scorers[i]` and the parameter. TypeScript: one object `{input, output, expected, metadata, traceId, item}`.
- **Return value.** `bool`/`boolean` → a `boolean` score; `int`/`float`/`number` (not `bool`, finite) → `numeric`; `str`/`string` → `categorical`; a `Score` → its value, `reason`, `data_type` and `metadata`; `None`/`undefined` → no score. Anything else, or a non-finite number, counts as a scorer error.
- **Errors.** A scorer that raises (or returns a bad value) gives no score for that run; the result counts it in `scorer_errors[name]`, and the SDK logs one warning per scorer and `evaluate` call: `"cetra: scorer <index> failed (<ExceptionTypeName>)"` (the index, never the name or the message).
- **Queued score.** Through the runtime's score queue: target `trace` with the run's trace id, `name` = the scorer name, `source: "sdk"`, a fresh UUIDv7 `score_id`, `reason`/`metadata` from a `Score`, plus `experiment_id` and `dataset_item_id` (D-J-12). Masking applies to `reason` and `metadata` as in `score()`.

`Score` is a small public type: Python `@dataclass(frozen=True, slots=True, kw_only=True) class Score: value: bool | int | float | str; reason: str | None = None; data_type: DataType | None = None; metadata: Mapping[str, object] | None = None` (validated like `score()`'s arguments when used; an invalid one counts as a scorer error). TypeScript `interface Score { value: boolean | number | string; reason?: string; dataType?: ScoreDataType; metadata?: Readonly<Record<string, unknown>> }`, recognised by `typeof value === "object" && value !== null && "value" in value`.

### Output

The row's `output` is the task output converted to JSON (Python `to_jsonable` from `_serialize`, TypeScript the serializer's JSON conversion used for payloads) and serialized compactly. When the serialized UTF-8 text is over 262,144 bytes, `output` becomes a **string**: the first 260,000 bytes of that text (cut on a character boundary) followed by `"…[cetra: truncated, original <n> bytes]"`. A task output of `None`/`undefined` leaves `output` out. A failed run has no `output`.

### Result

`ExperimentResult` (frozen):

| Python | TypeScript | Meaning |
|---|---|---|
| `experiment_id: str` | `experimentId: string` | |
| `experiment_name: str` | `experimentName: string` | |
| `dataset_id: str` | `datasetId: string` | |
| `dataset_version: int` | `datasetVersion: number` | the pinned version |
| `runs: tuple[Run, ...]` | `runs: readonly Run[]` | in item order, repetitions inner |
| `scorer_errors: Mapping[str, int]` | `scorerErrors: Readonly<Record<string, number>>` | scorer name → failed runs |
| `error_count: int` (property) | `errorCount: number` | runs with a task error |
| `summary() -> str` | `summary(): string` | below |
| `assert_min(name: str, threshold: float) -> None` | `assertMin(name: string, threshold: number): void` | below |

`Run`: `dataset_item_id`, `repetition`, `input`, `expected_output`, `output` (the task's value before conversion), `error`, `trace_id`, `latency_ns`, `scores: Mapping[str, Score]` (TypeScript camelCase).

`summary()` returns, lines joined with `\n`:

```text
Experiment <name> (<id>) on dataset <dataset_id> v<version>: <runs> runs, <errors> errors
  <scorer>: mean <m with 3 decimals> (<n> values)          numeric and boolean scorers (booleans as 0/1)
  <scorer>: <label> <count>, <label> <count>              categorical, labels sorted, counts descending
  <scorer>: no values                                      a scorer that produced nothing
  <scorer>: <k> errors                                     an extra line when scorer_errors[name] > 0
```

Scorers appear in the order given. `assert_min(name, threshold)` computes the mean of the numeric and boolean values of that scorer over every run; it raises `AssertionError` (Python) / `Error` (TypeScript) with `"<name>: mean <m:.3f> is below <threshold>"` when the mean is lower, and `"<name>: no numeric or boolean values"` when there is none or the name is unknown.

## J4.1 — Python: REST client and datasets

**Commit subject:** `sdk-py: add the REST client and datasets`

### Read first

`sdks/python/src/cetra/__init__.py`, `_config.py` (`resolve_config`, `ConfigError`, `Config`), `_scores.py` (`ScoreQueue._retry_delay`, `_post`, the constants), `_log.py`, `_runtime.py` (`current`), `tests/conftest.py` (all of it), `tests/test_scores.py` (helpers), `sdks/python/pyproject.toml`.

### Files

| File | Change |
|---|---|
| `sdks/python/src/cetra/_retry.py` | new: the shared retry delay |
| `sdks/python/src/cetra/_scores.py` | `ScoreQueue._retry_delay` calls `_retry.retry_delay` |
| `sdks/python/src/cetra/_client.py` | new: `ApiError`, `Client`, the HTTP layer |
| `sdks/python/src/cetra/_datasets.py` | new: `Datasets`, `Dataset`, `DatasetItem`, `DatasetInfo` |
| `sdks/python/src/cetra/__init__.py` | exports |
| `sdks/python/tests/conftest.py` | the recorder answers `GET`, `PATCH`, `DELETE`; `respond_with` |
| `sdks/python/tests/test_client.py` | new |
| `sdks/python/tests/test_datasets.py` | new |

### Steps

1. **`_retry.py`.** Move the body of `ScoreQueue._retry_delay` into

   ```python
   RETRY_BACKOFF_S: Final = (1.0, 2.0, 4.0)
   RETRY_AFTER_CAP_S: Final = 30.0

   def retry_delay(response: requests.Response | None, attempt: int) -> float:
       """``Retry-After`` seconds when a non-negative integer (capped at 30 s), else 1/2/4 s."""
   ```

   with the same comments. `_scores.py` deletes `_RETRY_BACKOFF_S`, `_RETRY_AFTER_CAP_S` and the method body; `ScoreQueue._retry_delay` becomes `return retry_delay(response, attempt)`. No behaviour changes; `test_scores.py` stays green unchanged.

2. **`_client.py`.**

   ```python
   class ApiError(Exception):
       """An error answer from the cetra API (a status outside 2xx, after retries).

       ``code`` is the problem's ``code``; ``errors`` its field errors as
       ``{"field": ..., "message": ...}`` dicts.
       """

       def __init__(
           self,
           *,
           method: str,
           path: str,
           status: int,
           code: str | None,
           detail: str | None,
           errors: Sequence[Mapping[str, object]],
       ) -> None: ...
       status: int
       code: str | None
       detail: str | None
       errors: tuple[Mapping[str, object], ...]
   ```

   A network failure or timeout after the last retry raises `ApiError` with `status=0`, `code="network"`, `detail=<exception type name>`.

   ```python
   class Client:
       """REST client for datasets (spec 10 § Datasets and experiments).

       Configuration resolves as for ``init()``: argument, then ``CETRA_API_KEY`` /
       ``CETRA_BASE_URL``, then the default URL. Raises ``ConfigError`` without a key.
       """

       def __init__(self, *, api_key: str | None = None, base_url: str | None = None) -> None: ...
       @property
       def datasets(self) -> "Datasets": ...
   ```

   `__init__` calls `resolve_config(api_key=api_key, base_url=base_url, enabled=True)`; when the result is not enabled (no key), raise `ConfigError("api_key: required")`. A key with the wrong prefix raises from `resolve_config` as for `init`. It keeps `base_url`, `api_key` (never in `repr`: define `__repr__` returning `f"Client(base_url={self._base_url!r})"`), and one `requests.Session`.

   The private HTTP method (used by `_datasets.py` and `_experiments.py`):

   ```python
   def _request(
       self,
       method: str,
       template: str,                     # e.g. "/datasets/{dataset}/items"; used in ApiError
       path: str,                         # the filled path, segments already encoded
       *,
       params: Mapping[str, str | int] | None = None,
       body: object | None = None,
       retry: bool | None = None,         # None: True for GET and DELETE, False otherwise
   ) -> Any:
       """One API call, retried only when ``retry``; the decoded JSON body, or ``None`` for 204."""
   ```

   It posts `json=body` when a body is given; timeout 30 s; when `retry` resolves to true, the retry policy of § HTTP with `retry_delay`, else one attempt (a network error or timeout raises `ApiError(status=0, code="network")` at once); sleeping through `self._sleep` (an attribute, default `time.sleep`, so tests replace it). On a non-2xx answer it decodes the problem JSON when the `Content-Type` is `application/problem+json` or `application/json` (ignoring decode errors) and raises `ApiError`. Add `def _segment(value: str) -> str: return urllib.parse.quote(value, safe="")`.

3. **`_datasets.py`.**

   ```python
   @dataclass(frozen=True, slots=True)
   class DatasetItem:
       """One item of a dataset version (D-J-2)."""
       id: str
       input: Any
       expected_output: Any          # None when absent
       metadata: Mapping[str, Any]
       version: int                  # the version this revision appeared in
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
       version: int                   # the version these items belong to
       items: tuple[DatasetItem, ...]
       _client: "Client" = field(repr=False, compare=False)

       def add_items(self, items: Iterable[Mapping[str, Any]]) -> "Dataset":
           """Add items; each batch of up to 1,000 makes one new version (D-J-3).

           Each mapping has ``input`` (required), ``expected_output`` and ``metadata``
           (a mapping); any other key is a ``ValueError``. Returns the latest version.
           """

   class Datasets:
       """``client.datasets``."""
       def create(
           self,
           name: str,
           *,
           description: str | None = None,
           input_schema: Mapping[str, Any] | None = None,
           expected_schema: Mapping[str, Any] | None = None,
       ) -> Dataset: ...                # version 0, no items
       def get(self, name_or_id: str, *, version: int | None = None) -> Dataset: ...
       def list(self) -> list[DatasetInfo]: ...
       def delete(self, name_or_id: str) -> None: ...
   ```

   - `create`: `POST /datasets` with the given fields (absent ones left out); builds a `Dataset` with `version=0`, `items=()`.
   - `get`: `GET /datasets/{dataset}` for the dataset, then `GET /datasets/{dataset_id}/items?version=<v>&limit=1000` page by page (`cursor`) where `v` is `version` or the dataset's `current_version`.
   - `list`: `GET /datasets?limit=1000` page by page.
   - `delete`: `DELETE /datasets/{dataset}`.
   - `add_items`: validates every mapping first (keys, `input` present, `metadata` a mapping when given), converts values with `to_jsonable`, posts `POST /datasets/{id}/items` `{items: [...]}` per batch of 1,000, then returns `self._client.datasets.get(self.id)`. An empty iterable returns `self` without a request.
   - JSON → dataclass conversion in one private function per type; unknown response keys are ignored.

4. **`__init__.py`.** Export `ApiError`, `Client`, `Dataset`, `DatasetInfo`, `DatasetItem` (keep `__all__` sorted).

5. **`conftest.py`.** `_RecordingHandler` gets `do_GET`, `do_PATCH` and `do_DELETE` that share `do_POST`'s body (read `Content-Length`, which is absent on a GET: default `"0"`). `HttpRecorder` gets

   ```python
   def respond_with(self, handler: Callable[[str, str, bytes], tuple[int, dict[str, str], bytes]]) -> None:
       """Answer every request without a queued response with ``handler(method, path, body)``."""
   ```

   `take_response` takes `method` too; order: a queued response, then the handler, then the path body. The handler sees the full path with its query string.

### Tests

`tests/test_client.py` (with `http_recorder`; replace `client._sleep` with a recorder of durations):

1. `client_needs_a_key`: `Client(api_key=None)` with no env key raises `ConfigError` whose message is `api_key: required` (use `monkeypatch.delenv("CETRA_API_KEY", raising=False)`).
2. `client_repr_hides_the_key`.
3. `request_sends_the_bearer_and_json`: a `POST` records path `/api/v1/projects/current/datasets`, `Authorization: Bearer sk-cetra-test`, `Content-Type: application/json`.
4. `retries_5xx_then_succeeds`: a `GET` gets 503, 429 (`Retry-After: 2`), 200; sleeps `[1.0, 2.0]`.
5. `gives_up_after_three_retries`: a `GET` gets four 500s and raises `ApiError(status=500)`; sleeps `[1.0, 2.0, 4.0]`.
5a. `create_is_not_retried`: `client.datasets.create("faq")` gets a 503: `ApiError(status=503)` is raised, exactly one request was recorded and no sleep happened. Same for a network error on create (`status=0`, one attempt, no sleep).
5b. `add_items_is_not_retried`: a 503 on `POST …/items` raises after one request.
6. `problem_becomes_api_error`: a 409 problem `{"code":"conflict","detail":"a dataset with this name exists","errors":[]}` raises `ApiError` with `status == 409`, `code == "conflict"`, and `str(error) == "POST /datasets: 409 conflict: a dataset with this name exists"`.
7. `402_is_not_a_pause`: a 402 raises `ApiError(status=402)` at once, no sleep.
8. `network_error_is_retried_then_raised`: a `GET` from a client pointed at `http://127.0.0.1:9` raises `ApiError(status=0, code="network")` after three sleeps.

`tests/test_datasets.py` (a small fake server through `respond_with`, keyed by method and path prefix):

1. `create_posts_the_fields_and_returns_version_0`.
2. `get_pages_items_and_pins_the_version`: two item pages (`next_cursor` then `null`); the second request's query is `version=3&limit=1000&cursor=c1`; the `Dataset` has 3 items in order.
3. `get_encodes_a_name`: `get("faq v1/x")` requests `/api/v1/projects/current/datasets/faq%20v1%2Fx`.
4. `add_items_batches_by_1000`: 2,500 items → three `POST …/items` with 1,000, 1,000 and 500 items, then one `get`.
5. `add_items_rejects_unknown_keys`: `[{"input": 1, "answer": 2}]` raises `ValueError` before any request.
6. `list_pages`, `delete_sends_delete`.
7. `get_404_raises`: `ApiError` with status 404.

### Done when

- `just check-py` and `just check` pass; `test_scores.py` is unchanged and green.
- `python -c "import cetra; cetra.Client"` works in `sdks/python` (`uv run`).

## J4.2 — Python: `evaluate` and `aevaluate`

**Commit subject:** `sdk-py: run experiments with evaluate`

### Read first

`_context.py` (all), `_processor.py` (all), `_spans.py` (`span`, `Span`, `_start_span`), `_scores.py` (`_invalid_reason`, `_build_item`, `uuid7`), `_serialize.py` (`to_jsonable`), `_runtime.py` (`current`, `flush`, `Runtime.config`), `_log.py` (`warn_once`), J4.1's `_client.py` and `_datasets.py`, `tests/conftest.py` (`memory`, `score_runtime`, `spans`), `tests/test_context.py`.

### Files

| File | Change |
|---|---|
| `sdks/python/src/cetra/_context.py` | `ContextValues.experiment`; `context()` copies it; `experiment_scope` |
| `sdks/python/src/cetra/_processor.py` | stamp the linkage and the `experiment` environment |
| `sdks/python/src/cetra/_scores.py` | `queue_experiment_score` |
| `sdks/python/src/cetra/_experiments.py` | new: `Score`, `Run`, `ExperimentResult`, `evaluate`, `aevaluate` |
| `sdks/python/src/cetra/__init__.py` | exports |
| `sdks/python/tests/test_context.py` | scope tests |
| `sdks/python/tests/test_experiments.py` | new |

### Steps

1. **`_context.py`.**

   ```python
   @dataclass(frozen=True, slots=True)
   class ExperimentLink:
       """The experiment and dataset item of a run (D-J-16)."""
       experiment_id: str
       dataset_item_id: str
   ```

   `ContextValues` gains `experiment: ExperimentLink | None = None` (last field). `context._enter` passes `experiment=outer.experiment` to the merged values. Add

   ```python
   @contextlib.contextmanager
   def experiment_scope(experiment_id: str, dataset_item_id: str) -> Iterator[None]:
       """Run the body in a fresh context holding only the experiment link.

       The context starts from ``Context()``: no active span, no baggage and none of
       the caller's session, user, tags or metadata, so the run's root span starts a
       new trace. Internal (``evaluate``).
       """
   ```

   It does `token = attach(set_value(CONTEXT_KEY, ContextValues(experiment=ExperimentLink(...)), Context()))` and `detach(token)` in `finally`.

2. **`_processor.py`.** In `_stamp`, after the metadata stamp:

   ```python
   if values.experiment is not None:
       span.set_attribute("cetra.experiment.id", values.experiment.experiment_id)
       span.set_attribute("cetra.dataset_item.id", values.experiment.dataset_item_id)
       span.set_attribute("cetra.environment", "experiment")
   elif self._environment is not None:
       span.set_attribute("cetra.environment", self._environment)
   ```

   (replacing the current environment `if`). Release is unchanged.

3. **`_scores.py`.** Add, next to `score()`:

   ```python
   def queue_experiment_score(
       runtime: Runtime,
       *,
       name: str,
       value: bool | int | float | str,
       trace_id: str,
       experiment_id: str,
       dataset_item_id: str,
       reason: str | None,
       data_type: DataType | None,
       metadata: Mapping[str, object] | None,
   ) -> str | None:
       """Queue one experiment score (D-J-12): a ``score()`` item plus the experiment
       fields; ``None`` and one warning when invalid or not queued. Internal."""
   ```

   It runs `_invalid_reason` (trace target, no span, no session, no score id) and `_build_item`, then sets `item["experiment_id"]` and `item["dataset_item_id"]`, then `runtime.scores.put(item)`. The `Runtime` import is under `TYPE_CHECKING` (the module already defers its runtime import).

4. **`_experiments.py`.** Public:

   ```python
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
       experiment_id: str
       experiment_name: str
       dataset_id: str
       dataset_version: int
       runs: tuple[Run, ...]
       scorer_errors: Mapping[str, int]
       @property
       def error_count(self) -> int: ...
       def summary(self) -> str: ...
       def assert_min(self, name: str, threshold: float) -> None: ...

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
       arguments, ``ApiError`` when the API refuses a call.
       """

   async def aevaluate(...same parameters...) -> ExperimentResult:
       """``evaluate`` for async tasks and scorers; sync ones run in ``asyncio.to_thread``."""
   ```

   - Follow § `evaluate` flow exactly. Implement the shared parts once: `_prepare(...)` (checks, client, create, items) returns a private `_Plan` with the experiment, items and the scorer table (`name`, callable, accepted parameter names); `_run_sync(plan, item, repetition)` and `async _run_async(...)` each do one run and return a `Run` plus the row dict; `_Uploader` holds the pending rows, a `threading.Lock`, and `add(row)` / `drain()` that post batches of at most 500 rows and 32 MiB of outputs (step 6) through the client with `retry=True`; the create is `_request("POST", "/experiments", "/experiments", body=..., retry=False)` and finish `_request(..., retry=True)`; `_finish(...)` flushes and finishes; `_result(...)` builds the `ExperimentResult`.
   - `evaluate`: a `concurrent.futures.ThreadPoolExecutor(max_workers=concurrency, thread_name_prefix="cetra-evaluate")`; submit every run, collect `Run`s by index so `runs` keeps the item order. On an upload `ApiError`, set a stop flag that makes pending runs return without running (they are left out of `runs`), then follow step 6.
   - `aevaluate`: an `asyncio.Semaphore(concurrency)` and `asyncio.gather`. An async task is awaited inside the scope and the span (`async with span(...)`); a sync task runs in `asyncio.to_thread` with `contextvars.copy_context()` (`to_thread` already copies the context). Scorers likewise. Uploads run in `asyncio.to_thread(uploader.drain_full)` so the event loop is not blocked.
   - The root span: `with span(experiment_name, kind="chain", input=item.input) as handle:`, call the task inside; `handle.set_output(output)` on success; on an exception, catch it **inside** the `with` after recording: let the exception propagate out of the `with` block (so `span.__exit__` records it), and catch it right outside. Then `handle.trace_id`.
   - Output conversion and truncation: § Output, with `json.dumps(to_jsonable(output), separators=(",", ":"), ensure_ascii=False, allow_nan=False)`; a conversion failure makes `output` the string `"<unserializable TypeName>"`.
   - Scores are queued with `queue_experiment_score(runtime, ...)` when `trace_id` is not `None`.

5. **`__init__.py`.** Export `ExperimentResult`, `Run`, `Score`, `aevaluate`, `evaluate`.

### Tests

`tests/test_context.py`:

1. `experiment_scope_drops_the_callers_values`: inside `context(session_id="s", metadata={"a": 1})` and an open span, `experiment_scope("e", "i")` gives `current_values() == ContextValues(experiment=ExperimentLink("e", "i"))` and no current span; after the block the outer values are back.
2. `context_inside_a_scope_keeps_the_link`: `context(user_id="u")` inside the scope keeps `experiment` and adds the user.

`tests/test_experiments.py`. A fixture `api(memory, http_recorder)` starts the in-memory runtime with `base_url=http_recorder.url` (`start(base_url=...)` from `memory`) and installs a fake server through `respond_with`: `POST /experiments` answers the flat `{"experiment": <e1 on dataset d1 version 2, repetitions echoed>, "items": [<first page>], "next_cursor": <"c1" when the test asks for two pages, else null>}`; `GET /datasets/d1/items` answers the second page; `POST /experiments/e1/items` answers `{"stored": n, "skipped": 0}`; `POST /experiments/e1/finish` answers the experiment `completed`; `POST /api/v1/scores` answers `{"score_ids": []}`. Helpers parse the recorded bodies.

1. `evaluate_needs_init`: no runtime → `ConfigError`, no request.
2. `bad_arguments_raise_before_any_request`: `concurrency=0`, `concurrency=65`, `repetitions=21`, a lambda scorer, two scorers named alike, a coroutine task (each a separate `pytest.mark.parametrize` case).
3. `create_body`: `dataset="faq"`, `metadata={"model": "m"}`, `repetitions=2` → the recorded body; a `Dataset` object sends its id and version.
4. `pages_items_with_the_pinned_version`: two pages; the `GET` query has `version=2&limit=1000&cursor=`.
5. `each_run_is_its_own_trace_with_the_linkage`: two items, `repetitions=2`, a task that opens a child `cetra.span("inner")`; the finished spans hold 4 roots named after the experiment (no parent), each with a child; every span has `cetra.experiment.id == "e1"`, `cetra.dataset_item.id` equal to its item, `cetra.environment == "experiment"` even with `init(environment="prod")`; the root's `cetra.input` is the item input and `cetra.output` the task output.
6. `caller_context_does_not_leak`: `evaluate` called inside `cetra.context(session_id="s")` and an open span: the roots have no parent and no `cetra.session.id`.
7. `task_with_item`: a two-parameter task receives the `DatasetItem`.
8. `task_error_is_recorded`: a task raising `KeyError("x")` on one item: that row has `error == "KeyError: 'x'"`, no `output`; its root span has status ERROR and an `exception` event; scorers did not run for it; `evaluate` returns normally and `error_count == 1`.
9. `scorer_arguments_by_name`: scorers `def exact(output, expected)`, `def uses_all(**kw)`, `def with_trace(trace_id, item)` receive the right values.
10. `scorer_return_types`: `True`, `0.5`, `"good"`, `Score(value=1, reason="r", metadata={"k": "v"})` and `None` give boolean, numeric, categorical, numeric with reason, and no score; the score bodies have `target.type == "trace"`, the run's trace id, `experiment_id == "e1"`, `dataset_item_id` and `source == "sdk"`.
11. `scorer_errors_are_counted_and_logged_once`: a scorer raising on every run → `scorer_errors == {"bad": 4}`, exactly one WARNING record `cetra: scorer 0 failed (RuntimeError)`; a scorer returning `float("nan")` counts too.
12. `results_are_batched_by_500`: 1,200 items with a trivial task → three `POST …/items` with 500, 500 and 200 results, then one `finish`, which comes after the last score post (check the recorded order); every recorded `repetition` is `0` (0-based).
12a. `results_are_batched_by_bytes`: 300 items whose task returns `"x" * 200_000` (each output ≈ 200 KiB, under the 256 KiB cut) with `repetitions=1` → the batches each hold at most 32 MiB of outputs: the first post holds 167 rows (167 × 200,002 B ≤ 32 MiB < 168 × 200,002 B), and the row counts sum to 300. Assert with the computed byte sums, not the literal 167, if the marker or quoting changes the per-row size.
12b. `repetitions_are_zero_based`: `repetitions=3` on one item → posted repetitions `[0, 1, 2]`.
12c. `create_experiment_is_not_retried`: `POST /experiments` answers 503 → `ApiError(status=503)` raised after one request; no items post and no finish.
12d. `finish_is_retried`: the first `finish` answers 503, the second 200 → `evaluate` returns normally; two finish requests were recorded and one sleep of 1.0 s.
12e. `score_pause_warns_and_keeps_running`: `POST /api/v1/scores` answers 402 with `Retry-After: 60` → `evaluate` finishes normally (results posted, finish sent), the result still holds the scores, and exactly one WARNING with the text of § 402 from the score queue was logged.
13. `output_is_truncated`: a task returning `"x" * 300_000` → the posted `output` is a string of 260,000 `x` followed by the marker with `300002` bytes (the JSON string with quotes).
14. `upload_failure_finishes_with_an_error_and_raises`: the second items post answers 422 → `ApiError(status=422)` raised; one `finish` with `{"error": "results upload failed"}` was sent.
15. `finish_failure_raises`.
16. `summary_and_assert_min`: a fixed `ExperimentResult` built directly: `summary()` equals the expected text exactly; `assert_min("exact", 0.5)` passes; `assert_min("exact", 0.9)` raises with the message; `assert_min("missing", 0)` raises.
17. `aevaluate_runs_async_tasks`: `asyncio.run(cetra.aevaluate(...))` with an async task and an async scorer, `concurrency=2`, records the same rows and linkage as test 5.
18. `sample_rate_warning`: `init(sample_rate=0.5)` → one warning with the exact text.
19. `no_payload_in_logs`: run with a task that raises `ValueError("secret-canary")` and a scorer that raises `ValueError("secret-canary-2")`; no log record contains `secret-canary`.

### Done when

- `just check-py` and `just check` pass.
- `mypy --strict` passes with no new `type: ignore`.

## J4.3 — Python: README and end-to-end

**Commit subject:** `sdk-py: document datasets and experiments`

### Read first

`sdks/python/README.md` (all), `tests/test_readme.py`, `tests/test_e2e.py`, `tools/e2e/run.sh`, `CHANGELOG.md` § [Unreleased].

### Files

| File | Change |
|---|---|
| `sdks/python/README.md` | new section "Datasets and experiments", after "Scores" |
| `sdks/python/tests/test_readme.py` | only if the new tested block needs the fake API server (below) |
| `sdks/python/tests/test_e2e.py` | one more test |
| `CHANGELOG.md` | one `### Added` line |

### Steps

1. **README section.** Subsections: *Datasets* (`Client`, `create`, `add_items`, `get(version=)`, `list`, `delete`; one item = `input`, `expected_output`, `metadata`; every `add_items` batch of 1,000 is a new version; the dataset's JSON Schemas are checked by the server and a refused item raises `ApiError` with the field errors); *Experiments* (the spec 10 example without `server_evaluators`: task signature rules, scorer argument names and return types, `Score`, `concurrency` and `repetitions` ranges, `aevaluate`, what `summary()` prints, `assert_min` for CI); *What gets recorded* (one trace per run with environment `experiment`, excluded from the default trace lists; scores need a trace, so `sample_rate` below 1 loses some; results upload in batches; nothing is scored on the server, D-J-11); *Keys* (the key needs `ingest`, `read` and `write`); *Errors* (`ConfigError`, `ApiError` fields; task and scorer exceptions do not abort the run).
2. Mark the experiments example `<!-- test -->` only if `test_readme.py` can run it against a fake API; read `test_readme.py` first. If it would need more than a `respond_with` fake added in that file's fixture, leave the block untested and say so in the report.
3. **e2e test** `test_dataset_experiment_round_trip` in `test_e2e.py`: `cetra.init(api_key=key, base_url=base_url)`; `client = cetra.Client(api_key=key, base_url=base_url)`; create a dataset named `f"e2e-py-{cetra.uuid7()}"`; `add_items` two items `{"input": {"q": "a"}, "expected_output": "A"}` and `{"input": {"q": "b"}, "expected_output": "B"}`; `evaluate(dataset=ds, task=lambda i: i["q"].upper(), scorers=[exact])` with `def exact(output, expected): return output == expected`; assert `result.error_count == 0` and `assert_min("exact", 1.0)`; `cetra.shutdown()`; then `_poll` `GET {base_url}/api/v1/projects/current/experiments/{result.experiment_id}/results` until `summary.scores` holds `exact` with `count == 2` and `pass_rate == 1.0`, and its `experiment.status == "completed"`. Finally delete the dataset. (This runs only in `just e2e`, after M3.)
4. **CHANGELOG** under `### Added`: `- Python SDK: \`cetra.Client\` for datasets and \`cetra.evaluate\` / \`aevaluate\` to run experiments on your machine (track J).`

### Tests

`test_readme.py` keeps passing; the e2e test is skipped by `just check` and runs in `just e2e`.

### Done when

`just check` passes. Report whether `just e2e` was run (it needs the server built with M3).

## J4.4 — TypeScript: REST client and datasets

**Commit subject:** `sdk-ts: add the REST client and datasets`

### Read first

`sdks/typescript/src/index.ts`, `config.ts` (`resolveConfig`, `ConfigError`), `web.ts` (`FetchLike`, `getFetch`, `runtimeGlobals`), `scores.ts` (`backoffMillis`, `waitMillis`, `defaultSleep`, `ScoreQueue.sendOne`), `serialize.ts` (the JSON conversion used for payloads), `scores.test.ts` (the scripted fetch), `testing.test-helpers.ts`, `package.json`, `tsdown.config.ts`, `tsconfig.json`.

### Files

| File | Change |
|---|---|
| `sdks/typescript/src/retry.ts` | new: `backoffMillis`, `waitMillis`, `defaultSleep` moved from `scores.ts` |
| `sdks/typescript/src/scores.ts` | imports them |
| `sdks/typescript/src/client.ts` | new: `ApiError`, `Client`, `RestFetch` |
| `sdks/typescript/src/datasets.ts` | new: `Datasets`, `Dataset`, `DatasetItem`, `DatasetInfo` |
| `sdks/typescript/src/index.ts` | exports |
| `sdks/typescript/src/client.test.ts`, `datasets.test.ts` | new |

### Steps

1. **`retry.ts`.** Move `backoffMillis`, `waitMillis` and `defaultSleep` (and their constants) out of `scores.ts` unchanged and export them; `scores.ts` imports them. `scores.test.ts` stays green unchanged.

2. **`client.ts`.** `FetchLike` cannot send a GET (its `body` is required) or read a body, so the client has its own structural type, which the global `fetch` satisfies:

   ```ts
   /** The part of `fetch` the REST client uses. */
   export type RestFetch = (
     url: string,
     init: { method: string; headers: Record<string, string>; body?: string; signal?: unknown },
   ) => Promise<{ status: number; ok: boolean; headers: { get(name: string): string | null }; text(): Promise<string> }>;

   export interface ClientOptions {
     apiKey?: string;
     baseUrl?: string;
     /** Tests inject a fake; the default is the global `fetch`. */
     fetch?: RestFetch;
     /** Tests inject a fake; the default is `setTimeout`. */
     sleep?: (ms: number) => Promise<void>;
   }

   export class ApiError extends Error {
     readonly status: number;
     readonly code: string | undefined;
     readonly detail: string | undefined;
     readonly errors: readonly { field: string; message: string }[];
   }

   export class Client {
     constructor(options?: ClientOptions);
     readonly datasets: Datasets;
     /** Internal: one API call with retries. */
     request<T>(method: string, template: string, path: string, options?: { query?: Record<string, string | number>; body?: unknown; retry?: boolean }): Promise<T>;
   }
   ```

   - The constructor resolves `resolveConfig({ apiKey, baseUrl, enabled: true })`; not enabled → `throw new ConfigError("apiKey: required")`. No global `fetch` and no option → `ConfigError("fetch: not available in this runtime")`.
   - `request` takes `options.retry?: boolean` (default `true` for `GET` and `DELETE`, `false` otherwise; `datasets.ts` and `experiments.ts` pass `retry: true` for the results post and finish only, § HTTP).
   - `request`: URL `${baseUrl}/api/v1/projects/current${path}` plus the query (`URLSearchParams` is not in the ES2023 lib: build the query with `encodeURIComponent` by hand), `AbortSignal.timeout(30_000)` when available (`runtimeGlobals().AbortSignal?.timeout`), JSON body, § HTTP retries with `waitMillis`, `ApiError` from a problem body, `status: 0, code: "network"` after network failures. A `204` returns `undefined`. `ApiError.name` is `"ApiError"`. Mark `request` with a `/** @internal */` comment; it stays public for `datasets.ts` and `experiments.ts`.
   - `segment(value: string): string` = `encodeURIComponent(value)`.

3. **`datasets.ts`.** Same API as Python, camelCase:

   ```ts
   export interface DatasetItem { readonly id: string; readonly input: unknown; readonly expectedOutput: unknown; readonly metadata: Readonly<Record<string, unknown>>; readonly version: number; readonly sourceTraceId: string | undefined; readonly sourceSpanId: string | undefined }
   export interface DatasetInfo { readonly id: string; readonly name: string; readonly description: string | undefined; readonly currentVersion: number; readonly itemCount: number }
   export interface NewDatasetItem { input: unknown; expectedOutput?: unknown; metadata?: Readonly<Record<string, unknown>> }
   export class Dataset {
     readonly id: string; readonly name: string; readonly description: string | undefined;
     readonly version: number; readonly items: readonly DatasetItem[];
     addItems(items: Iterable<NewDatasetItem>): Promise<Dataset>;
   }
   export class Datasets {
     create(name: string, options?: { description?: string; inputSchema?: Record<string, unknown>; expectedSchema?: Record<string, unknown> }): Promise<Dataset>;
     get(nameOrId: string, options?: { version?: number }): Promise<Dataset>;
     list(): Promise<DatasetInfo[]>;
     delete(nameOrId: string): Promise<void>;
   }
   ```

   Wire names are snake_case (`expected_output`, `input_schema`, …); conversion in one function per type. `addItems` rejects an item with a key other than `input`, `expectedOutput`, `metadata` with a `TypeError` before any request.

4. **`index.ts`.** Export `ApiError`, `Client`, `type ClientOptions`, `Dataset`, `Datasets` (type only), `type DatasetInfo`, `type DatasetItem`, `type NewDatasetItem`. No new package entry: `tsdown.config.ts` and `package.json` `exports` are unchanged.

### Tests

`client.test.ts` and `datasets.test.ts`, with a fake `RestFetch` recording `(method, url, headers, body)` and answering from a list or a function, and a `sleep` that records durations. The same cases as J4.1's tests 1–8 and datasets tests 1–7 (names in camelCase: `clientNeedsAKey`, …, `getEncodesAName` expects `/datasets/faq%20v1%2Fx`).

### Done when

`just check-ts` and `just check` pass (`check:package`, publint and attw included: the new exports are in the main entry).

## J4.5 — TypeScript: `evaluate`

**Commit subject:** `sdk-ts: run experiments with evaluate`

### Read first

`context.ts` (all), `processor.ts` (all), `span.ts` (`tracedSpan`, `startTracedSpan`, `recordError`, `setPayload`, `LiveSpanHandle`, `withSpan`), `scores.ts` (`invalidReason`, `buildBody`, `ScoreBody`), `serialize.ts`, `runtime.ts` (`currentRuntime`, `flush`), `log.ts` (`warnOnce`), `uuid.ts`, J4.4's files, `testing.test-helpers.ts`, `span.test.ts`.

### Files

| File | Change |
|---|---|
| `sdks/typescript/src/context.ts` | `ContextValues.experiment`; `withContext` copies it; `coerceValues` keeps it; `experimentContext` |
| `sdks/typescript/src/processor.ts` | stamp the linkage and the environment |
| `sdks/typescript/src/scores.ts` | `ScoreBody` experiment fields; `queueExperimentScore` |
| `sdks/typescript/src/experiments.ts` | new: `Score`, `Run`, `ExperimentResult`, `evaluate`, `EvaluateOptions`, `Scorer` |
| `sdks/typescript/src/index.ts` | exports |
| `sdks/typescript/src/context.test.ts`, `experiments.test.ts` | tests |

### Steps

1. **`context.ts`.** `ContextValues` gains `readonly experiment: { readonly experimentId: string; readonly datasetItemId: string } | undefined`; `EMPTY_VALUES` has it `undefined`; `contextFor` copies `outer.experiment`; `coerceValues` keeps a well-formed one (two strings), else `undefined`. Add

   ```ts
   /** A context with only the experiment link, built on ROOT_CONTEXT (no span, no baggage). Internal. */
   export function experimentContext(experimentId: string, datasetItemId: string): Context;
   ```

2. **`processor.ts`.** In `onStart`, after `stampContext`: read `currentValues(parentContext).experiment`; when set, set `cetra.experiment.id`, `cetra.dataset_item.id` and `cetra.environment = "experiment"`; else the configured environment as today. Release unchanged.

3. **`scores.ts`.** `ScoreBody` gains `experiment_id?: string; dataset_item_id?: string`. Add `export function queueExperimentScore(runtime: Runtime, options: ScoreOptions & { traceId: string; experimentId: string; datasetItemId: string }): string | undefined`: `invalidReason`, a minted id, `buildBody`, the two fields, `runtime.scores.put`; one warning when invalid, as `score()`.

4. **`experiments.ts`.**

   ```ts
   export interface Score { value: boolean | number | string; reason?: string; dataType?: ScoreDataType; metadata?: Readonly<Record<string, unknown>> }
   export interface ScorerArgs { input: unknown; output: unknown; expected: unknown; metadata: Readonly<Record<string, unknown>>; traceId: string | undefined; item: DatasetItem }
   export type Scorer = (args: ScorerArgs) => boolean | number | string | Score | undefined | null | Promise<boolean | number | string | Score | undefined | null>;
   export interface EvaluateOptions {
     dataset: string | Dataset;
     task: (input: unknown, item: DatasetItem) => unknown;
     scorers?: readonly Scorer[];
     experimentName?: string;
     description?: string;
     metadata?: Readonly<Record<string, unknown>>;
     datasetVersion?: number;
     concurrency?: number;   // 1–64, default 8
     repetitions?: number;   // 1–20, default 1
     client?: Client;
   }
   export interface Run { … § Result, camelCase … }
   export class ExperimentResult { … § Result … }
   export function evaluate(options: EvaluateOptions): Promise<ExperimentResult>;
   ```

   - § `evaluate` flow. Concurrency: `concurrency` async workers that take the next run index from a shared counter until none is left or the stop flag is set; `runs` are stored by index.
   - A run: `context.with(experimentContext(experimentId, item.id), () => tracedSpan(...))` through the existing span runner with `name = experimentName`, `kind = "chain"`, the item input, and a body that awaits the task (a sync task's value is wrapped in `Promise.resolve`) and calls `handle.setOutput(output)`; the runner records a rejection on the span. Catch the rejection outside to record the run's error (`${error.name}: ${error.message}` for an `Error`, else `String(error)`, cut to 4,096). Read the trace id inside the body with `currentTraceId()`.
   - Scorers run after the span ended, awaited one after the other.
   - Output: § Output with the SDK's JSON conversion (the one `setPayload` uses for non-strings) and a byte count through `TextEncoder` (`runtimeGlobals().TextEncoder`; without one, count UTF-16 code units × 3 as an upper bound).
   - Uploads: a pending array with its running output byte total; after each run, while it holds 500 rows or the next row would pass 32 MiB, post a batch (awaited, so one post at a time); at the end post the rest (§ `evaluate` flow step 6).

5. **`index.ts`.** Export `evaluate`, `ExperimentResult`, `type EvaluateOptions`, `type Run`, `type Score`, `type Scorer`, `type ScorerArgs`.

### Tests

`context.test.ts`: `experimentContextHasOnlyTheLink`, `withContextInsideAnExperimentKeepsTheLink` (J4.2's context tests). `experiments.test.ts`: `startMemory({...}, {}, { fetch })` for the score queue and the span pipeline, and a `Client` with a fake `RestFetch` that implements the same small fake server as J4.2; J4.2's tests 1–16 (with 12a–12e) and 18–19 with camelCase names (no `aevaluate`; test 2 has no coroutine case; test 9 checks the single object argument; test 13 uses `"x".repeat(300_000)`).

### Done when

`just check-ts` and `just check` pass.

## J4.6 — TypeScript: README and end-to-end

**Commit subject:** `sdk-ts: document datasets and experiments`

### Read first

`sdks/typescript/README.md`, `readme.test.ts`, `readme-examples.test-helpers.ts`, `e2e.test.ts`, `CHANGELOG.md` § [Unreleased].

### Files

| File | Change |
|---|---|
| `sdks/typescript/README.md` | section "Datasets and experiments" |
| `sdks/typescript/src/readme-examples.test-helpers.ts`, `readme.test.ts` | an `evaluateExample` when it fits the existing harness |
| `sdks/typescript/src/e2e.test.ts` | one more test |
| `CHANGELOG.md` | one `### Added` line |

### Steps

1. README: the same subsections as J4.3 step 1, in TypeScript.
2. A tested `evaluateExample` (`<!-- readme:evaluateExample -->`) when the harness can give it a fake `RestFetch` through `client`; otherwise an untested block, reported.
3. e2e `"runs an experiment and reads its results back"`: J4.3 step 3 in TypeScript (dataset name `e2e-ts-${uuid7()}`).
4. CHANGELOG `### Added`: `- TypeScript SDK: \`new cetra.Client()\` for datasets and \`cetra.evaluate\` to run experiments on your machine (track J).`

### Done when

`just check` passes. Report whether `just e2e` was run.

## What this milestone assumes of other milestones

- M3's `ScoreCreate` accepts `experiment_id` and `dataset_item_id` on `POST /api/v1/scores` with an `sk` key (D-J-12), and the server validates that the experiment exists in the key's project.
- `POST /experiments` answers the flat `{experiment, items: [DatasetItem], next_cursor}` (D-J-10, `CreateExperimentResponse`) with `items` the first page (up to 1,000) of the pinned version.
- A repeated `POST /experiments/{id}/finish` with the same outcome answers `200` with the experiment (D-J-10); results posts are upserts; the outputs of one results request total at most 32 MiB.
- `GET /datasets/{dataset}/items` accepts an id for `{dataset}` and `limit=1000`.
- The experiment page link: the SDK prints the experiment id in `summary()` but no URL, because an `sk` key cannot learn its org slug (`GET /projects/current` has no org slug). A UI route that resolves an experiment id alone would let `summary()` print a link; it is not in J's plan.
