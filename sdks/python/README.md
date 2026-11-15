# Python SDK

`llmobs` is the Python SDK for llmobs, the single-binary LLM observability
server. It is a thin layer over OpenTelemetry: spans go out as OTLP/HTTP
protobuf to `<base_url>/v1/traces`, and scores go to
`<base_url>/api/v1/scores`. Python >= 3.11. MIT.

## Install

```bash
pip install llmobs
```

Auto-instrumentation is optional, one extra per library:

| `instrument` name | Extra |
|---|---|
| `openai` | `llmobs[openai]` |
| `anthropic` | `llmobs[anthropic]` |
| `langchain` | `llmobs[langchain]` |
| `llama-index` | `llmobs[llama-index]` |
| `litellm` | `llmobs[litellm]` |
| `google-genai` | `llmobs[google-genai]` |
| `bedrock` | `llmobs[bedrock]` |
| `openai-agents` | `llmobs[openai-agents]` |
| `crewai` | `llmobs[crewai]` |
| `dspy` | `llmobs[dspy]` |

`llmobs[all]` installs every instrumentor. The core package pulls in only
`opentelemetry-api`, `opentelemetry-sdk`, `opentelemetry-exporter-otlp-proto-http`
and `requests`.

## Quick start

<!-- test -->
```python
import llmobs

llmobs.init(api_key="sk-llmobs-test")


@llmobs.observe(kind="tool", name="search_docs")
def search_docs(query: str, limit: int = 2) -> list[str]:
    return ["doc-1", "doc-2"]


@llmobs.observe(name="answer")
def answer(question: str) -> str:
    docs = search_docs(question)
    return f"It is in {docs[0]}."


answer("why?")

llmobs.flush()  # blocks until spans (and scores) are exported
```

`init()` is called once per process; it also registers an `atexit` hook that
calls `shutdown()` (flush, then stop). `flush()` is the manual lever when you
need the data out now — see [Serverless](#serverless).

Without an API key (argument and `LLMOBS_API_KEY` both unset), `init()` logs
one warning — `llmobs: no API key; set LLMOBS_API_KEY, or LLMOBS_ENABLED=false
to silence this; tracing is disabled` — and the SDK runs as a no-op: nothing
raises, nothing is sent. Set `LLMOBS_ENABLED=false` to silence the warning.

## Configuration

An explicit argument wins over the environment variable, which wins over the
default. An argument set to `None` counts as not given; env values are trimmed
and an empty value counts as unset.

| Option | Env | Default |
|---|---|---|
| `api_key` | `LLMOBS_API_KEY` | unset: one warning, SDK disabled |
| `base_url` | `LLMOBS_BASE_URL` | `http://localhost:8080` |
| `environment` | `LLMOBS_ENVIRONMENT` | unset |
| `release` | `LLMOBS_RELEASE` | unset (falls back to `GIT_COMMIT`, then `SOURCE_VERSION`) |
| `capture_content` | `LLMOBS_CAPTURE_CONTENT` | `true` |
| `sample_rate` | `LLMOBS_SAMPLE_RATE` | `1.0` |
| `instrument` | — | `[]` |
| `mask` | — | `None` |
| `export_filter` | — | `None` |
| `use_global_provider` | — | `False` |
| `enabled` | `LLMOBS_ENABLED` | `true` (no-op mode when false) |

- `enabled` is resolved first; when it is false, no other option is read or
  validated.
- Booleans from the environment accept `true`/`false`, `1`/`0`, `yes`/`no`,
  `on`/`off`, case-insensitive.
- `api_key` must start with `sk-llmobs-`. A key with the wrong prefix raises;
  a missing key does not (see above).
- `base_url` must be an absolute `http`/`https` URL without query or fragment.
  A path is allowed (reverse-proxy prefix); trailing slashes are removed.
- `environment` must not be the reserved evaluation environment `llmobs-eval`.
- `sample_rate` must be a finite number in `[0, 1]` (a `ParentBased(
  TraceIdRatioBased)` sampler on our own provider). From the environment it
  must be a plain decimal (`1`, `0.25`, `.5`).
- Any other invalid value raises `ConfigError` (a `ValueError` subclass)
  naming the option or env var, never its value. Resolution stops at the
  first problem.
- `init()` is once per process: any later call — whatever the first one did —
  logs `llmobs: init() was already called; this call is ignored` and changes
  nothing.

## Decorator rules

`@llmobs.observe` traces a function, coroutine function, generator function or
async generator function as one span. Both forms work: `@llmobs.observe` and
`@llmobs.observe(name=..., kind=..., capture_input=..., capture_output=...,
reduce_output=...)`. The span name defaults to the function's qualified name;
`kind` defaults to `"chain"` and must be one of `llm`, `embedding`, `tool`,
`agent`, `chain`, `retriever`, `reranker`, `guardrail`, `evaluator`, `span` —
an invalid `kind` raises `ValueError` at decoration time, before the function
runs. A function decorated before `init()` is traced once the SDK starts: the
runtime is looked up at call time.

**Input.** The bound arguments with defaults applied, as one JSON object keyed
by parameter name. A first parameter named `self` or `cls` is dropped, `*args`
becomes a list and `**kwargs` an object under their parameter names.
`capture_input=False` records no input.

**Output.** A `str` return value is recorded as text; anything else is
serialized to compact JSON (dataclasses, pydantic v2 models, enums, UUIDs,
datetimes, `bytes` as base64 data URIs, mappings and sequences — with cycle
and depth guards). Payloads over 1 MiB are cut and marked
`…[llmobs: truncated, original N bytes]`. `capture_output=False` records no
output. A plain function that *returns* a generator is traced as a call: the
returned object is passed through untouched, never consumed, and its output is
not recorded.

**Async.** The span of a coroutine function starts when the coroutine is first
awaited, not when it is created.

**Generators.** Detected at decoration time. The span starts at the
generator's first step and is current only while the body runs. Exhaustion
records the items: the joined string when every item is a `str`, otherwise a
JSON list — or `reduce_output(items)` when given. Closing early (`break`,
`close()`, `aclose()`) ends the span with status `ok` and the items yielded so
far. A half-consumed sync generator that is garbage-collected is closed the
same way; an abandoned async generator is never exported (see
[Known limits](#known-limits)) — `aclose()` it if you stop early.

**Errors.** An exception raised by the function adds one `exception` event
(`exception.type`, `exception.message`, `exception.stacktrace`), sets the span
status to error with the message `"<type>: <message>"`, and is re-raised
unchanged. A `GeneratorExit` is not an error.

## Manual spans and `llm_span`

`llmobs.span(name, kind=..., input=..., metadata=...)` is a sync and async
context manager. The body's return value is never recorded; the handle's
setters write it instead.

<!-- test -->
```python
import llmobs

llmobs.init(api_key="sk-llmobs-test")

with llmobs.span("rerank", kind="reranker", input=["a", "b"]) as s:
    ranked = ["b", "a"]
    s.set_output(ranked)
    s.set_metadata(model="bge")

llmobs.flush()
```

The handle (`llmobs.Span`) exposes `set_input(value)`, `set_output(value)`,
`set_metadata(**values)` and the `trace_id` / `span_id` properties (32 and 16
lowercase hex characters). Every method is a no-op after the span ended and
never raises.

`llmobs.llm_span(name, provider=..., model=...)` writes the GenAI attributes
of a chat call — for providers without an instrumentor. `provider` and
`model` are required non-empty strings (a bad one raises `ValueError` at
construction).

<!-- test -->
```python
import llmobs

llmobs.init(api_key="sk-llmobs-test")

with llmobs.llm_span(
    "chat",
    provider="acme",
    model="acme-1",
    params={"temperature": 0.2},
    input_messages=[{"role": "user", "content": "hi"}],
) as s:
    s.set_output_messages([{"role": "assistant", "content": "hello"}])
    s.set_response_model("acme-1-2026-01")
    s.set_usage(input_tokens=12, output_tokens=3)
    s.set_cost(total_usd=0.0004)
    s.record_first_token()

llmobs.flush()
```

Messages may use the canonical `{"role", "parts"}` form or the short form
`{"role", "content": "<text>"}`, converted to one text part. On the
`llmobs.LlmSpan` handle:

| Setter | Attribute |
|---|---|
| `set_input_messages(messages)` | `gen_ai.input.messages` |
| `set_output_messages(messages)` | `gen_ai.output.messages` |
| `set_response_model(m)` | `gen_ai.response.model` |
| `set_usage(input_tokens=, output_tokens=, cache_read_tokens=, cache_write_tokens=, reasoning_tokens=)` | the `gen_ai.usage.*` counts; ints >= 0 only, `input_tokens` includes cache tokens |
| `set_cost(total_usd)` | `llmobs.cost.total` (USD, finite, >= 0) |
| `record_first_token()` | `llmobs.ttft_ns`; only the first call counts |
| `set_metadata(**values)` | `llmobs.metadata.<key>`, like every span |

The inherited `set_input` / `set_output` remain available for non-chat
payloads. With `capture_content=False` the models, params, usage, cost and
metadata are kept, but no messages, system instructions or input/output
payloads are written.

## Context and baggage

`llmobs.context(session_id=..., user_id=..., tags=[...], metadata={...})` is a
sync and async context manager that applies those values to every span started
inside — ours and the instrumentors' alike. The values live in the OpenTelemetry
context, so they follow `contextvars` (and `copy_context()`) across tasks.

Nested blocks merge: the inner `session_id`/`user_id` win when given, tags are
the union (outer first, no duplicates), metadata merges by key with the inner
value winning. Metadata values are masked when the block is entered.

<!-- test -->
```python
import llmobs

llmobs.init(api_key="sk-llmobs-test", environment="production", release="1.4.2")


@llmobs.observe(name="turn")
def turn() -> None: ...


with llmobs.context(session_id="chat-42", user_id="u-17", tags=["beta"], metadata={"plan": "pro"}):
    turn()

llmobs.flush()
```

Every span of the block then carries `llmobs.session.id`, `llmobs.user.id`,
`llmobs.tags`, `llmobs.metadata.plan`, and — when set at `init()` —
`llmobs.environment` and `llmobs.release`. Metadata keys must match
`[A-Za-z0-9_.-]+`; a key that does not is skipped (logged at debug level with
its length only).

`context(..., propagate=True)` additionally writes the session and user ids to
W3C **baggage** as `llmobs.session.id` and `llmobs.user.id`. It is opt-in
because baggage is sent as an HTTP header on every outgoing request of the app,
to third-party APIs included. Tags and metadata never go into baggage. A
downstream service running this SDK picks the ids up from baggage without
calling `context()` itself.

## Scores

`llmobs.score(...)` attaches a score to a trace, a span or a session. It
returns the score id — a UUIDv7 minted in the SDK — or `None` when nothing was
queued. Passing `score_id` updates that score instead of creating one, which
makes retries idempotent.

<!-- test -->
```python
import llmobs

llmobs.init(api_key="sk-llmobs-test")

with llmobs.span("answer", input="why?") as s:
    s.set_output("It is 42.")
    score_id = llmobs.score(
        trace_id=llmobs.current_trace_id(),
        name="correct",
        value=True,
        reason="matches the reference answer",
    )

llmobs.flush()  # also waits for pending scores
```

- **Target** (one of): `trace_id` + `span_id` (a span), `trace_id` alone (a
  trace), or `session_id` (a session). Ids are the lowercase hex strings
  `llmobs.current_trace_id()` / `llmobs.current_span_id()` return.
- **Value**: `True`/`False` is boolean, a number is numeric (must be finite),
  a string is categorical with the string as label, or pass
  `data_type="text"` for free text. `data_type` may also force `numeric`,
  `boolean` or `categorical` and must match the value's type.
- `reason`, `user_id` (taken from the active `context()` unless passed) and
  `metadata` are optional. Metadata values are strings, or any other value
  serialized to compact JSON.
- Invalid input logs one warning — `llmobs: score not queued: <reason>` — and
  returns `None`; nothing is queued and nothing raises.
- Scores are buffered (up to 10 000; a full buffer drops with a warning that
  counts the drops) and posted in batches of up to 1000 every second by a
  background thread. `429`, `5xx`, network errors and timeouts retry up to 3
  more times, honouring an integer `Retry-After` (capped at 30 s, else 1 s,
  2 s, 4 s); any other status drops the batch at once.
- `flush()` waits for pending scores too. `llmobs.uuid7()` is exported if you
  need your own idempotent ids.

## Datasets and experiments

`llmobs.Client` is the REST client for datasets and experiments. It reads
`api_key` and `base_url` the way `init()` does (argument, then
`LLMOBS_API_KEY` / `LLMOBS_BASE_URL`, then the default), and raises
`ConfigError` without a key. Unlike tracing and scoring, its calls raise on
failure (see [Errors](#errors)).

### Datasets

```python
import llmobs

client = llmobs.Client()  # the key and URL come from the environment

ds = client.datasets.create("faq-v1", description="FAQ questions")
ds = ds.add_items(
    [
        {
            "input": {"q": "How do I cancel?"},
            "expected_output": {"a": "Settings > Billing > Cancel"},
            "metadata": {"topic": "billing"},
        }
    ]
)
ds = client.datasets.get("faq-v1", version=1)  # the items of version 1
print([info.name for info in client.datasets.list()])
client.datasets.delete("faq-v1")  # the dataset and its items
```

- An item is `input` (any JSON value, required), `expected_output` (optional)
  and `metadata` (a mapping, default `{}`). Any other key raises `ValueError`
  before a request is sent.
- `add_items` sends batches of 1,000 items, and each batch makes a new
  version. Versions are integers: a new dataset is at version 0 and has no
  items. `add_items` returns the dataset at its latest version.
- `get(name_or_id, version=...)` reads a version with all its items; without
  `version` it reads the current one.
- `create(..., input_schema=..., expected_schema=...)` takes JSON Schemas
  (Draft 2020-12). The server checks every item written afterwards against
  them; a refused item raises `ApiError`, and its `errors` list names the
  failing fields.

### Experiments

`llmobs.evaluate` runs your task on every item of a dataset version, scores
each run, and stores the results on the server. It needs `llmobs.init()`
first.

```python
import llmobs

llmobs.init()  # the key comes from LLMOBS_API_KEY


def task(input: dict) -> str:  # receives item.input; may also take (input, item)
    return my_app.answer(input["q"])


def exact(output: str, expected: dict) -> bool:  # scorers take any of the names below
    return output.strip() == expected["a"].strip()


def length_ok(output: str) -> llmobs.Score:  # a Score carries a reason
    return llmobs.Score(value=len(output) < 500, reason=f"{len(output)} chars")


result = llmobs.evaluate(
    dataset="faq-v1",
    task=task,
    scorers=[exact, length_ok],
    experiment_name="gpt-5-mini prompt v3",
    metadata={"model": "gpt-5-mini", "prompt": "v3"},
    concurrency=8,
    repetitions=1,
)
print(result.summary())
result.assert_min("exact", 0.8)  # raises AssertionError when the mean is below 0.8
```

- **Dataset.** A name, an id or a `Dataset` object. A `Dataset` runs at the
  version it holds; `dataset_version=` pins another one.
- **Task.** `task(input)` when it takes one positional parameter, or
  `task(input, item)` when it takes two or more (`item` is the `DatasetItem`).
  Async tasks go through `aevaluate`: `await llmobs.aevaluate(...)`.
  `evaluate` refuses a coroutine function.
- **Scorers.** Each scorer's name is its function's name; lambdas, partials
  and duplicate names are refused before any request. Its parameters are
  chosen by name from `input`, `output`, `expected` (the item's
  `expected_output`), `metadata`, `trace_id` and `item`; a `**kwargs`
  parameter receives all of them. A required parameter with another name is
  refused.
- **Scorer return values.** `bool` is a boolean score, `int` / `float` a
  numeric one (finite), `str` a categorical label. `llmobs.Score(value=...,
  reason=..., data_type=..., metadata=...)` adds the reason and metadata;
  `None` means no score. Anything else counts as a scorer error.
- **Concurrency and repetitions.** `concurrency` is 1 to 64 (default 8);
  `repetitions` is 1 to 20 (default 1). Each row records its 0-based
  `repetition`. `experiment_name` is 1 to 200 characters; by default it is the
  dataset name and the UTC time.
- **`summary()`** prints one block per experiment: the run and error counts,
  then per scorer the mean of numeric and boolean values (booleans count as 0
  and 1), or the label counts of a categorical one, and its scorer errors.
- **`assert_min(name, threshold)`** raises `AssertionError` when the mean of a
  numeric or boolean scorer is below `threshold`, or when it has no such
  values. Use it to gate CI.
- **`ExperimentResult`** holds `experiment_id`, `runs` (one `Run` per item and
  repetition, in item order: `dataset_item_id`, `repetition`, `input`,
  `expected_output`, `output`, `error`, `trace_id`, `latency_ns`, `scores`),
  `error_count` and `scorer_errors`.

### What gets recorded

- Each run is one trace. Its root span is named after the experiment, has
  kind `chain` and the item's input, and every span started inside it, the
  instrumentors' included, carries the experiment and dataset item ids with
  environment `experiment`, whatever `init()` set. Experiment traces are
  excluded from the default trace lists.
- A task exception ends that run's span with the error and sets the run's
  `error` to `"<ExceptionName>: <message>"`; a scorer exception gives that run
  no score. Neither aborts the experiment.
- Scores go through the score queue, attached to the run's trace. A run
  without a trace (sampling below 1 drops its root span) keeps its scores in
  the local result only, and `evaluate` warns once.
- Scorers run in your process; the server never scores experiments. A judge
  is a scorer that calls a model (D-J-11).
- Results are uploaded in batches of up to 500 rows or 32 MiB. An output over
  256 KiB (its JSON) is cut to 260,000 bytes and marked as truncated.
- When the monthly units cap pauses score uploads, the scores are dropped
  with a warning; the run continues, and the local result still holds them.
- `summary()` prints the experiment id, not a link to the UI.

### Keys

The key needs the `read`, `write` and `ingest` scopes. `read` covers listing
and reading datasets and experiments, `write` creates datasets, items and
experiments and uploads their results, and `ingest` sends spans and scores.
A key without one of them gets a `403`, which raises `ApiError`.

### Errors

- `ConfigError`: no API key, or `evaluate` before `llmobs.init()`.
- `ValueError` (`TypeError` for a wrong type) for a bad argument, named in the
  message. Nothing is sent.
- `ApiError` for an error answer from the API. Its `status`, `code` and
  `detail` come from the server's problem, `errors` holds the field errors.
  The message is the method, the route template (`/datasets/{dataset}/items`,
  never a name), the status and, when the server sends them, the code and
  the detail. The server never puts item content in a detail.
- Retries: network errors, timeouts, `429` and `5xx` are retried up to three
  times for reads, deletes, result uploads and `finish`. Creates and item
  writes are sent once, so a failure there is never retried silently.
- `evaluate` raises the first `ApiError` when a result upload fails. It stops
  the new runs, finishes the experiment with the error `results upload
  failed`, and raises.

## Instrumentation

`init(instrument=[...])` enables [OpenInference](https://github.com/Arize-ai/openinference)
instrumentors by name, from the fixed registry in the [install table](#install):

```python
llmobs.init(api_key="sk-llmobs-test", instrument=["openai", "anthropic"])
```

An unknown name raises `ConfigError` listing the known names; a name whose
package is missing raises `ConfigError` naming the extra to install (for
example `instrument: 'openai' needs \`pip install llmobs[openai]\``). Nothing
is installed when a name fails: the error surfaces from `init()` itself.

The instrumentors are created with our tracer provider, so their spans nest
under ours and carry the `context()` values. A library you already
instrumented yourself is left alone (one debug line). `shutdown()` uninstruments
everything the SDK enabled. Our provider raises the OTel attribute limit to
10 000, so flattened message attributes are not clipped.

## Existing OpenTelemetry setups

By default `init()` builds its own `TracerProvider` and never registers it
globally: your OTel setup is untouched, and instrumentor spans reach llmobs
because the SDK binds them to our provider. The OTel *context* is always the
global one, so spans nest correctly in both modes.

With `use_global_provider=True`:

- an app's SDK `TracerProvider` already set as global takes our span processor
  (the app keeps its sampler: a `sample_rate` below 1 logs one warning and is
  ignored);
- when nothing is registered yet, our provider becomes the global one;
- any other global provider cannot take processors: the SDK logs one warning
  and keeps its own.

In global mode other libraries create spans on the same provider, so the
default export filter keeps only the LLM-related ones: spans whose
instrumentation scope is `llmobs` or `ai`, or with an attribute key starting
with `llmobs.`, `gen_ai.`, `llm.`, `openinference.`, `ai.`, `traceloop.`,
`langfuse.`, `lmnr.` or `gcp.vertex.agent.`, or with an `input.value` /
`output.value` key. Pass your own callable as `export_filter=` to replace that
rule in either mode: it receives the finished span and returns a boolean; if
it raises, the span is exported.

`llmobs.span_processor()` returns our span processor for apps that build their
own OTel pipeline, and `llmobs.tracer_provider()` returns the provider in use
(a no-op provider before `init()`, when disabled or after `shutdown()`), for
libraries that accept a provider or tracer.

## Masking and `capture_content`

`mask` is a callable applied at capture time to every value the SDK records —
inputs, outputs, metadata values, LLM messages — before serialization:

```python
def redact(value): ...  # return the value with secrets removed


llmobs.init(api_key="sk-llmobs-test", mask=redact)
```

At export time a second pass rewrites spans from *other* instrumentation
scopes (the instrumentors'): for each attribute under a payload key —
`llmobs.input`, `llmobs.output`, `gen_ai.input.messages`,
`gen_ai.output.messages`, `gen_ai.system_instructions`, `input.value`,
`output.value`, the `llm.input_messages.*` / `llm.output_messages.*` /
`gen_ai.prompt.*` / `gen_ai.completion.` prefixes and the other keys of spec
10 — the mask receives the decoded value (JSON-parsed when the string is a
JSON object or array) and the result is serialized back. Only string values
are masked; a non-string value under a payload key is deleted. If `mask`
raises, the value becomes `[llmobs: mask failed]` — the SDK fails closed, and
a batch that cannot be rewritten is dropped, never exported unmasked.

`capture_content=False` records no payloads at all on our spans (no
input/output, no messages, no system instructions; metrics-like attributes —
models, params, token counts, cost — are kept), deletes the payload keys from
instrumentor spans, and configures the OpenInference instrumentors we create
with their hide-inputs/hide-outputs options.

## Threads

The SDK does not patch `threading` or executors (D-F-14). The OTel context —
including `context()` values and the current span — does not cross a thread
boundary by itself, so a span started in a worker thread would be a detached
root without them. Copy the context explicitly:

<!-- test -->
```python
import threading
from contextvars import copy_context

import llmobs

llmobs.init(api_key="sk-llmobs-test")


@llmobs.observe(name="lookup")
def lookup(key: str) -> str:
    return "found"


def work() -> None:
    lookup("pricing")


with llmobs.span("request", input="why?"):
    ctx = copy_context()
    worker = threading.Thread(target=ctx.run, args=(work,))
    worker.start()

worker.join()
llmobs.flush()
```

For thread pools, `ThreadPoolExecutor(..., initializer=...)` cannot capture a
per-task context this way; OTel's
[opentelemetry-instrumentation-threading](https://pypi.org/project/opentelemetry-instrumentation-threading/)
instrumentor patches `threading` for you instead.

## Serverless

`atexit` handlers do not run when a serverless platform freezes or terminates
an invocation. Call `llmobs.flush()` at the end of every invocation (or
`llmobs.shutdown()` when the instance is not reused); otherwise spans and
scores still sitting in the batch queue are lost.

## Known limits

- **Abandoned async generators are never exported.** An async generator that
  is neither exhausted nor `aclose()`d leaves its span unended — an async
  generator cannot be closed from a finalizer. `aclose()` it when you stop
  early.
- **`inspect.isasyncgenfunction` (and `isgeneratorfunction`) is false on a
  wrapped function.** `observe` detects generator functions at decoration
  time; decorating an already-wrapped async generator function traces it as a
  plain call whose output is not recorded.
- **In global mode, the app's own spans get `llmobs.*` attributes.** Our
  processor stamps `llmobs.session.id`, `llmobs.user.id`, `llmobs.tags`,
  `llmobs.metadata.*`, `llmobs.environment` and `llmobs.release` on every span
  of the provider it is attached to, so the app's other exporters see them.
- **`OTEL_EXPORTER_OTLP_HEADERS` is sent to llmobs too.** The OTel HTTP
  exporter merges `OTEL_EXPORTER_OTLP_HEADERS` and
  `OTEL_EXPORTER_OTLP_TRACES_HEADERS` from the environment into its request
  headers; headers meant for another OTLP backend reach the llmobs server as
  well. Unset them if that matters.
- **`KeyboardInterrupt` and `asyncio.CancelledError` are not errors.** Only
  `Exception` gets the error status. A `BaseException` that is not an
  `Exception` — inside a `span()` / `llm_span()` block or raised through
  `@observe` (functions, coroutine functions and both generator wrappers)
  alike — ends the span with the status unset, records no output, and is
  re-raised unchanged.
- **Zero-argument functions and `None`**: a call with no arguments records
  `{}` as input; a `None` return records `null` as output. TypeScript records
  no input for no arguments and no output for `undefined` (return `null` there
  to record a JSON `null`).
- **Integral floats**: Python writes `1.0` as a double; TypeScript has one
  number type and writes an integral value as an int. The server reads both.
- **Large integers**: Python ints beyond ±(2^53−1) inside JSON payloads are
  written as JSON numbers; TypeScript `bigint` becomes a decimal string. As
  metadata values both are JSON strings.
- **Resource attributes**: Python reads `OTEL_RESOURCE_ATTRIBUTES`; TypeScript
  reads only `OTEL_SERVICE_NAME`.
- **Abandoned sync generators** that are garbage-collected half-consumed end
  with status ok (TypeScript has no finalizer and leaves them unexported).
- **Usage counts**: `set_usage` records any non-negative `int`, including
  values beyond 2^53; TypeScript `setUsage` records only safe integers.
- **Threads and serverless**: see [Threads](#threads) and
  [Serverless](#serverless).

## What is never logged

The SDK's own logs and errors never contain payload content (prompts,
completions, attribute values), secrets, API keys or email addresses. Log
lines carry ids, sizes, counts and exception type names only; configuration
errors name the option, never its value; the API key is excluded from `repr`
and serialization of the config. Messages go to the `llmobs` logger, which has
no handlers by default — configure the standard `logging` module to see them.
The missing-key and second-`init()` warnings appear once per process; a full
score buffer warns at most once per minute, and rejected or failed score
batches are logged as they happen.
