# M1 spike: TypeScript host stamp (D-IN-18)

Question: can `openai(OpenAI)` stamp the client's base-URL host on the LLM span as `server.address` through a context key read by a span processor?

## What was run

A throwaway vitest (not committed; copy at `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/spike-host.test.ts`) in `sdks/typescript/src/`, built like `instrument.test.ts`: `startMemory({ instrument: [openai(OpenAI)] })`, a client with a custom `fetch` returning a canned chat completion (or an SSE stream), then:

- `OpenAI.Chat.Completions.prototype.create` wrapped outermost, after OpenInference's patch: it runs the original inside `context.with(context.active().setValue(KEY, new URL(this._client.baseURL).hostname), ...)`;
- `runtime.processor.onStart` patched to read `parentContext.getValue(KEY)` and, when `span.attributes["server.address"]` is undefined, write a marker attribute (`spike.stamped.address`) so the stamp can be told apart from OpenInference's own value.

Run: `pnpm exec vitest run src/spike-host.test.ts` (4 cases, all passed). The file was removed afterwards; nothing tracked changed.

## Versions

- openai 7.57.0
- @arizeai/openinference-instrumentation-openai 4.5.0
- @opentelemetry/context-async-hooks 2.12.1 (cetra's `AsyncLocalStorageContextManager`), Node 26.13.0

## Results

| Case | `llm.provider` | `server.address` / `server.port` (from OpenInference) | Context stamp reached `onStart` |
|---|---|---|---|
| novita `https://api.novita.ai/openai`, non-stream | absent (host not in its table) | `api.novita.ai` / 443 | yes (`api.novita.ai`) |
| novita, `stream: true` | absent | `api.novita.ai` / 443 | yes |
| groq `https://api.groq.com/openai/v1`, non-stream | `groq` | `api.groq.com` / 443 | yes |
| groq, `stream: true` | `groq` | `api.groq.com` / 443 | yes |

`llm.system` is `openai` in all cases.

## Findings

1. **OpenInference JS 4.5.0 already writes `server.address` and `server.port`** for every OpenAI-compatible client, whether or not it knows the provider (`getServerAttributes(this)` in `dist/src/instrumentation.js`; it reads `baseURL` from the resource or its `_client`, hostname only, port defaulted from the scheme). It also writes `llm.provider` from a host-suffix table (`HOST_SUFFIX_TO_PROVIDER`: groq yes, novita no). So for the TypeScript SDK the stamp is not needed to get a host on the span; what is missing for novita is only a provider name, which cetra must derive from `server.address` itself (the same host map as Python's).
2. The stamp mechanism itself works on Node: `AsyncLocalStorageContextManager` carries the key through `context.with`, and the processor sees it in `parentContext` for the LLM span, with and without streaming (the span starts inside the wrapped call, before any chunk is read).
3. In `onStart`, `span.attributes["server.address"]` read as undefined even though OpenInference passes it in the `startSpan` attributes (`OITracer` apparently applies attributes after `startSpan` returns, so the processor runs first). An "if absent" check in `onStart` therefore always fires; OpenInference's later write wins on the same key, so the result is the same value, but the check cannot be relied on. Either set the stamp unconditionally (identical value) or do the work in `onEnd`/at export time from `server.address`.
4. Context manager outside Node: `Runtime.create` always registers `AsyncLocalStorageContextManager` (`registerContextManager` in `runtime.ts`) and keeps one already registered. Cloudflare Workers need `nodejs_compat` (documented in `reference/typescript/runtimes.mdx`); Deno and Bun provide `AsyncLocalStorage`. The browser entry (`src/browser.ts`) imports no OTel and has no runtime, so the OpenAI helper does not run in a browser at all. If no working context manager is registered (the API falls back to its no-op manager: `context.with` just calls the function and `context.active()` is always the root), the stamp is silently lost, but so is span nesting under `observe()` spans, so this is an already-unsupported setup, not a new failure mode.

## Verdict

Works on Node (and on any runtime with `AsyncLocalStorage`, same requirement as cetra's span nesting); does not exist in the browser. But it is unnecessary for `server.address`: OpenInference JS 4.5.0 sets it itself. Recommended: do not wrap anything in the TS helper for the host; resolve the provider name from `server.address` (and `llm.provider` when OpenInference set it) in cetra's processor or at ingestion.

If D-IN-18 still wants the stamp (for example for a host OpenInference misses, such as a client with a non-string `baseURL`), the exact methods to wrap, all on the OpenAI classes' prototypes after `manuallyInstrument(module)` (each is the one OpenInference patches, and the wrapper reads `this._client.baseURL`):

- `OpenAI.Chat.Completions.prototype.create`
- `OpenAI.Responses.prototype.create`
- `OpenAI.Embeddings.prototype.create`

OpenInference 4.5.0 also patches `OpenAI.Completions.prototype.create` (legacy completions) and `OpenAI.Decisions.prototype.create`; wrap them too if those spans should carry the stamp. Caveat: `disable()` unwraps OpenInference's patch with `_unwrap`, which would not restore a wrapper placed on top; the wrapper must undo itself on disable (the existing "creates no span once the runtime has shut down" test pattern covers this).
