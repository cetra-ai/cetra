# llmobs for TypeScript

The `llmobs` package (MIT, ESM only) is the TypeScript SDK of
[llmobs](../../docs/spec/10-sdk.md), an open-source LLM observability platform.
It is a thin layer over OpenTelemetry: tracing goes out as OTLP/HTTP protobuf,
scores and feedback over a small REST surface. Runs on Node ≥ 22.12, Bun, Deno
and edge runtimes (D77) — one `fetch`-based transport everywhere.

## Install

```sh
pnpm add llmobs
```

The instrumentation helpers need their OpenInference packages (optional peers,
installed next to the SDK):

```sh
pnpm add @arizeai/openinference-instrumentation-openai openai
```

## Quick start

```ts
import { flush, init, observe } from "llmobs";
```

<!-- readme:quick-start -->
```ts
init({ apiKey: process.env.LLMOBS_API_KEY });
const searchDocs = observe(({ query }: { query: string }) => `results for ${query}`, {
  kind: "tool",
  name: "search_docs",
});
searchDocs({ query: "pricing" });
await flush();
```

`init()` is once per process: a second call logs one warning and changes
nothing (D-F-15). Without an API key (argument and `LLMOBS_API_KEY` both
unset) it warns once and runs disabled; `LLMOBS_ENABLED=false` silences the
warning.

## Configuration

| Option              | Env                      | Default                           |
| ------------------- | ------------------------ | --------------------------------- |
| `apiKey`            | `LLMOBS_API_KEY`         | unset → one warning, SDK disabled |
| `baseUrl`           | `LLMOBS_BASE_URL`        | `http://localhost:8080`           |
| `environment`       | `LLMOBS_ENVIRONMENT`     | unset                             |
| `release`           | `LLMOBS_RELEASE`         | unset                             |
| `captureContent`    | `LLMOBS_CAPTURE_CONTENT` | `true`                            |
| `sampleRate`        | `LLMOBS_SAMPLE_RATE`     | `1` (trace-id ratio sampler)      |
| `useGlobalProvider` | —                        | `false`                           |
| `instrument`        | —                        | `[]`                              |
| `mask`              | —                        | none                              |
| `exportFilter`      | —                        | none                              |
| `enabled`           | `LLMOBS_ENABLED`         | `true` (no-op mode when `false`)  |

An explicit argument wins over the env var, which wins over the default. Env
values are trimmed; an empty value counts as unset. An invalid value throws a
`ConfigError` naming the option, never its value. `OTEL_SERVICE_NAME` (from
the config's env source) sets `service.name`.

## Wrapper rules (`observe`)

- **Input**: no arguments → none recorded; one argument → that value; more →
  the JSON array of them. Destructuring defaults are invisible to `observe`:
  pass the fields you want recorded explicitly.
- **Output**: the return value, or the resolved value of a returned promise.
  `undefined` records no output; return `null` to record a JSON `null`.
- **Generators**: a generator or async generator function (detected at wrap
  time) is traced as a generator: the span starts at the first `next()`, the
  output is the yielded items — their concatenation when every item is a
  string, else the JSON array. An early `return()` ends the span with status
  ok, not an error.
- **Errors**: a throw records one `exception` event and sets the status to
  error, then re-throws unchanged. Any other function shape — including one
  _returning_ a generator — is traced as a call, and a returned generator
  object is passed through untouched, never consumed.
- `captureInput` / `captureOutput` (default `true`) turn each side off.
- An invalid `kind` throws a `TypeError` at wrap time, before the function
  runs.

## Manual spans and `withLlmSpan`

`withSpan(name, options?, fn)` runs `fn` inside a span; the body's return
value is never recorded — only `span.setInput` / `span.setOutput` /
`span.setMetadata` write attributes.

<!-- readme:with-llm-span -->
```ts
withLlmSpan(
  "chat",
  {
    provider: "openai",
    model: "gpt-4o-mini",
    inputMessages: [{ role: "user", content: "hi" }],
    params: { temperature: 0.2 },
  },
  (span) => {
    span.setOutputMessages([{ role: "assistant", content: "hello" }]);
    span.setUsage({ inputTokens: 12, outputTokens: 3 });
  },
);
```

`withLlmSpan` writes the GenAI attributes (`gen_ai.*`), `llmobs.span.kind =
"llm"` and, through the handle, the response model, usage, cost
(`setCost(totalUsd)`) and time-to-first-token (`recordFirstToken()`).
Messages may be the canonical `{ role, parts }` format or the short
`{ role, content: "<text>" }` form, which becomes one text part.

## Context and baggage

<!-- readme:with-context -->
```ts
const turn = observe(({ question }: { question: string }) => `answered: ${question}`, {
  name: "turn",
});
withContext({ sessionId: "chat-42", userId: "u-17", tags: ["beta"] }, () =>
  turn({ question: "why?" }),
);
```

The values live in the OTel context, so they follow `AsyncLocalStorage` into
promises and concurrent flows; nested blocks merge (inner session/user win,
tags union, metadata merges by key). They are stamped on every span of the
SDK's provider — session and user ids, tags, metadata, `environment` and
`release`. Metadata is recorded to be read back by the server: keep personal
data out of it, or mask it (below). `propagate: true` also sets the session
and user ids as W3C baggage entries for downstream services.

## Scores

<!-- readme:score -->
```ts
// The returned id is yours to keep: pass it back as `scoreId` to update.
const id = score({ name: "helpful", value: 0.8, sessionId: "chat-42", reason: "human review" });
return id;
```

`scores` are queued in the background and posted to `/api/v1/scores` in
batches (up to 10 000 buffered), with retries on 429/5xx and network errors.
The target is a `traceId` + `spanId` pair, a `traceId`, or a `sessionId`;
`currentTraceId()` / `currentSpanId()` give the ids of the running span.
Values map: `boolean` → boolean, `number` → numeric, `string` → categorical
label (or `dataType: "text"`). `user_id` comes from the active
`withContext` unless passed.

## Datasets and experiments

`new llmobs.Client()` is the REST client for datasets and experiments. It reads
`apiKey` and `baseUrl` the way `init()` does (argument, then
`LLMOBS_API_KEY` / `LLMOBS_BASE_URL`, then the default), and throws
`ConfigError` without a key. Unlike tracing and scores, its calls reject on
failure (see [Errors](#errors)).

### Datasets

```ts
import { Client } from "llmobs";

const client = new Client(); // the key and URL come from the environment

let dataset = await client.datasets.create("faq-v1", { description: "FAQ questions" });
dataset = await dataset.addItems([
  {
    input: { q: "How do I cancel?" },
    expectedOutput: { a: "Settings > Billing > Cancel" },
    metadata: { topic: "billing" },
  },
]);
dataset = await client.datasets.get("faq-v1", { version: 1 }); // the items of version 1
console.log((await client.datasets.list()).map((info) => info.name));
await client.datasets.delete("faq-v1"); // the dataset and its items
```

- An item is `input` (any JSON value, required), `expectedOutput` (optional)
  and `metadata` (an object, default `{}`). Any other key throws a `TypeError`
  before a request is sent.
- `addItems` sends batches of 1,000 items, and each batch makes a new
  version. Versions are integers: a new dataset is at version 0 and has no
  items. `addItems` resolves to the dataset at its latest version. If a later
  batch fails, the earlier batches are already stored (each made a version),
  and the promise rejects.
- `get(nameOrId, { version })` reads a version with all its items; without
  `version` it reads the current one.
- `create(name, { inputSchema, expectedSchema })` takes JSON Schemas (Draft
  2020-12). The server checks every item written afterwards against them; a
  refused item rejects with an `ApiError`, and its `errors` array names the
  failing fields.

### Experiments

`evaluate` runs your task on every item of a dataset version, scores each
run, and stores the results on the server. It needs `init()` first.

```ts
import { evaluate, init, type Score } from "llmobs";

init(); // the key comes from LLMOBS_API_KEY

function exact({ output, expected }: { output: unknown; expected: unknown }): boolean {
  return String(output).trim() === (expected as { a: string }).a.trim();
}

function lengthOk({ output }: { output: unknown }): Score {
  const text = String(output);
  return { value: text.length < 500, reason: `${text.length} chars` };
}

const result = await evaluate({
  dataset: "faq-v1",
  task: (input) => myApp.answer((input as { q: string }).q),
  scorers: [exact, lengthOk],
  experimentName: "gpt-5-mini prompt v3",
  metadata: { model: "gpt-5-mini", prompt: "v3" },
  concurrency: 8,
  repetitions: 1,
});
console.log(result.summary());
result.assertMin("exact", 0.8); // throws when the mean is below 0.8
```

- **Dataset.** A name, an id or a `Dataset` object. A `Dataset` runs at the
  version it holds; `datasetVersion` pins another one. A `Dataset` at
  version 0 has no items, so no version is sent and the server uses its
  current version.
- **Task.** `task(input, item)` receives `item.input` and the `DatasetItem`.
  It may return a promise.
- **Scorers.** A scorer's name is its function's name: a named function or a
  `const` works, while an anonymous function or a duplicate name is refused
  with a `TypeError` before any request. It receives one object with `input`,
  `output`, `expected` (the item's `expectedOutput`), `metadata`, `traceId`
  and `item`.
- **Scorer return values.** `boolean` is a boolean score, `number` a numeric
  one (finite), `string` a categorical label. A `Score`
  (`{ value, reason, dataType, metadata }`) adds the reason and metadata;
  `undefined` or `null` means no score. Anything else counts as a scorer
  error.
- **Concurrency and repetitions.** `concurrency` is 1 to 64 (default 8);
  `repetitions` is 1 to 20 (default 1). Each run records its 0-based
  `repetition`. `experimentName` is 1 to 200 characters; by default it is the
  dataset name and the UTC time.
- **Result.** `summary()` prints one line per scorer with its mean and count,
  and the experiment id. `errorCount` counts runs whose task threw;
  `assertMin(name, threshold)` throws when a score's mean is below the
  threshold, which is what a CI gate needs.

### What gets recorded

- Each run is one trace: its root span (kind `chain`, named after the
  experiment) carries the input and the task's output, and every span the
  task starts carries the experiment id, the dataset item id and the
  environment `experiment`. The default trace lists leave these traces out.
- A score needs its trace, so with `sampleRate` below 1 some runs have no
  trace and no score. Scores are queued and posted in batches, and
  `await flush()` waits for them.
- Nothing is scored on the server: a judge runs inside a scorer.
- Results are uploaded in batches; each upload is retried on transient
  failures.

### Keys

The API key needs the `ingest`, `read` and `write` scopes.

### Errors

`ConfigError` names the option that is missing or invalid, never its value.
`ApiError` carries `status`, `code`, `detail` and `errors` (the field errors of
a refused dataset item). A task or scorer that throws does not stop the run:
the error is recorded on that run, and `errorCount` and the summary count it.

## Instrumentation (OpenAI, Anthropic, LangChain)

`init({ instrument: [...] })` takes instrumentation objects (anything with
`setTracerProvider`, `enable` and `disable`). The subpath helpers build the
OpenInference instrumentation and patch the module by hand, so ESM works
without loader flags:

```ts
import { openai } from "llmobs/openai";
import OpenAI from "openai";

init({ apiKey, instrument: [openai(OpenAI)] });
```

The same shape exists as `llmobs/anthropic` (takes the `Anthropic` class from
`@anthropic-ai/sdk`) and `llmobs/langchain` (takes the LangChain global from
`@langchain/core`). The OpenInference packages are optional peers. Pass the
helpers' `captureContent` the same value as `init`'s.

## Vercel AI SDK

The AI SDK sends spans to the tracer it is given; `aiTelemetry()` hands out
one from the llm provider (scope `ai`) plus your context values as telemetry
metadata.

<!-- readme:ai-telemetry -->
```ts
// Pass `telemetry` as `experimental_telemetry` to the Vercel AI SDK; the
// tracer it hands out is the llmobs one, so AI spans land in llmobs.
const telemetry = aiTelemetry({ sessionId: "chat-42", functionId: "chat" });
telemetry.tracer?.startSpan("ai.chat").end();
```

Full usage with the AI SDK itself:

```ts
import { generateText } from "ai";

const result = await generateText({
  model: myModel,
  experimental_telemetry: aiTelemetry({ sessionId: "chat-42", functionId: "chat" }),
});
```

## Existing OpenTelemetry setups

By default the SDK keeps its own `TracerProvider` and never registers it
globally, so it cannot hijack an app's OTel setup. The OTel _context_ is
always the global one, so instrumentor spans nest under ours.

- `useGlobalProvider: true` registers our provider as the global one when
  none is set; OTel JS 2.x providers cannot take processors after
  construction, so with an app provider already registered the SDK logs one
  warning and stays private.
- `spanProcessor()` returns our span processor, for apps that build their own
  OTel pipeline and add it to their provider.
- `exportFilter(span)` replaces the default export rule (which, in global
  mode, keeps spans whose scope is `llmobs` or `ai` or that carry known
  LLM attribute prefixes).
- `tracerProvider()` returns the provider in use.

## Masking and `captureContent`

`mask(value)` runs on every value the SDK records (inputs, outputs, metadata,
LLM messages) before serialization, and at export time on payload keys of
other spans (instrumentor spans included). It gets the decoded value and must
return a new one; a string stays a string, anything else is serialized back.
If it throws, the value becomes `"[llmobs: mask failed]"` (fail closed).

`captureContent: false` records no payload attributes on our spans and
deletes the payload keys from other spans; token counts, models and costs
still flow.

```ts
init({
  apiKey,
  mask: (value) => (typeof value === "string" ? value.replaceAll("sk-live-", "[key]") : value),
});
```

## Edge runtimes and serverless

One transport everywhere: OTLP protobuf over `fetch`, gzipped through
`CompressionStream` when the runtime has it. On Cloudflare Workers, enable
Node compatibility (the SDK uses `AsyncLocalStorage` for context):

```toml
compatibility_flags = ["nodejs_compat"]
```

In serverless (Workers, Lambda, edge functions), flush explicitly — the
`beforeExit` hook only fires when the event loop empties, never on a frozen
or reused invocation:

```ts
export default {
  async fetch(request, env, ctx) {
    const response = handle(request);
    ctx.waitUntil(flush());
    return response;
  },
};
```

## Browser feedback

The `llmobs/browser` subpath imports nothing from OpenTelemetry; it posts an
end-user feedback score with a public key (`pk-llmobs-…`) and never throws:

```ts
import { feedback } from "llmobs/browser";

const { ok } = await feedback({
  baseUrl: "https://llmobs.example.org",
  publicKey: "pk-llmobs-…",
  traceId, // 32 hex chars, from traceparent or your app
  value: 1,
});
```

## Known limits

- **Zero-argument functions and `undefined`**: no arguments record no input
  (Python records `{}`); an `undefined` return records no output (Python
  records `null`). Return `null` to record a JSON `null`.
- **Integral floats**: JavaScript has one number type, and OTel JS encodes an
  integral number as an int attribute; Python writes a double for `1.0`. The
  server reads both. `bigint` becomes a decimal string in payloads.
- **Resource attributes**: only `OTEL_SERVICE_NAME` is read (from the
  config's env source); `OTEL_RESOURCE_ATTRIBUTES` is not (Python reads it).
- **Usage counts**: `setUsage` records only safe integers
  (`Number.isSafeInteger`); a larger value, a non-integer or a negative one is
  skipped with a debug line. Python's `set_usage` records any non-negative
  `int`, including values beyond 2^53.
- **Abandoned generators**: a half-consumed generator is left unexported
  (TypeScript has no finalizer); Python's garbage collector ends it with
  status ok.
- **Cloudflare Workers** need `nodejs_compat` (above); flush through
  `ctx.waitUntil(flush())` before the invocation is frozen.
- **Serverless**: call `await flush()` (or `shutdown()`) at the end of each
  invocation; the SDK installs no signal handlers and `beforeExit` does not
  fire on `process.exit()`.

## What is never logged

Logs and warnings never contain payloads (prompts, completions, attribute
values), API keys or email addresses — ids, sizes and counts only. An invalid
score logs `llmobs: score not queued: <reason>` with the reason, never the
value. The API key never appears in errors or serialized config.
