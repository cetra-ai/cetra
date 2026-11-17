# cetra for TypeScript

The `cetra` package (MIT, ESM only) is the TypeScript SDK of
[cetra](../../docs/spec/10-sdk.md), an open-source LLM observability platform.
It is a thin layer over OpenTelemetry: tracing goes out as OTLP/HTTP protobuf,
scores and feedback over a small REST surface. Runs on Node ≥ 22.12, Bun, Deno
and edge runtimes (D77) — one `fetch`-based transport everywhere.

## Install

```sh
pnpm add @cetra-ai/sdk
```

The instrumentation helpers need their OpenInference packages (optional peers,
installed next to the SDK):

```sh
pnpm add @arizeai/openinference-instrumentation-openai openai
```

## Quick start

```ts
import { flush, init, observe } from "@cetra-ai/sdk";
```

<!-- example:quick-start -->
```ts
init({ apiKey: process.env.CETRA_API_KEY });
const searchDocs = observe(({ query }: { query: string }) => `results for ${query}`, {
  kind: "tool",
  name: "search_docs",
});
searchDocs({ query: "pricing" });
await flush();
```

`init()` is once per process: a second call logs one warning and changes
nothing (D-F-15). Without an API key (argument and `CETRA_API_KEY` both
unset) it warns once and runs disabled; `CETRA_ENABLED=false` silences the
warning.

## Documentation

The full guide and reference are at https://cetra.sh/docs/reference/typescript/ (configuration,
tracing, context, scores, datasets and experiments, OpenTelemetry, runtimes, the API).
