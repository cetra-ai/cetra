# M8 spike: setup-only languages (Go, Ruby, Rust, C++)

Status: spike, not a plan. Nothing here changes the design or the decisions. Five items need the user (§ 7). Written 2026-11-24, install clock about 13:00 UTC, so the 7-day cutoff is **2026-11-17 13:00 UTC**. Every pick below was published on or before 2026-11-16; the newer releases that were excluded are listed in § 2.

Evidence root (E): `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m8/setup-only`. Sources are under `E/go/`, `E/rb/`, `E/rs/`, `E/cpp/`; downloads under `E/dl/`; registry and GitHub metadata JSON under `E/notes/`. Paths below are relative to the package root of each source, given as `file:line`.

Scope, per the brief: read package sources and docs only. **No code was run, nothing was built, no loopback namespace was used, and no fake provider or OTLP sink was started** (rule 4 does not apply). Each fact is tagged:

- **src**: read in a downloaded package source (file:line given);
- **docs**: read from a README or docs page in that source (file given);
- **gh**: read from the GitHub API or a registry metadata record, not from source.

Nothing is tagged **run**.

## 1. Summary

1. **Go, Genkit Go 1.14.0** is a real native GenAI route, but it is opt-in: `tracing.SetInstrumentation(otelgenai.New(...))`. Its spans carry `gen_ai.*` keys, and content is off by default. Page possible.
2. **Go, openai-go**: the current major (v3, 3.100.0) has no telemetry of its own, and no route covers it. OpenInference Go's middleware works only with **openai-go v1** (v1.12.0, 2025). OpenLIT Go wraps only its own HTTP client, not openai-go. **Needs a decision (§ 7.3).**
3. **Go, langchaingo 0.1.15** has no OpenTelemetry code and no OpenInference or OpenLIT instrumentation. Only a `callbacks.Handler` interface exists. **Page impossible as designed (§ 7.2).**
4. **Go, OpenLIT Go** is an untagged module. Its newest commit before the cutoff is `6b97612e` (2026-10-13), usable only as a pseudo-version. It writes `gen_ai.system` (not `gen_ai.provider.name`) and **no output tokens**. Content is on by default.
5. **Ruby, ruby_llm 2.2.2** has a built-in, first-party OTel module (`RubyLLM::OpenTelemetry.enable`) that writes `gen_ai.*`. Content is off by default and enabled in code only. A third-party gem, `opentelemetry-instrumentation-ruby_llm` 0.7.1 (thoughtbot), does the same job by patching RubyLLM. `ruby_llm-opentelemetry` 0.1.0 is a placeholder. **Which one is "its OTel gem" is for the user (§ 7.5).**
6. **Rust, rig-core 0.46.0** writes `gen_ai.*` on `tracing` spans (`chat`, `generate_content`, `embeddings`, …) and has no OTel exporter. The app must bridge with `tracing-opentelemetry`. Content is opt-in per request (`record_content_telemetry(true)`). Page possible.
7. **Rust, Swiftide 0.34.0** writes **no `gen_ai.*` attributes at all**. Its spans are agent, step, tool and hook spans with no model, provider, token or I/O fields. Its Langfuse path is Langfuse's own JSON API, not OTLP. The design's "native GenAI" is wrong for Swiftide. **Needs a decision (§ 7.6).**
8. **C++, "AI SDK C++"**: the only project with that exact name in its README is ClickHouse's `ai-sdk-cpp`. It has **no OpenTelemetry code at all** (a grep of the whole tree finds none). Its only tracing is a Langfuse JSON client. The other two projects with that name have no OTel either. **The design's "its OTel export" does not exist in the project it names (§ 7.1).**

Setup-only pages can still be written for rig and Genkit. The others wait for the user's answers in § 7.

## 2. Versions (the 7-day rule)

| Item | Pinned | Published (UTC) | Source and integrity |
|---|---|---|---|
| Genkit Go | `github.com/firebase/genkit/go` **v1.14.0** | 2026-10-19T19:24:58Z (proxy `.info`) | zip `E/dl/genkit-go-v1.14.0.zip`, sha256 `879211018df944120f1ff51163062d8a5e3b7ac2ab6d7ccfb6d55296f45cb90c`; origin commit `6b34d5fe691002a217e11228b85979f58664288b`, tag `go/v1.14.0` |
| openai-go (current major) | `github.com/openai/openai-go/v3` **v3.100.0** | 2026-11-16T22:37:15Z | zip `E/dl/openai-go-v3.100.0.zip`, sha256 `883c49b4ebdaa13401857ca2f4f6a62c44bdc691b6c88514d2eadc3c3b9d432b` |
| openai-go (v1, the only one OpenInference supports) | `github.com/openai/openai-go` **v1.12.0** | 2025-07-30T12:45:47Z | zip `E/dl/openai-go-v1.12.0.zip`, sha256 `a3ce4967e9edb2bea4a1b6e5044180595e7fe7c04054404f9d14b92d90bb2354` |
| OpenInference Go, OpenAI middleware | `github.com/Arize-ai/openinference/go/openinference-instrumentation-openai-go` **v0.1.2** | 2026-11-04T20:36:09Z | zip `E/dl/oi-openai-go-v0.1.2.zip`, sha256 `a2aed617b05eefc16394caf111831a37f3048edc43046ef5b4189f45ea058db8` |
| OpenInference Go, core | `github.com/Arize-ai/openinference/go/openinference-instrumentation` **v0.1.2** | 2026-09-15T04:50:23Z | zip `E/dl/oi-instrumentation-v0.1.2.zip`, sha256 `4a71eefc8f9c7893cfadc0a4d8bf74e02059698018cf1044afe59eee38ed4f91` |
| OpenLIT Go | `github.com/openlit/openlit/sdk/go` **pseudo-version** `v0.0.0-20261013100950-6b97612e58d1` (commit `6b97612e58d1e9253ac0d0ab89230da1008638de`, committed 2026-10-13T10:09:50Z) | 2026-10-13 (commit time) | repo tarball `E/dl/openlit-6b97612.tgz` (109,305,127 bytes), sha256 `6bba15da890376d693642a73630d9ebfdbda04765f379da7c7488d51b15511e9`; `sdk/go/version.go` says `0.2.0`; **no tag exists** for this module |
| langchaingo | `github.com/tmc/langchaingo` **v0.1.15** | 2026-10-05T05:25:52Z | zip `E/dl/langchaingo-v0.1.15.zip`, sha256 `2a94c52cfcdc98475593ade26da4a4f82ef08381ba405c11639b527d90e919ab` |
| OTLP/HTTP exporter for Go (the one Genkit 1.14.0 and its `otel` v1.46.0 use) | `go.opentelemetry.io/otel/exporters/otlp/otlptrace/otlptracehttp` **v1.46.0** | 2026-08-25T16:52:31Z | zip `E/dl/otlptracehttp-v1.46.0.zip`, sha256 `f87ca4401bb735a8f70c279e9fa07977d795f49c95d2d7518488614732d5ef55` |
| ruby_llm | `ruby_llm` **2.2.2** | 2026-11-15T03:54:56Z | gem `E/dl/ruby_llm-2.2.2.gem`, sha256 `e08da09150bc11e15b294a7628b8295e0813d7f611ca97f74dcb1286ba7cdc79` |
| ruby_llm OTel (thoughtbot) | `opentelemetry-instrumentation-ruby_llm` **0.7.1** | 2026-07-31T19:17:50Z | gem `E/dl/opentelemetry-instrumentation-ruby_llm-0.7.1.gem`, sha256 `80e58f37a3d827c94aaa6a250f421333dc78ddd36a4870016f523d2fbc4aa608` |
| ruby_llm OTel placeholder | `ruby_llm-opentelemetry` 0.1.0 | 2026-08-08T18:37:08Z | gem `E/dl/ruby_llm-opentelemetry-0.1.0.gem`, sha256 `d0cb8504c7a6b8d6e0376760dcd4d4b203e136bd7218cc1ae4746a69b926f124`. Its README says "This is a placeholder release" (not usable) |
| Ruby OTLP exporter | `opentelemetry-exporter-otlp` **0.37.0** | 2026-09-29 | gem `E/dl/opentelemetry-exporter-otlp-0.37.0.gem`, sha256 `edc8bda657dce5f4d527ac735f75d44043bb5924de6df0e3f01260108aa76136` |
| Ruby SDK | `opentelemetry-sdk` **1.13.1** | 2026-09-16 | gem `E/dl/opentelemetry-sdk-1.13.1.gem`, sha256 `91c9cc315ac7ef30243549eeff8b27cd0d39bd02bdf9c9ae481ab96a1cec6754` |
| rig | `rig-core` **0.46.0** (crate `rig-core`, library `rig`; repo `0xPlaygrounds/rig`) | 2026-10-31T06:51:07Z | crate `E/dl/rig-core-0.46.0.crate`, sha256 `c46316e31b3e80a050ad35790f1b612b4dfdb46982293cc78641caf4271f1b05` |
| Swiftide | `swiftide` **0.34.0** | 2026-11-06T14:47:34Z | crate `E/dl/swiftide-0.34.0.crate`, sha256 `17e033a248b1b09673d609ba77b2bdbd86e83d5cf20b3d8e1246f62543697931` |
| Swiftide agents | `swiftide-agents` **0.34.0** | 2026-11-06T14:47:34Z | crate `E/dl/swiftide-agents-0.34.0.crate`, sha256 `d1492bdffb2e719ee1d72dcf6bf1bb845aa797653d744a9a3445dd8c721a578d` |
| Swiftide integrations | `swiftide-integrations` **0.34.0** | 2026-11-06T14:47:34Z | crate `E/dl/swiftide-integrations-0.34.0.crate`, sha256 `546a677e886ec5ae9d235989835f241fd4352efbdf0466cb2f1cbf73db1fb8c7` |
| Swiftide Langfuse | `swiftide-langfuse` **0.34.0** | 2026-11-06T14:47:34Z | crate `E/dl/swiftide-langfuse-0.34.0.crate`, sha256 `2e5e58fb8a1d1532bd65c864db28bbe1937163e87b830ff9840c588f27fbe522` |
| Rust OTLP exporter | `opentelemetry-otlp` **0.33.1** | 2026-10-09 | crate `E/dl/opentelemetry-otlp-0.33.1.crate`, sha256 `0756af24841e367128ef31c831999937b8b5c5197d47914590d8f4e2e88c2e1a` |
| Rust OTel SDK | `opentelemetry_sdk` **0.33.1** | 2026-10-09 | crate `E/dl/opentelemetry_sdk-0.33.1.crate`, sha256 `372a912db2974d1990207006fd0e3aa0ef3fd4ad3be43c65cfaadf2206866061` |
| Rust `tracing` bridge | `tracing-opentelemetry` **0.34.0** (depends on `opentelemetry` 0.33.0, `Cargo.toml:115-116`) | 2026-09-23 | crate `E/dl/tracing-opentelemetry-0.34.0.crate`, sha256 `0a904802a1b902f43638b677ff2a650847e3b4404101b6c586d648e8c1e3e8fe` |
| C++ "AI SDK C++" (ClickHouse) | `ClickHouse/ai-sdk-cpp`, commit **`9d8cd18200a3`** (the default branch head, `main`; committed 2026-08-19T21:08:40Z) | 2026-08-19 | tarball `E/dl/clickhouse-ai-sdk-cpp-9d8cd18.tar.gz` (409,085 bytes), sha256 `00b973f749cbc946262f148a23f44deaf1f2c0fef5aace6db83b96ea109ad52a`. **The repo has no releases and no tags** (`gh api …/releases` and `…/tags` return nothing), so the rule's "newest release" cannot apply; the head commit is used and is older than the cutoff |

Excluded, because published after the cutoff: genkit go v1.14.x later than v1.14.0 (none); openai-go v3.101.0 (2026-11-17T18:14Z) and later; ruby_llm 2.2.3 (2026-11-18); rig-core 0.47.0 (2026-11-20); swiftide none later.

Metadata JSON read for these dates is in `E/notes/` (proxy `.info` files were read live and not saved; registry `versions` lists saved as `*.versions.json`, `rl-versions.json`, `v-*.json`).

## 3. Method

- Versions: Go proxy `@v/list` and `@v/<v>.info` (`E/notes/gover.sh`); RubyGems `api/v1/versions` and `api/v1/gems`; crates.io `api/v1/crates/<name>/versions`; GitHub via `gh api` for tags, releases, trees, commits and file contents.
- Downloads: proxy zips, `rubygems.org/downloads`, `static.crates.io`, and a `codeload.github.com` source tarball. Each file was checked with `sha256sum` and extracted under `E/go`, `E/rb`, `E/rs`, `E/cpp` for reading only. Nothing was built, installed or executed. The OpenLIT tarball is the whole repo (109 MB); only `sdk/go` was extracted.
- Searches over sources used `grep -rn`; the repo's `AGENTS.md` and this doc were read first.

## 4. Go

### 4.1 Genkit Go v1.14.0 (native GenAI, opt-in)

**Route.** `github.com/firebase/genkit/go/plugins/otelgenai`, enabled with `tracing.SetInstrumentation(otelgenai.New(&otelgenai.Options{...}))` (`plugins/otelgenai/README.md`, "Usage"; `core/tracing/instrumentation.go:252`, `SetInstrumentation(providers ...Instrumentation)`). Genkit's default is its own `genkit:*` OTel spans (`core/tracing/otel_instrumentation.go`, "implicit default"; `plugins/otelgenai/README.md` says `OTelInstrumentation` must be listed to keep them). The app owns the SDK (README). The tracer is resolved once in `New` (`plugins/otelgenai/instrumentation.go:166-217`), so set the global tracer provider **before** calling `otelgenai.New`.

**Span names and kinds** (`plugins/otelgenai/spans.go`):
- model action → `chat <model>`, kind CLIENT (`spans.go:143`). `<model>` is the name without its provider prefix (`genai.SplitModelName`, `plugins/otelgenai/internal/genai/attributes.go`). Scope `genkit-genai` (`instrumentation.go:173-176`).
- tool action → `execute_tool <name>`, INTERNAL, **only** with `EmitToolSpans: true` (`spans.go:149-155`; `instrumentation.go:206-208`).
- any other action → its own name, INTERNAL, with `genkit.action.type` (`spans.go:160-165`).

**Attributes on the chat span** (`spans.go:125-141`; `attributes.go:46-118`; `internal/genai/attributes.go`):
- `gen_ai.operation.name` = `chat`; `gen_ai.provider.name` from the prefix via `DeriveProviderName` (`internal/genai/attributes.go:140-156`): `openai` → `openai`, `googleai` → `gcp.gemini`, `vertexai` → `gcp.vertex_ai`, `anthropic` → `anthropic`, anything else lower-cased. `gen_ai.request.model`.
- request config: `gen_ai.request.temperature`, `.top_p`, `.top_k`, `.max_tokens`, `.stop_sequences`, `.frequency_penalty`, `.presence_penalty`, `.seed`, `.choice.count` (omitted when 1), `gen_ai.output.type` (`attributes.go:46-85`).
- at end: `gen_ai.response.finish_reasons` (`tool_calls` when the turn requests a tool, `attributes.go:87-119`), `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens` (always set when usage exists), `gen_ai.usage.reasoning.output_tokens` and `gen_ai.usage.cache_read.input_tokens` only when non-zero (`attributes.go:87-108`).
- **No `gen_ai.response.model`** is written (`attributes.go:87-108`). The model comes from the request only.
- labels (`tracing.WithTelemetryLabels`) become raw span attributes (`spans.go:103-111`).

**Content.** Default `NoContent` (`instrumentation.go:81-84`). The env var `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT` (`CaptureContentEnvVar`, values `NO_CONTENT`, `SPAN_ONLY`, `EVENT_ONLY`, `SPAN_AND_EVENT`) is read **only** when `Options.ContentCapturingMode` is empty (`instrumentation.go:169-172`, `219-226`). With `SPAN_ONLY`, `gen_ai.input.messages`, `gen_ai.output.messages` and `gen_ai.system_instructions` are JSON strings on the chat span (`content.go:33-60`). With `EVENT_ONLY` or `SPAN_AND_EVENT` they go to a log event `gen_ai.client.inference.operation.details` (`content.go:61-…`, `attributes.go` constant `OperationDetailsEvent`), which cetra's OTLP intake receives as a log record. `CaptureActionIO` (default false) adds `genkit.input`/`genkit.output` (`instrumentation.go:108-112`).

**Metrics.** `gen_ai.client.token.usage` and `gen_ai.client.operation.duration` are also emitted by default (`instrumentation.go:114-123`, `DisableMetrics`); cetra does not read metrics for traces.

**Minimal setup** (by source, **not compiled or run**; the OTLP exporter lives outside Genkit's module):

```go
ctx := context.Background()
exp, err := otlptracehttp.New(ctx) // reads OTEL_EXPORTER_OTLP_ENDPOINT and _HEADERS
if err != nil { log.Fatal(err) }
tp := sdktrace.NewTracerProvider(sdktrace.WithBatcher(exp))
otel.SetTracerProvider(tp)
defer tp.Shutdown(ctx)
tracing.SetInstrumentation(otelgenai.New(&otelgenai.Options{
	ContentCapturingMode: otelgenai.SpanOnly, // or leave unset and set the env var
}))
plugin := &oai.OpenAI{APIKey: os.Getenv("OPENAI_API_KEY")}   // github.com/firebase/genkit/go/plugins/compat_oai/openai
g := genkit.Init(ctx, genkit.WithPlugins(plugin), genkit.WithDefaultModel("openai/gpt-5.4"))
resp, err := genkit.Generate(ctx, g, ai.WithPrompt("…"))
_ = resp.Text() // ai/generate.go:2161
```

- OTLP env (`otlptracehttp` v1.46.0, `internal/envconfig/envconfig.go:159-184`): `OTEL_EXPORTER_OTLP_ENDPOINT` (base URL, `/v1/traces` appended) and `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20<key>`. Values are `url.PathUnescape`d, so `%20` is correct.
- OpenAI plugin: `plugins/compat_oai/openai/openai.go:276` (`type OpenAI struct { APIKey string; Opts []option.RequestOption }`), with the README's `genkit.Init(...WithPlugins(plugin), WithDefaultModel("openai/gpt-5.4"))` (`plugins/compat_oai/openai/README.md`). `Init` returns `*Genkit` without an error (`genkit/genkit.go:307`). `DefineModel` on the compat plugin is deprecated (`compat_oai.go` deprecation note); the page should not use it.
- Pitfall: `genkit.Init` (not `otelgenai`) is the place to add plugins; `otelgenai` does not need a Genkit instance (README).

**Normalizer (by reading the cetra convention code only).** Every key is `gen_ai.*`, so detection marks `genai` (`crates/otel/src/detect.rs:43-44`). Provider comes from `gen_ai.provider.name` (`crates/otel/src/context.rs:371, 390`). Kind follows `gen_ai.operation.name` `chat` → an LLM span. **Not run.** Expected: one `llm` span per model call, tokens present, input/output present only under span-only content, and model from the request name. Genkit spans with `genkit.action.type` and no GenAI keys would be generic spans.

### 4.2 openai-go (no OTel of its own; two routes, neither covers v3)

- **openai-go v3.100.0** (the current major): **0 files mention OpenTelemetry** (`E/go/openai-go/…/v3@v3.100.0`). Its `option.WithMiddleware` exists (`option/requestoption.go:98-102`), so an app can add tracing itself, but that is hand-written code.
- **OpenInference Go, OpenAI middleware v0.1.2**: `Middleware(tracer, opts...)` returns an `option.Middleware` (`openai.go:72`); the package imports `github.com/openai/openai-go/option` **v1** (its `go.mod` requires `openai-go v1.12.0`; `openai.go:41`). Using it with v3 is **not verified**: the types may not match, and nothing was compiled.
  - Usage (`README.md`, "Use"): `option.WithMiddleware(openaiotel.Middleware(otel.Tracer("my-app")))`.
  - Span: `openai.chat.completions.create` (docs, `README.md` table); `openinference.span.kind` = `LLM`; `llm.system` = `openai`; `llm.provider` = `openai` (or `azure` for Azure hosts); `llm.model_name`; `llm.invocation_parameters` (JSON); `llm.input_messages.{i}.message.*`; `llm.output_messages.{i}.message.*`; `input.value`, `output.value`; `llm.finish_reason`; `llm.token_count.prompt` / `.completion` / `.total` (`README.md`; source `openai.go:467-…`).
  - Content: on by default. `OPENINFERENCE_HIDE_INPUTS` and `OPENINFERENCE_HIDE_OUTPUTS` hide it (`E/go/oi-instrumentation/…/traceconfig.go:102-105`; `instrumentation.go:37-38`).
  - Responses API: `openai.responses.create` (`responses.go`).
- Core OpenInference package: `openinference-instrumentation` v0.1.2 (proxy zip above). The OpenInference Go repo has **no** LangChain, Genkit or OpenLIT-style integration: its module list is `openinference-instrumentation-anthropic-sdk-go`, `openinference-instrumentation-openai-go`, `openinference-instrumentation`, `openinference-semantic-conventions` (gh, `Arize-ai/openinference/go`).

### 4.3 OpenLIT Go (its own OpenAI client; no openai-go)

- Module `github.com/openlit/openlit/sdk/go` at commit `6b97612e` (2026-10-13). No tags on the module (`git/refs/tags` filter for `sdk/go` is empty; the proxy has no versions). Pinning means a pseudo-version (§ 2). Its README's install line is `go get github.com/openlit/openlit/sdk/go` (`sdk/go/README.md`).
- **It does not wrap openai-go.** Its OpenAI support is `instrumentation/openai`, with its own `InstrumentedClient` and its own HTTP calls (`instrumentation/openai/client.go:20-30`; `instrumentor.go:28-72` posts to `c.baseURL+"/chat/completions"`). The package imports no `github.com/openai/...` (grep: 0 matches). `InstrumentedTransport` is a passthrough that records nothing (`instrumentation/openai/transport.go:28-31`).
- Init and export (`openlit.go:46-99, 211-220`): `openlit.Init(openlit.Config{OtlpEndpoint, OtlpHeaders, ApplicationName, Environment, …})` (`config.go:10-50`). `OtlpHeaders map[string]string` goes to `otlptracehttp.WithHeaders` (`openlit.go:219-220`); `OtlpEndpoint` goes to `WithEndpointURL` (`openlit.go:215`). **The endpoint is passed as a full URL; the otel doc says it "should resemble `https://example.com:4318/v1/traces`" (`otlptracehttp` v1.46.0, `internal/otlpconfig/options.go:280-281`), but the config default is `http://127.0.0.1:4318` (`config.go:12`). Whether a bare base URL is accepted needs a check before the page is written.** `openlit.go:84` sets the global tracer provider.
- Span: `chat <model>`, CLIENT (`instrumentor.go:38-39`). Attributes (`instrumentor.go:307-313`, `…:362-…`):
  - `gen_ai.operation.name` = `chat`; **`gen_ai.system` = `openai`** (the older key, `semconv/genai.go:31`; not `gen_ai.provider.name`); `gen_ai.request.model`; `server.address` = `api.openai.com`, `server.port`; `gen_ai.request.is_stream`; `gen_ai.request.user`, `.temperature`, `.top_p`, `.max_tokens`.
  - response: `gen_ai.response.model`; `gen_ai.usage.input_tokens` and **`gen_ai.usage.total_tokens`, but no `gen_ai.usage.output_tokens`** (`instrumentor.go:197-200`; `gen_ai.usage.output_tokens` is defined in `semconv/genai.go:91` and never set); `gen_ai.usage.cost` (`instrumentor.go:102, 203`, computed by the SDK's own price table); `gen_ai.response.finish_reasons`; `gen_ai.output.messages` and `gen_ai.input.messages` (`instrumentor.go:347-356, 411-…`).
  - metrics: `gen_ai.client.token.usage` and `gen_ai.client.operation.duration` (`instrumentor.go:31-34`).
- **Content: on by default** (`helpers/config.go:6`, `captureMessageContent = true`). `openlit.go:69` sets it from `!cfg.DisableCaptureMessageContent` (`config.go:67-70`). No environment variable is read for it.
- Embeddings: `embedding` (`instrumentor.go:133`); images `image` (`:228`).

### 4.4 langchaingo v0.1.15 (no OpenTelemetry route)

- No OTel, tracing or span code in any non-test source file: `grep -rn Tracer\|StartSpan\|trace.Span\|tracing` returns only an httprr comment and a devtools lint comment (`E/go/langchaingo/…/v0.1.15`). `go.mod` lists `go.opentelemetry.io/otel` and `otlptrace` only as indirect test dependencies (`go.mod:145-150, 365-368`).
- The one hook is `callbacks.Handler` (`callbacks/callbacks.go:14-…`): `HandleLLMGenerateContentStart`, `HandleLLMGenerateContentEnd`, `HandleLLMError`, `HandleChainStart`, and so on. An app could write an adapter that opens spans. That is custom code, not a native route, and no OpenInference or OpenLIT package provides one (§ 4.2, § 4.3).
- **Page impossible as the design states it ("Genkit native, OpenInference Go, OpenLIT Go" for Go overall is fine; langchaingo specifically has no route).** See § 7.2.

## 5. Ruby

### 5.1 ruby_llm 2.2.2 (built-in OTel module, GenAI attributes)

**Route.** `RubyLLM::OpenTelemetry`, in the `ruby_llm` gem itself (`lib/ruby_llm/open_telemetry.rb`, `attributes.rb`, `content.rb`; `docs/_advanced/opentelemetry.md` in the repo, read through `gh`). The app owns the SDK and exporter; RubyLLM never configures them (`open_telemetry.rb:10-12`; docs "Enable Tracing").

**Enable.** `RubyLLM::OpenTelemetry.enable` or `enable(content: true)` (`open_telemetry.rb:19`); `disable` (`:33`). It requires `opentelemetry-api`, which `opentelemetry-sdk` provides (`:20, 27-28`).

**Span names and kinds** (`attributes.rb:6-42`): `<operation> <target>`, target = model for chat/embeddings, agent class for `invoke_agent`, tool name for `execute_tool`. `chat gpt-…` CLIENT; `invoke_agent <Agent>` INTERNAL; `execute_tool <tool>` INTERNAL; also `embeddings`, `generate_content`, `transcription`, `ocr`, `rerank`, `moderation`, `judgment`, `invoke_workflow`, `ruby_llm.workflow_step`, `ruby_llm.code_execution` (`attributes.rb:6-24`).

**Attributes.**
- Model call (`attributes.rb:44-61`): `gen_ai.operation.name`; `gen_ai.provider.name` = `provider.open_telemetry_name` (`attributes.rb:63-66`; default is the slug, `provider.rb:584-587`; overridden in `providers/bedrock.rb:54`, `gemini.rb:42`, `vertexai.rb:54`, `azure.rb:150`, `mistral.rb:34`, `moonshot.rb:32`, `ollama_cloud.rb:36`, `xai.rb:26`); `gen_ai.request.model`; `gen_ai.request.temperature`; `gen_ai.request.max_tokens`; `gen_ai.request.stream` (only when true); `gen_ai.request.reasoning.level`; `gen_ai.embeddings.dimension.count`; `gen_ai.output.type` (`json`, `image`, `speech`).
- Response (`attributes.rb:80-99`): `gen_ai.response.model`, `gen_ai.response.finish_reasons` (one value), `gen_ai.usage.input_tokens` (input + cache read + cache write), `gen_ai.usage.output_tokens`, `gen_ai.usage.cache_read.input_tokens`, `gen_ai.usage.cache_write.input_tokens`, `gen_ai.usage.reasoning.output_tokens`.
- Agent (`attributes.rb:140-147`): `gen_ai.operation.name` = `invoke_agent`, `gen_ai.agent.name`, `gen_ai.agent.description`, `gen_ai.conversation.id`.
- Tool (`attributes.rb:115-124`): `gen_ai.operation.name` = `execute_tool`, `gen_ai.tool.name`, `gen_ai.tool.call.id`, `gen_ai.tool.description`, `gen_ai.tool.type` = `function`, `gen_ai.agent.name`; for a skill load, `gen_ai.skill.name` and, with content, `gen_ai.skill.resource.name`.
- Errors: `error.type` = exception class, span status ERROR (`open_telemetry.rb:78-83`).

**Content.** Off unless `enable(content: true)` (`open_telemetry.rb:19, 38-40`). **No environment variable** switches it. When on (`content.rb:10-40`): `gen_ai.system_instructions`, `gen_ai.input.messages`, `gen_ai.output.messages` (JSON strings in the OTel GenAI parts format `{role, parts:[{type, content}]}`, `content.rb:42-65`); tools get `gen_ai.tool.call.arguments` and `gen_ai.tool.call.result` (`content.rb:12-22`). Attachments are described, not sent (`content.rb:67-76`). URIs lose their query string (`content.rb:89-92`).

**Setup** (by source, not run):

```ruby
require "opentelemetry/sdk"
require "opentelemetry/exporter/otlp"
require "ruby_llm"

OpenTelemetry::SDK.configure { |c| c.service_name = "my-app" }
RubyLLM::OpenTelemetry.enable            # or enable(content: true)
```

with `OTEL_EXPORTER_OTLP_ENDPOINT=<base>` and `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20<key>` (`exporter.rb:55` reads `OTEL_EXPORTER_OTLP_TRACES_HEADERS` or `OTEL_EXPORTER_OTLP_HEADERS`; `exporter.rb:302-306` decodes each value with `URI.decode_uri_component`, so `%20` is correct). The SDK's default trace exporter is `otlp` (`opentelemetry-sdk` 1.13.1, `lib/opentelemetry/sdk/configurator.rb:210`). The OTLP exporter sends **protobuf only** (`exporter.rb:143`, `Content-Type: application/x-protobuf`). The exporter appends `v1/traces` via `build_uri` (`exporter.rb:61`).

**Normalizer.** `gen_ai.*` → `genai` convention (`detect.rs:43-44`). Provider from `gen_ai.provider.name` (`context.rs:371`). Chat span → LLM. Tool and agent spans carry `gen_ai.operation.name` `execute_tool` / `invoke_agent`, which the genai reader already handles (`conventions/genai.rs`, tests at ~931-1297). **Not run.**

### 5.2 opentelemetry-instrumentation-ruby_llm 0.7.1 (third-party, thoughtbot)

- Gem metadata: author Clarissa Borges, MIT, homepage `github.com/thoughtbot/opentelemetry-instrumentation-ruby_llm` (gh/rubygems record `opentelemetry-instrumentation-ruby_llm.meta.json`, `E/notes/`). Depends on `opentelemetry-api ~> 1.0` and `opentelemetry-instrumentation-base ~> 0.23` (`gemspec:21-22`). Its base version is not downloaded here.
- Setup (`README.md`, "Usage"): `OpenTelemetry::SDK.configure { |c| c.use 'OpenTelemetry::Instrumentation::RubyLLM', capture_content: true }`. It refuses `ruby_llm` below 1.8.0 (`instrumentation.rb:7, 20-34`).
- It **monkey-patches** `RubyLLM::Chat` (`patches/chat.rb`) and `RubyLLM::Embedding`, and on Rails, the ActiveRecord chat methods (`instrumentation.rb:36-65`).
- Spans: `chat <model_id>` CLIENT (`patches/chat.rb:35`); `execute_tool <name>` INTERNAL (`:91`).
- Attributes: `gen_ai.operation.name`, `gen_ai.provider.name` = the **raw** provider slug (`chat.rb:21-26`), `gen_ai.request.model`, `gen_ai.conversation.id`, `gen_ai.request.stream` (when streaming), `gen_ai.response.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.usage.cache_read.input_tokens`, `gen_ai.usage.cache_creation.input_tokens`, `gen_ai.request.temperature` (`chat.rb:47-60`). Tool spans: `gen_ai.tool.name`, `gen_ai.tool.call.id`, `gen_ai.tool.call.arguments` (JSON), `gen_ai.tool.call.result` truncated to 500 characters (`chat.rb:81-107`; `instrumentation.rb:14`).
- Content: **off by default** (`instrumentation.rb:13`, `capture_content` default false). `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true` overrides the option when set (`patches/chat.rb:111-116`). When on: `gen_ai.system_instructions`, `gen_ai.input.messages`, `gen_ai.output.messages` on the chat span (`chat.rb:62-72`).
- **Overlap.** It and the built-in module both write `chat` spans. Enabling both doubles every model call (the same problem as in § 4.2's OpenAI note).

### 5.3 ruby_llm-opentelemetry 0.1.0 (placeholder, not a route)

Its README says "This is a placeholder release. The full implementation is coming soon!" (`README.md`). Not usable. Recorded so nobody picks it up.

## 6. Rust

### 6.1 rig-core 0.46.0 (`tracing` spans with GenAI attributes; no exporter)

**Route.** The crate writes `tracing` spans; there is **no OpenTelemetry code** in `src/`, `tests/` or `benches/` (`grep -rn opentelemetry` returns nothing). `Cargo.toml:240-268` lists `opentelemetry` 0.32, `opentelemetry-otlp` 0.32, `opentelemetry_sdk` 0.32 and `tracing-opentelemetry` 0.33 as **dev-dependencies only**, unused in the sources. So the app bridges the spans to OTLP itself with `tracing-opentelemetry`. The design's "native GenAI" is true for the attributes, not for the export.

**Span names** (`src/telemetry/mod.rs`):
- completions: `chat` (chat) or `generate_content` (Gemini), target `rig::completions`, opened by `new_completion_span!` (`mod.rs:66-78`, match at `:380-390`). If the current span is a conforming completion parent, rig reuses it (`adopt_completion_parent`, `mod.rs:~420-440`).
- other modalities, target `rig::modalities`: `embeddings`, `rerank`, `transcription`, `image_generation`, `audio_generation` (`mod.rs:326-342`, `:390-400`).
- Span kind is **not set** (`otel.kind` does not appear in `src/`). A `tracing-opentelemetry` bridge therefore exports INTERNAL unless the app sets it (not verified in this spike).

**Attributes** (`mod.rs:49-60, 72-75, 143-156, 331-342, 443-448, 682-711, 787-…`):
- `gen_ai.operation.name` (`chat`, `generate_content`, `embeddings`, …); `gen_ai.provider.name` = the wire name, e.g. `openai` (`PROVIDER_NAME`, `src/providers/openai/mod.rs:24`; passed at `src/operation/completion.rs:66`); `gen_ai.request.model`; `gen_ai.response.model`; `gen_ai.response.id`; `gen_ai.request.stream` (completions only).
- tokens: `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.usage.cache_read.input_tokens`, `gen_ai.usage.cache_creation.input_tokens`, `gen_ai.usage.tool_use_prompt_tokens`, `gen_ai.usage.reasoning_tokens` (`mod.rs:770-800`, a reported zero is recorded, an absent counter is not).
- `rig.provider_request_id` (`mod.rs:131`).
- content, only on opt-in: `gen_ai.system_instructions`, `gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.tool.definitions` (`mod.rs:682-740`).

**Content switch.** Per request: `CompletionRequest::record_content_telemetry(true)` (`src/completion/request.rs:1642`), **off by default**, and the doc says it "exposes" sensitive content (`request.rs:1640`). It is not an environment variable (no `env::var` for it in `src/`). Passed as `request.record_telemetry_content` (`src/operation/completion.rs:70-75`).

**Setup** (by source, not run; verify the exact builder names against `tracing-opentelemetry` 0.34 and `opentelemetry-otlp` 0.33 when the page is written):

```rust
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::SpanExporter;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

let exporter = SpanExporter::builder().with_http().build()?;   // opentelemetry-otlp/src/span.rs:66
let provider = SdkTracerProvider::builder()                    // opentelemetry_sdk provider.rs:177
    .with_batch_exporter(exporter)                             // provider.rs:334
    .build();
let tracer = provider.tracer("my-app");
tracing_subscriber::registry()
    .with(tracing_opentelemetry::layer().with_tracer(tracer))  // tracing-opentelemetry layer.rs:104, 717
    .init();
```

with `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20<key>` (`opentelemetry-otlp` 0.33.1 reads `OTEL_EXPORTER_OTLP_HEADERS` in 12 places; the protocol default is `http/protobuf`, `src/exporter/mod.rs:26-31`). The decoding of `%20` in this crate was not read.

**Normalizer.** `gen_ai.*` → genai convention. Span name `chat` carries no model; the model is in `gen_ai.request.model`. `gen_ai.operation.name` = `chat` → LLM. **Not run.**

### 6.2 Swiftide 0.34.0 (no GenAI attributes)

- The crate and its sub-crates have **no `opentelemetry` or `tracing-opentelemetry` dependency** (`Cargo.lock`: no `opentelemetry` entries; `tracing` is a plain dependency of `swiftide-agents`, `Cargo.toml:183`).
- Spans are plain `tracing` spans (`swiftide-agents` 0.34.0):
  - `agent.query`, `agent.query_once`, `agent.run`, `agent.run_once` (`src/agent.rs:371, 400, 411, 422`);
  - `step` with `otel.name` = `step-N` (`agent.rs:536-540`);
  - `tool` with `otel.name` = `tool.<name>` (`agent.rs:650`);
  - `hook` with `otel.name` = `hook.<Type>` (`src/util.rs:44-47`);
  - `local_executor`, `mcp` spans (`tools/local_executor.rs:342`, `tools/mcp.rs:152`).
- Integrations (`swiftide-integrations` 0.34.0): LLM calls are `#[tracing::instrument(skip_all, err)]` on the function (e.g. `anthropic/chat_completion.rs:27, 115`), so the span name is the function name. The only `langfuse.type = "GENERATION"` fields are in the Bedrock module (`aws_bedrock_v2/chat_completion.rs:57, 206, 436`), which are Langfuse-specific.
- **`gen_ai` appears in 0 files** across `swiftide-integrations`, and `swiftide-agents` has none either. No model, provider, token or I/O field is recorded on any span (the only `.record` is a cache `hit` flag, `src/redis/node_cache.rs:49`).
- Langfuse (`swiftide-langfuse` 0.34.0): posts to Langfuse's own `/api/public/ingestion` JSON API (`src/apis/ingestion_api.rs:34`; batch manager `src/langfuse_batch_manager.rs`). It is not OTLP and cetra does not receive it.
- Consequence: an OTLP setup for Swiftide gives agent, step, tool and hook spans with **no** LLM attributes. cetra would not see model calls, tokens or prompts. The design's "native GenAI" is wrong for Swiftide (§ 7.6).

## 7. C++

### 7.1 "AI SDK C++": which project, and no OTel

- **ClickHouse/ai-sdk-cpp** (Apache-2.0, created 2025-06-21, default branch `main`, head `9d8cd18200a3`). Its README is titled "# AI SDK CPP" (`README.md` line 1), which matches the design's name best. It has **no OpenTelemetry**: a grep for `opentelemetry|otlp|otel` (case-insensitive) over all 252 tree entries finds nothing (`E/cpp/clickhouse-ai-sdk-cpp/ai-sdk-cpp-9d8cd18200a3`).
- Its only tracing is **Langfuse**: `include/ai/langfuse.h:3-7` ("Records traces, generations, and tool spans … POSTs them to Langfuse's `/api/public/ingestion` endpoint"), `src/langfuse/tracer.cpp:215-229` (the ingestion path and the error log). The example is `examples/langfuse_tracing.cpp`. Langfuse's JSON API is not OTLP, so cetra cannot receive it.
- Other projects with the same name: **mhdimo/ai-sdk-cpp** (MIT, "A C++20 framework…", pushed 2026-10-17; 0 OTel files) and **abhisheksatyam123/qcode** ("AI SDK C++ — multi-provider LLM client (qcode TUI fork)", pushed 2026-11-14; 0 OTel files). Neither has a release or tag (`gh api …/releases` empty). Neither is pinned here; they are recorded only to show they were checked.
- **Conclusion:** no project named "AI SDK C++" has an OpenTelemetry export. A setup-only page for it cannot say "its OTel export" (§ 7.1 question).

## 8. Open questions for the user (none answered here; each needs a decision, D-IN-17)

1. **AI SDK C++.** The only exact-name match (ClickHouse) exports to Langfuse's JSON API only, and the other two candidates have no export at all. Options: (a) name ClickHouse's project as the target and move the page to § Not supported (D-IN-10) because it cannot send spans, or (b) keep a page that says "no OTLP export; Langfuse only", which is not a setup-only page. Which project is meant, and which option?
2. **langchaingo.** It has no OTel code and no OpenInference or OpenLIT route. Options: (a) § Not supported; (b) a hand-written `callbacks.Handler` adapter in the page (custom code, not a native route, so it needs a design decision). Which?
3. **openai-go.** The current major (v3) has no route. Options: (a) OpenInference Go's middleware, which is written for openai-go **v1.12.0** (2025-07-30), an old major, with the v3 compatibility unverified; (b) OpenLIT Go's own client (not openai-go); (c) a hand-written middleware on v3. Which?
4. **OpenLIT Go.** It is an untagged module pinned by commit (`6b97612e`, 2026-10-13); its chat spans write `gen_ai.system`, not `gen_ai.provider.name`, and no output tokens. Accept a commit pin and the token gap, or leave OpenLIT Go out of the Go page? Also confirm how `OtlpEndpoint` should be written (full `/v1/traces` URL or base URL, § 4.3).
5. **Ruby.** "Its OTel gem": (a) the built-in `RubyLLM::OpenTelemetry` in `ruby_llm` 2.2.2 (first-party, documented in ruby_llm's docs, no extra gem), (b) the thoughtbot gem `opentelemetry-instrumentation-ruby_llm` 0.7.1 (third-party, monkey-patches RubyLLM, content env var), or (c) both as alternatives on the page. Enabling both doubles spans. Which?
6. **Swiftide.** It writes no GenAI attributes; the "native GenAI" premise is wrong. Options: (a) a setup-only page that says the OTLP output is agent, step, tool and hook spans with no model or tokens (its Limits would say so), (b) § Not supported. Which?

The other items need no decision: Genkit Go, rig, and the ruby_llm built-in route are documented in this spike as possible, pending the user's answer to 5 for Ruby.

## 9. Normalizer, facts only (no changes proposed here)

- Every GenAI route above (Genkit, rig, ruby_llm built-in, the thoughtbot gem, OpenLIT Go) writes `gen_ai.*` and is read by the `genai` convention (`crates/otel/src/detect.rs:43`). Two points the normalizer may need, which the plan decides:
  - **`gen_ai.system` as provider** (OpenLIT Go writes only this key): `context.rs:390` already accepts it as a provider key, so this should work, but no run confirmed it.
  - **Output tokens missing** (OpenLIT Go): nothing to read; the trace will show input tokens only.
- Span-only content: Genkit (`SPAN_ONLY`), ruby_llm (`enable(content: true)`), thoughtbot gem (`capture_content`), rig (`record_content_telemetry(true)`) all put JSON strings on the span; Genkit's `EVENT_ONLY` mode puts content on a log record, which cetra drops for OTLP logs unless a reader exists (not checked here; compare D-IN-23 item 4, which says Semantic Kernel's log-record messages are dropped).
- OpenInference Go (`llm.*`, `input.value`) → the `openinference` convention, which is already in the precedence list (`crates/otel/src/conventions/mod.rs:13-35`).
- Swiftide's plain spans are not a GenAI convention; cetra can only show them as generic spans.

## 10. Network and runs

- **No run.** No program was built, installed or executed. The loopback namespace (`netrun.sh`) was not used because nothing ran.
- **Outbound requests** (all read-only, to official sources; none carried user data): `proxy.golang.org`; `rubygems.org` (API and `downloads/`); `crates.io` API and `static.crates.io`; `api.github.com` via `gh`; `codeload.github.com` (one tarball). No other host was contacted.
- User data: none sent. Each request had only a package name or URL.
- Nothing left loopback because nothing ran.

## 11. Downloads (all in `E/dl/`)

| File | Size (bytes) | sha256 | Source URL |
|---|---|---|---|
| genkit-go-v1.14.0.zip | 2,989,651 | 879211018df944120f1ff51163062d8a5e3b7ac2ab6d7ccfb6d55296f45cb90c | https://proxy.golang.org/github.com/firebase/genkit/go/@v/v1.14.0.zip |
| openai-go-v3.100.0.zip | 2,393,995 | 883c49b4ebdaa13401857ca2f4f6a62c44bdc691b6c88514d2eadc3c3b9d432b | https://proxy.golang.org/github.com/openai/openai-go/v3/@v/v3.100.0.zip |
| openai-go-v1.12.0.zip | 454,642 | a3ce4967e9edb2bea4a1b6e5044180595e7fe7c04054404f9d14b92d90bb2354 | https://proxy.golang.org/github.com/openai/openai-go/@v/v1.12.0.zip |
| oi-openai-go-v0.1.2.zip | 60,854 | a2aed617b05eefc16394caf111831a37f3048edc43046ef5b4189f45ea058db8 | https://proxy.golang.org/github.com/!arize-ai/openinference/go/openinference-instrumentation-openai-go/@v/v0.1.2.zip |
| oi-instrumentation-v0.1.2.zip | 18,230 | 4a71eefc8f9c7893cfadc0a4d8bf74e02059698018cf1044afe59eee38ed4f91 | https://proxy.golang.org/github.com/!arize-ai/openinference/go/openinference-instrumentation/@v/v0.1.2.zip |
| langchaingo-v0.1.15.zip | 5,756,558 | 2a94c52cfcdc98475593ade26da4a4f82ef08381ba405c11639b527d90e919ab | https://proxy.golang.org/github.com/tmc/langchaingo/@v/v0.1.15.zip |
| otlptracehttp-v1.46.0.zip | 82,722 | f87ca4401bb735a8f70c279e9fa07977d795f49c95d2d7518488614732d5ef55 | https://proxy.golang.org/go.opentelemetry.io/otel/exporters/otlp/otlptrace/otlptracehttp/@v/v1.46.0.zip |
| openlit-6b97612.tgz | 109,305,127 | 6bba15da890376d693642a73630d9ebfdbda04765f379da7c7488d51b15511e9 | `gh api repos/openlit/openlit/tarball/6b97612e58d1e9253ac0d0ab89230da1008638de` |
| ruby_llm-2.2.2.gem | 882,176 | e08da09150bc11e15b294a7628b8295e0813d7f611ca97f74dcb1286ba7cdc79 | https://rubygems.org/downloads/ruby_llm-2.2.2.gem |
| opentelemetry-instrumentation-ruby_llm-0.7.1.gem | 21,504 | 80e58f37a3d827c94aaa6a250f421333dc78ddd36a4870016f523d2fbc4aa608 | https://rubygems.org/downloads/opentelemetry-instrumentation-ruby_llm-0.7.1.gem |
| ruby_llm-opentelemetry-0.1.0.gem | 5,632 | d0cb8504c7a6b8d6e0376760dcd4d4b203e136bd7218cc1ae4746a69b926f124 | https://rubygems.org/downloads/ruby_llm-opentelemetry-0.1.0.gem |
| opentelemetry-exporter-otlp-0.37.0.gem | 22,528 | edc8bda657dce5f4d527ac735f75d44043bb5924de6df0e3f01260108aa76136 | https://rubygems.org/downloads/opentelemetry-exporter-otlp-0.37.0.gem |
| opentelemetry-sdk-1.13.1.gem | 34,816 | 91c9cc315ac7ef30243549eeff8b27cd0d39bd02bdf9c9ae481ab96a1cec6754 | https://rubygems.org/downloads/opentelemetry-sdk-1.13.1.gem |
| rig-core-0.46.0.crate | 1,574,837 | c46316e31b3e80a050ad35790f1b612b4dfdb46982293cc78641caf4271f1b05 | https://static.crates.io/crates/rig-core/rig-core-0.46.0.crate |
| swiftide-0.34.0.crate | 88,748 | 17e033a248b1b09673d609ba77b2bdbd86e83d5cf20b3d8e1246f62543697931 | https://static.crates.io/crates/swiftide/swiftide-0.34.0.crate |
| swiftide-agents-0.34.0.crate | 71,496 | d1492bdffb2e719ee1d72dcf6bf1bb845aa797653d744a9a3445dd8c721a578d | https://static.crates.io/crates/swiftide-agents/swiftide-agents-0.34.0.crate |
| swiftide-integrations-0.34.0.crate | 235,349 | 546a677e886ec5ae9d235989835f241fd4352efbdf0466cb2f1cbf73db1fb8c7 | https://static.crates.io/crates/swiftide-integrations/swiftide-integrations-0.34.0.crate |
| swiftide-langfuse-0.34.0.crate | 46,969 | 2e5e58fb8a1d1532bd65c864db28bbe1937163e87b830ff9840c588f27fbe522 | https://static.crates.io/crates/swiftide-langfuse/swiftide-langfuse-0.34.0.crate |
| opentelemetry-otlp-0.33.1.crate | 93,336 | 0756af24841e367128ef31c831999937b8b5c5197d47914590d8f4e2e88c2e1a | https://static.crates.io/crates/opentelemetry-otlp/opentelemetry-otlp-0.33.1.crate |
| opentelemetry_sdk-0.33.1.crate | 278,427 | 372a912db2974d1990207006fd0e3aa0ef3fd4ad3be43c65cfaadf2206866061 | https://static.crates.io/crates/opentelemetry_sdk/opentelemetry_sdk-0.33.1.crate |
| tracing-opentelemetry-0.34.0.crate | 64,693 | 0a904802a1b902f43638b677ff2a650847e3b4404101b6c586d648e8c1e3e8fe | https://static.crates.io/crates/tracing-opentelemetry/tracing-opentelemetry-0.34.0.crate |
| clickhouse-ai-sdk-cpp-9d8cd18.tar.gz | 409,085 | 00b973f749cbc946262f148a23f44deaf1f2c0fef5aace6db83b96ea109ad52a | https://codeload.github.com/ClickHouse/ai-sdk-cpp/tar.gz/9d8cd18200a3 |

Metadata only, not in the table: registry JSON under `E/notes/` (crates.io, RubyGems, GitHub API responses), and proxy `.info` records read live.

Notes on the table: the `openai-go-v1.12.0.zip` is needed only for OpenInference's middleware and for the § 4.2 question. Nothing else in the list is a dependency of a shipped crate.
