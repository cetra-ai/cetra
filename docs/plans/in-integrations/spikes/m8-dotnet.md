# M8 spike: .NET (Microsoft.Extensions.AI, Semantic Kernel, Microsoft Agent Framework)

Status: spike, not a plan. Nothing here changes the design or the decisions. The open questions in § 7 need the user or the plan. Written 2026-11-24 (UTC clock about 12:00).

Evidence root (E): `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m8/dotnet`. Paths below are relative to E unless absolute. Each fact is tagged:

- **run**: observed by a command in this session, with the evidence path.
- **docs**: read from a package's own files or a Microsoft page, with the path or URL.

## 1. Scope

- Microsoft.Extensions.AI 11.0.0 with OpenAI 2.15.0 as the chat client (section 4.1).
- Semantic Kernel .NET 1.82.0 (section 4.2): where its content lands and the AppContext switches.
- Microsoft Agent Framework .NET 1.28.0 (section 4.3): its OTel, agent and tool spans.
- Each program: one chat call (no tools offered), then one tool-call round (the model asks for `read_file`, the program runs it, the model answers). The fake provider (`m7/tools/server.py`) answers the first request that carries tools with a tool call and every later request with text.
- Not covered: Java, Go, Ruby, Rust, C++ (M8 setup-only or Java runner; not in this spike). The capture-runner questions (section 6) are answered from `tools/capture` reads, not from a build of a runner.

## 2. Versions (the 7-day rule)

Rule: the newest release published at least 7 days before the install. Install clock: 2026-11-24 about 11:54 UTC, so the cutoff is **2026-11-17 11:54 UTC**. A newer release is not eligible.

Sources: `logs/releases-index.json` and `logs/releases-10.0.json` (Microsoft .NET release metadata, read this session); `logs/nuget-versions.txt` (NuGet registration index, read by `logs/nuget_versions.py`).

| Item | Version pinned | Publish time (UTC) | Download or package URL | sha256 / integrity | Notes |
|---|---|---|---|---|---|
| .NET SDK (LTS line 10.0) | SDK `10.0.404`, runtime `10.0.15` | release group 2026-11-10 (`releases.json`, `release-date`) | `https://builds.dotnet.microsoft.com/dotnet/Sdk/10.0.404/dotnet-sdk-10.0.404-linux-x64.tar.gz` (231.9 MB) | sha512 `577d0eb1…dc50` matches `releases.json`; sha256 `c0610757a818fde4b960d27a0ac421f273068a05dc5ec7abafc01dbf84e9cb4d` (`dl/`) | `latest-sdk` of the 10.0 channel. The 10.0 line is LTS, active to 2028-11-14. |
| .NET 11 (STS) | not used | SDK 11.0.100 is listed `active`, not LTS | — | — | No pinned package below needs it (all eligible packages target net10.0 or lower, see § 2 notes). |
| .NET 8 | not used | EOL 2026-11-10 | — | — | `releases-index.json` lists `8.0 eol`. |
| Microsoft.Extensions.AI | `11.0.0` | 2026-11-10T23:48:23Z | nuget.org `microsoft.extensions.ai/11.0.0` | in `logs/nuget-hashes.tsv` | `11.0.1` (2026-11-21) is too new. |
| Microsoft.Extensions.AI.OpenAI | `11.0.0` | 2026-11-10T23:48:31Z | nuget.org | in `logs/nuget-hashes.tsv` | `AsIChatClient()` over the OpenAI client. |
| OpenAI (.NET SDK) | `2.15.0` | 2026-11-05T08:01:25Z | nuget.org | in `logs/nuget-hashes.tsv` | newest stable. |
| OpenTelemetry (.NET SDK) | `1.21.0` | 2026-11-11T11:29:02Z | nuget.org | in `logs/nuget-hashes.tsv` | `1.21.0` is also newest. |
| OpenTelemetry.Exporter.OpenTelemetryProtocol | `1.21.0` | 2026-11-11T11:29:03Z | nuget.org | in `logs/nuget-hashes.tsv` | OTLP/HTTP protobuf exporter. |
| Microsoft.SemanticKernel | `1.82.0` | 2026-11-07T00:51:29Z | nuget.org | in `logs/nuget-hashes.tsv` | newest stable. |
| Microsoft.Agents.AI | `1.28.0` | 2026-11-06T21:57:55Z | nuget.org | in `logs/nuget-hashes.tsv` | newest stable (Agent Framework .NET). |
| Microsoft.Agents.AI.OpenAI | `1.28.0` | 2026-11-06T21:57:46Z | nuget.org | in `logs/nuget-hashes.tsv` | OpenAI-backed agent. |

Versions in the csproj files are exact pins; `packages.lock.json` records the resolved graph (the `contentHash` of each package).

## 3. Method

- **Install.** The SDK is extracted into `sdk/` (no system install). `env.sh` exports `DOTNET_ROOT`, `DOTNET_CLI_HOME`, `NUGET_PACKAGES`, `NUGET_HTTP_CACHE_PATH`, `NUGET_PLUGINS_CACHE_PATH`, `HOME` (inside E) and `DOTNET_CLI_TELEMETRY_OPTOUT=1`.
- **Build (outside the namespace).** `dotnet restore` (online, nuget.org only, writes `packages.lock.json` with `RestorePackagesWithLockFile=true`), then `dotnet build --no-restore`. Output is `bin/Debug/net10.0/<name>.dll`.
- **Run (inside the namespace).** `runner/m8run.sh <name> <dll> <env-file>` copies the env file to `runs/<name>/env`, sets `PWD` to the run dir, and calls `m7/tools/netrun.sh <run> 150 -- bash -c '…; exec "$DOTNET_ROOT/dotnet" <dll>'`. The inner command takes `OTEL_EXPORTER_OTLP_ENDPOINT` from `$M7_OUT/base.url` (the fake provider and OTLP sink base URL, written by `inner.sh`); the endpoint is not in the env file because `inner.sh` writes `base.url` only after the server starts.
- **Normalize.** `m7/tools/h.py <run> <name>` (the real cetra normalizer, `m7/norm-target/release/norm`) writes `out/<name>.dump.txt` and `out/<name>.norm.jsonl`.
- **Raw view.** `runner/rawspans.py` prints each span's protobuf kind, name, scope, parent presence and content-bearing keys (keys only). `h.py` prints `kind=None` for every span because `MessageToDict` emits enum names; the normalizer is unaffected. Use `rawspans.py` for kinds.
- **Fake key.** `sk-fake-m8` (`sk-cetra-fake-m8` for the OTLP header). No real key, no real provider.
- **Project layout.** `src/extai/` (section 4.1). Sections 4.2 and 4.3 add `src/sk/` and `src/maf/`.
- **Guard.** `real-home.txt` per run. The first Extensions.AI run without the `PWD` fix recorded one hit: bash stat'ed the caller's cwd (a worktree path under the real home) because the inherited `PWD` variable pointed there. That run was discarded; the runner now sets `PWD` to the run directory, and the runs in § 4 have no real-home touches.

## 4. Run index

| Run (`runs/`) | Program | What it tests | Result |
|---|---|---|---|
| `extai-sens1` | extai | chat + tool round, `EnableSensitiveData=true`, OTLP header `Authorization: Bearer sk-cetra-fake-m8` | 6 spans, 3 model calls, 2 traces; content on llm and tool spans; cost computed |
| `extai-default` | extai | same, `EnableSensitiveData=false` set explicitly | 6 spans; no content attribute anywhere |
| `extai-nodefault` | extai-nodefault | same, no `configure` call (the default) | 6 spans; no content attribute anywhere |
| `sk-none` | sk | chat and tool round, no AppContext switch (default) | 1 span only: `execute_tool read_file`. No chat span for any of the 3 model calls |
| `sk-on` | sk | `EnableOTelDiagnostics=true` | 4 spans: 3 chat, 1 tool; no events, no content |
| `sk-sensitive` | sk | `EnableOTelDiagnostics` and `EnableOTelDiagnosticsSensitive` = true | 4 spans; chat spans carry `gen_ai.*` events with content; tool span carries argument and result |
| `sk-log-sensitive` | sk-log | as `sk-sensitive`, plus a stderr `ILogger` provider at Trace level | 15 `ILOG` lines on stderr; content in them; no `/v1/logs` post (no OTel logging bridge configured) |
| `sk-log-none` | sk-log | as `sk-none`, with the stderr `ILogger` | 15 `ILOG` lines; 4 of them carry prompt or tool content |
| `maf-default` | maf | `UseOpenTelemetry` with no content switch, no env var | 6 spans, no content attribute |
| `maf-code` | maf | `UseOpenTelemetry(configure: o => o.EnableSensitiveData = true)` | 6 spans; content on agent, chat, and tool spans |
| `maf-env` | maf | no code switch; `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true` | 6 spans; the same content as `maf-code` |

## 4.1 Microsoft.Extensions.AI 11.0.0 (`UseOpenTelemetry()`)

### Program

`src/extai/Program.cs` (the key lines):

```csharp
using var tracerProvider = Sdk.CreateTracerProviderBuilder()
    .AddSource("Experimental.Microsoft.Extensions.AI")
    .AddOtlpExporter()
    .Build();
IChatClient inner = new OpenAIClient(new ApiKeyCredential("sk-fake-m8"),
    new OpenAIClientOptions { Endpoint = new Uri(baseUrl + "/v1") })
    .GetChatClient("gpt-4o-mini").AsIChatClient();
IChatClient client = new ChatClientBuilder(inner)
    .UseFunctionInvocation()
    .UseOpenTelemetry(sourceName: "Experimental.Microsoft.Extensions.AI",
        configure: c => c.EnableSensitiveData = sensitive)
    .Build();
var chat = await client.GetResponseAsync("Say hello in one sentence.");          // no tools
var tool = AIFunctionFactory.Create(ReadFile, "read_file");
var round = await client.GetResponseAsync("Read hello.txt and tell me what it says.",
    new ChatOptions { Tools = [tool] });
```

`sensitive` comes from `M8_SENSITIVE` (`1` in `extai-sens1`, `0` in `extai-default`). The exporter reads `OTEL_EXPORTER_OTLP_*`.

Both programs print the fake's final text for both calls (`CHAT:` and `TOOL:` lines in `run.stdout`), because the fake returns its fixed text for any text answer; the tool round is proven by the `execute_tool read_file` span and the `tool_calls` finish reason (below).

### Setup and wiring

- Packages: `Microsoft.Extensions.AI` 11.0.0, `Microsoft.Extensions.AI.OpenAI` 11.0.0, `OpenAI` 2.15.0, `OpenTelemetry` 1.21.0, `OpenTelemetry.Exporter.OpenTelemetryProtocol` 1.21.0 (csproj: `src/extai/m8-extai.csproj`, lock: `logs/extai-packages.lock.json`).
- Build: `dotnet build --no-restore` succeeded, 0 warnings, 0 errors (run in this session).
- OTLP export, from the environment (`runner/env-base`):
  - `OTEL_EXPORTER_OTLP_ENDPOINT` = `http://127.0.0.1:<port>` (base URL, set by `m8run.sh`); the exporter posts to `/v1/traces` (run: `runs/extai-sens1/http.jsonl`, `otlp` event with `path: /v1/traces`).
  - `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`; the body is `application/x-protobuf` (run: `http.jsonl`, `content_type`).
  - `OTEL_EXPORTER_OTLP_HEADERS='authorization=Bearer sk-cetra-fake-m8'`. The sink received `Authorization: Bearer sk-cetr...(23 chars)` (run: `http.jsonl`; 23 = `Bearer ` + 16 characters). A plain space in the value is accepted; no `%20` form was needed.
  - User agent `OTel-OTLP-Exporter-Dotnet/1.21.0`.

### Content switch

- Name: `ChatClientBuilder.UseOpenTelemetry(configure: c => c.EnableSensitiveData = …)`, which sets `OpenTelemetryChatClient.EnableSensitiveData`. Code, not an environment variable.
- Default: **off**. Run `extai-nodefault` (`src/extai-nodefault/`, the same program without any `configure` call) has no content attribute on any span, so the default is off. In `extai-default` no span has `gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.tool.call.arguments` or `gen_ai.tool.call.result` (run: `runs/extai-default/` raw view; the `rawspans.py` output shows an empty key list on every span).
- On (`extai-sens1`): the content is in **span attributes**, not in events and not in log records. `events=0` on every span (run: `rawspans.py` output).
- Shape of the content: `gen_ai.input.messages` and `gen_ai.output.messages` are JSON strings of `{role, parts:[{type, content}]}` (the current GenAI semconv shape; `h.txt`: `fmt=messages`). Tool calls appear as `parts` of `type: tool_call`. `gen_ai.tool.definitions` is written on the chat span of the tool round.
- The XML docs of `EnableSensitiveData` were not read in this session; the name and the default are from the runs above.

### Spans received

Six spans in each run, two traces (one per top-level call). Raw kinds from `rawspans.py` (protobuf enum: 1 = INTERNAL, 3 = CLIENT):

| Span name | Kind | Scope | Parent | Attributes of note | Normalizer (cetra today, `h.txt`) |
|---|---|---|---|---|---|
| `orchestrate_tools` (trace 1, the chat call) | INTERNAL | `Experimental.Microsoft.Extensions.AI` | root | none | `span_kind="span"`, no tokens |
| `chat gpt-4o-mini` (trace 1) | CLIENT | same | `orchestrate_tools` | `gen_ai.operation.name=chat`, `gen_ai.provider.name=openai`, `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.response.id`, `gen_ai.response.finish_reasons`, `gen_ai.usage.input_tokens=131`, `gen_ai.usage.output_tokens=12`, `openai.api.type=chat_completions`, `server.address=127.0.0.1`, `server.port` (+ content in `extai-sens1`) | `span_kind="llm"`, provider `openai`, model `gpt-4o-mini`, tokens 131/12, `cost_total=2.685e-05` (computed), input and output previews present in `extai-sens1` |
| `orchestrate_tools` (trace 2, the tool round) | INTERNAL | same | root | `gen_ai.usage.input_tokens=213`, `gen_ai.usage.output_tokens=29` (the sum of the two model calls below) | `span_kind="span"`; the usage stays in `attributes_left` (not read) |
| `chat gpt-4o-mini` (trace 2, first call, asks for the tool) | CLIENT | same | `orchestrate_tools` (trace 2) | `gen_ai.tool.definitions`, `gen_ai.response.finish_reasons=["tool_calls"]`, tokens 82/17 | `span_kind="llm"`, tokens 82/17 |
| `execute_tool read_file` | INTERNAL | same | `orchestrate_tools` (trace 2) | `gen_ai.operation.name=execute_tool`, `gen_ai.tool.name=read_file`, `gen_ai.tool.type=function`, `gen_ai.tool.call.id=call_m7_1`, `gen_ai.tool.description`, `gen_ai.tool.call.arguments` and `gen_ai.tool.call.result` (content on) | `span_kind="tool"`, `operation=execute_tool`, input and output previews in `extai-sens1` |
| `chat gpt-4o-mini` (trace 2, final answer) | CLIENT | same | `orchestrate_tools` (trace 2) | `gen_ai.output.messages` (text), finish `stop`, tokens 131/12 | `span_kind="llm"`, tokens 131/12, cost computed |

- Duplicates: no llm span is duplicated; each model call is one span. The `orchestrate_tools` span is a wrapper that carries the **summed usage of its children** on the tool-round trace only (213/29). A normalizer rule that reads usage from every span with `gen_ai.usage.*` would double count it. The current normalizer leaves it unread (run: `attributes_left` in `h.txt`).
- Noise spans: `orchestrate_tools` on both traces. It has no `gen_ai.operation.name`.
- Missing fields: none on llm spans. `gen_ai.response.finish_reasons` is present. The provider host is loopback in the run (`server.address=127.0.0.1`); against the real API it would be the API host.
- HTTP: 3 `POST /v1/chat/completions` (no-tools call, tool-round call, tool-result call), `Authorization: Bearer sk-fake...(17 chars)` on each (run: `http.jsonl`; `User-Agent: OpenAI/2.15.0 (.NET 10.0.15…) MEAI/11.0.0`).

### What the normalizer would need (facts only)

- Nothing for the llm spans: the `gen_ai.*` keys are read today. `gen_ai.provider.name=openai` is correct.
- The `orchestrate_tools` wrapper: a rule to keep it out of usage totals (it is kind `span`, and its usage duplicates its children). Today it is harmless; a rule that sums usage on any span would break it.
- `execute_tool` spans: `span_kind=tool` is already read (`gen_ai.tool.*`).

### Left loopback

- Every `connect()` in `extai-sens1` and `extai-default` went to `::ffff:127.0.0.1` (IPv4-mapped loopback), 2 connects per run (run: `runs/<name>/trace.strace`, parsed with `python3`). Nothing else. `real-home.txt` is empty in both runs.


## 4.2 Semantic Kernel .NET 1.82.0 (`Microsoft.SemanticKernel`)

### Program

`src/sk/Program.cs` (key lines):

```csharp
if (mode is "on" or "sensitive")
    AppContext.SetSwitch("Microsoft.SemanticKernel.Experimental.GenAI.EnableOTelDiagnostics", true);
if (mode == "sensitive")
    AppContext.SetSwitch("Microsoft.SemanticKernel.Experimental.GenAI.EnableOTelDiagnosticsSensitive", true);
using var tracerProvider = Sdk.CreateTracerProviderBuilder()
    .AddSource("Microsoft.SemanticKernel*")
    .AddOtlpExporter()
    .Build();
var builder = Kernel.CreateBuilder();
builder.AddOpenAIChatCompletion(modelId: "gpt-4o-mini", apiKey: "sk-fake-m8", endpoint: new Uri(baseUrl + "/v1"));
var kernel = builder.Build();
kernel.ImportPluginFromType<FilePlugin>();
var first = new ChatHistory(); first.AddUserMessage("Say hello in one sentence.");
var chatReply = await chat.GetChatMessageContentAsync(first, kernel: kernel);          // no tools sent
var settings = new OpenAIPromptExecutionSettings { FunctionChoiceBehavior = FunctionChoiceBehavior.Auto() };
var toolReply = await chat.GetChatMessageContentAsync(second, settings, kernel);        // tool round
```

`src/sk-log/` is the same program plus `Microsoft.Extensions.Logging` with a stderr provider (for § 4.2 ILogger finding).

### Setup and wiring

- Packages: `Microsoft.SemanticKernel` 1.82.0 (the umbrella package; it carries the OpenAI connector), `OpenTelemetry` 1.21.0, `OpenTelemetry.Exporter.OpenTelemetryProtocol` 1.21.0 (`src/sk/m8-sk.csproj`, lock `src/sk/packages.lock.json`).
- Build: `dotnet build --no-restore`, 0 warnings, 0 errors (run in this session).
- The OpenAI connector's endpoint is set by `endpoint: new Uri(baseUrl + "/v1")` (run: `http.jsonl`, `POST /v1/chat/completions`).
- OTLP: the same environment as § 4.1 (`runner/env-sk-*`): protobuf over HTTP to `/v1/traces`, header `Authorization: Bearer sk-cetra-fake-m8`.

### Switches (docs: package binary strings; run: the three modes)

- Names, read from the package's own assemblies (`lib/net*/Microsoft.SemanticKernel.Abstractions.dll` and `Microsoft.SemanticKernel.Connectors.OpenAI.dll`, UTF-16 user strings; the same two names in both):
  - `Microsoft.SemanticKernel.Experimental.GenAI.EnableOTelDiagnostics`
  - `Microsoft.SemanticKernel.Experimental.GenAI.EnableOTelDiagnosticsSensitive`
- Set in code with `AppContext.SetSwitch(name, true)` before the first SK call (run: all three modes). Environment-variable forms of the switches were not tested.
- Default: **both off, and chat spans are not emitted at all.** `sk-none`: one span, `execute_tool read_file`; the three model calls leave no span (run: `runs/sk-none/`).
- `EnableOTelDiagnostics` only: chat spans appear with metadata and tokens. No events and no content (`runs/sk-on/`).
- `EnableOTelDiagnosticsSensitive` too: content is in **span events** on the chat spans, and the tool span gets its arguments and result as attributes (`runs/sk-sensitive/`).

### Where content lands

- **Span events (traced, `EnableOTelDiagnosticsSensitive`).** Event names and shape from the dump (`out/sk-sensitive.dump.txt`):
  - `gen_ai.user.message`, `gen_ai.assistant.message`, `gen_ai.tool.message`, `gen_ai.choice`. Each event has one attribute `gen_ai.event.content` (a JSON string such as `{"role":"user","name":null,"content":"Say hello in one sentence.","tool_calls":[]}`), and `gen_ai.system=openai`.
  - The choice event's JSON uses a capitalized role (`"role":"Assistant"`) and `index`, `message` (`content`, `tool_calls`).
  - Event counts: first chat 2 events, the tool-asking chat 2, the final chat 4 (user, assistant, tool, choice).
  - This is the older GenAI event shape: the content is not in `gen_ai.input.messages` or `gen_ai.output.messages` attributes, and the span has no `events` other than these.
- **ILogger (log records in .NET terms, not OTLP logs).** SK writes the chat history and the function arguments and results through `ILogger` at Trace level. Samples from `runs/sk-log-sensitive/run.stderr`:
  - `Trace Microsoft.SemanticKernel.Connectors.OpenAI.OpenAIChatCompletionService : ChatHistory: [{"Role":{"Label":"user"},"Items":[{"$type":"TextContent","Text":"Say hello in one sentence."}]}]`
  - `Trace Microsoft.SemanticKernel.KernelFunction : Function FilePlugin-read_file result: hello from the M8 tool`
  - In `runs/sk-log-none` (both switches off) the same Trace lines appear: 4 of the 15 `ILOG` lines carry prompt or tool content. The switches do not gate `ILogger` output. The `ILogger` level does (this run set Trace; a host with the default Information filter would drop Trace; not run).
  - No `/v1/logs` post in any run: the program adds no OTel logging bridge. Exporting these `ILogger` records to OTLP needs one (not run). cetra does not read OTLP logs (D-CA-2).

### Spans received

Four spans per run (3 chat calls, 1 tool), each its own trace: no span has a parent. Raw kinds from `rawspans.py` (protobuf enum: 3 = CLIENT, 1 = INTERNAL).

| Span name | Kind | Scope | Parent | Attributes of note | Normalizer (cetra today, `h.txt`) |
|---|---|---|---|---|---|
| `chat gpt-4o-mini` (no tools) | CLIENT | `Microsoft.SemanticKernel.Diagnostics` | none | `gen_ai.operation.name=chat`, `gen_ai.system=openai` (not `gen_ai.provider.name`), `gen_ai.request.model`, `gen_ai.response.finish_reason=["Stop"]` (singular key, capitalized value), `gen_ai.response.id`, `gen_ai.usage.input_tokens=131`, `gen_ai.usage.output_tokens=12`, `server.address=http://127.0.0.1:<port>/v1` (a URL, not a host), `server.port` | `span_kind="llm"`, provider `openai`, tokens 131/12, `cost_total=2.685e-05` (computed); **no input or output** (in `sensitive` the content is in events, which the normalizer does not read) |
| `chat gpt-4o-mini` (asks for the tool) | CLIENT | same | none | as above, `finish_reason=["ToolCalls"]`, tokens 82/17; in `sensitive` 2 events | `span_kind="llm"`, tokens 82/17, cost computed; no I/O |
| `execute_tool read_file` | INTERNAL | `Microsoft.SemanticKernel` | none | `gen_ai.operation.name=execute_tool`, `gen_ai.tool.name=read_file` (the model sees `FilePlugin-read_file`), `gen_ai.tool.description`; in `sensitive` also `gen_ai.tool.call.arguments` (JSON) and `gen_ai.tool.call.result` (raw text) | `span_kind="tool"`, `operation=execute_tool`; in `sensitive` input and output previews are present; no `tool_name` shown in `h.txt` |
| `chat gpt-4o-mini` (final answer) | CLIENT | same | none | tokens 131/12, `finish_reason=["Stop"]`; in `sensitive` 4 events (user, assistant, tool, choice) | `span_kind="llm"`, tokens 131/12; no I/O |

- Duplicates: none (each model call is one span). The chat spans of the tool round are separate traces from the tool span.
- Noise: none.
- Missing: the chat spans' content (§ above; events are not read); no parent, so the tool span is not linked to the call that asked for it; `server.address` is a URL with scheme and path; `gen_ai.provider.name` is absent (`gen_ai.system=openai`).
- Default-off consequence: with no switch, cetra would see no model calls from SK at all, only tool spans.
- HTTP: request 1 sends no `tools` (`http.jsonl`: `tools: []`), requests 2 and 3 send `['FilePlugin-read_file']`, all with `Authorization: Bearer sk-fake...(17 chars)`.

### What the normalizer would need (facts only)

- The `gen_ai.user.message`, `gen_ai.assistant.message`, `gen_ai.tool.message` and `gen_ai.choice` events, with their `gen_ai.event.content` JSON, to fill input and output on chat spans (the `sensitive` mode only).
- `gen_ai.system` as the provider when `gen_ai.provider.name` is absent. It is `openai` here (the normalizer already reads it: provider `openai`).
- `gen_ai.response.finish_reason` (singular) and its capitalized values (`Stop`, `ToolCalls`) if the finish reason is to be read; today it is not read.
- `server.address` given as a URL needs the host part, if the provider-from-endpoint rule (D-IN-2) is to see it.

### Left loopback

- Every `connect()` in the SK runs went to `::ffff:127.0.0.1` (2 per run). Real-home: none in all five runs (`real-home.txt` empty). The same check passes for the § 4.1 runs.


## 4.3 Microsoft Agent Framework .NET 1.28.0 (`Microsoft.Agents.AI`)

### Program

`src/maf/Program.cs` (key lines):

```csharp
using var tracerProvider = Sdk.CreateTracerProviderBuilder()
    .AddSource("m8-maf")
    .AddSource(OpenTelemetryAgent.DefaultSourceName)
    .AddSource("Experimental.Microsoft.Extensions.AI")
    .AddOtlpExporter()
    .Build();
ChatClient chatClient = new OpenAIClient(new ApiKeyCredential("sk-fake-m8"),
    new OpenAIClientOptions { Endpoint = new Uri(baseUrl + "/v1") }).GetChatClient("gpt-4o-mini");
Action<OpenTelemetryAgent> configure = mode == "code" ? o => o.EnableSensitiveData = true : _ => { };
AIAgent plain = chatClient.AsAIAgent(name: "m8-agent", instructions: "Be brief.")
    .AsBuilder()
    .UseOpenTelemetry(sourceName: SourceName, configure: configure)
    .Build();
var chat = await plain.RunAsync("Say hello in one sentence.");                 // no tools
AIAgent withTool = chatClient.AsAIAgent(name: "m8-agent", instructions: "Be brief.",
        tools: [AIFunctionFactory.Create(ReadFile, "read_file")])
    .AsBuilder().UseOpenTelemetry(sourceName: SourceName, configure: configure).Build();
var round = await withTool.RunAsync("Read hello.txt and tell me what it says.");
```

The API is from the package's own XML docs (`lib/net10.0/Microsoft.Agents.AI.xml`): `OpenAIChatClientExtensions.AsAIAgent(ChatClient, instructions, name, description, tools, …)`, `AIAgentExtensions.AsBuilder`, and `OpenTelemetryAgentBuilderExtensions.UseOpenTelemetry(AIAgentBuilder, string sourceName, Action<OpenTelemetryAgent>)`. The docs say the chat spans are emitted below the function-invoking layer, so tool spans come from the agent source.

### Setup and wiring

- Packages: `Microsoft.Agents.AI` 1.28.0, `Microsoft.Agents.AI.OpenAI` 1.28.0, `OpenTelemetry` 1.21.0, `OpenTelemetry.Exporter.OpenTelemetryProtocol` 1.21.0 (`src/maf/m8-maf.csproj`, lock `src/maf/packages.lock.json`).
- Build: `dotnet build --no-restore`, 0 warnings, 0 errors.
- The OpenAI client is the same as § 4.1. The agent's OTel source name is the one passed to `UseOpenTelemetry`; the TracerProvider must `AddSource` the same name (docs, XML `UseOpenTelemetry` remarks). The chat spans and the tool spans were emitted on that source (scope `m8-maf` in every span).

### Content switch (docs and run)

- Code: `OpenTelemetryAgent.EnableSensitiveData` (default `false`), set in the `UseOpenTelemetry` `configure` callback (run: `maf-code`).
- Environment: `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true`. The XML docs say it sets the default, and an explicit code value overrides it (docs: `Microsoft.Agents.AI.xml`, property `EnableSensitiveData`). Run `maf-env` confirmed the env form: the same content as `maf-code`, with no code switch.
- Default: off. Run `maf-default`: no content attribute on any span.
- Where content lands: **span attributes**, not events and not log records. `events=0` on every span (`rawspans.py` output). The keys are `gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.system_instructions` (on chat spans), `gen_ai.tool.definitions` (on tool-round chat spans), `gen_ai.tool.call.arguments` and `gen_ai.tool.call.result`.

### Spans received

Six spans per run in two traces (one per `RunAsync`). Raw kinds (3 = CLIENT, 1 = INTERNAL):

| Span name | Kind | Scope | Parent | Attributes of note | Normalizer (cetra today, `h.txt`) |
|---|---|---|---|---|---|
| `invoke_agent m8-agent(<32-hex agent id>)` (no tools) | CLIENT | `m8-maf` | none (root) | `gen_ai.operation.name=invoke_agent`, `gen_ai.provider.name=openai`, `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.response.id`, `gen_ai.response.finish_reasons=["stop"]`, `gen_ai.agent.id`, `gen_ai.agent.name=m8-agent`, `gen_ai.usage.input_tokens=131`, `gen_ai.usage.output_tokens=12` (the same numbers as its child chat span), `server.address=127.0.0.1`, `server.port` (+ content in `code` and `env`) | `span_kind="agent"`, provider openai, model, operation `invoke_agent`; **tokens not read** (left in `attributes_left`); I/O present in `code` and `env` |
| `chat gpt-4o-mini` (no tools) | CLIENT | `m8-maf` | `invoke_agent` | `gen_ai.provider.name=openai`, `openai.api.type=chat_completions`, tokens 131/12, `gen_ai.response.finish_reasons=["stop"]`, `server.address=127.0.0.1` (a host, not a URL) (+ content) | `span_kind="llm"`, tokens 131/12, `cost_total=2.685e-05` (computed); I/O in `code` and `env` |
| `invoke_agent m8-agent(<id>)` (tool round) | CLIENT | `m8-maf` | none (root) | tokens **213/29** (the sum of the two model calls below), `gen_ai.output.messages` containing a `tool_call` part (+ content) | `span_kind="agent"`; tokens not read |
| `chat gpt-4o-mini` (asks for the tool) | CLIENT | `m8-maf` | `invoke_agent` (tool round) | `gen_ai.tool.definitions` (`read_file`), `finish_reasons=["tool_calls"]`, tokens 82/17 | `span_kind="llm"`, tokens 82/17, cost computed |
| `execute_tool read_file` | INTERNAL | `m8-maf` | `invoke_agent` (tool round) | `gen_ai.operation.name=execute_tool`, `gen_ai.tool.name=read_file`, `gen_ai.tool.type=function`, `gen_ai.tool.call.id=call_m7_1`, `gen_ai.tool.description`; + `gen_ai.tool.call.arguments` and `.result` (content on) | `span_kind="tool"`; I/O previews in `code` and `env` |
| `chat gpt-4o-mini` (final answer) | CLIENT | `m8-maf` | `invoke_agent` (tool round) | tokens 131/12, `finish_reasons=["stop"]` | `span_kind="llm"`, tokens 131/12, cost computed |

- Span names carry the agent instance id (`invoke_agent m8-agent(<32-hex>)`), which differs on every run. A rule that matches the name must use the prefix `invoke_agent `.
- Duplicates: the `invoke_agent` span carries the usage of its children (131/12 with one model call, 213/29 for the tool round). The chat children carry the same numbers. The normalizer does not read tokens on `agent` spans, so nothing is counted twice today. A rule that sums usage over every span would double count. In `code` and `env` the agent span and its chat child also carry the same I/O (duplicate content, not counted twice).
- Noise: none. Every span is an agent, model call, or tool.
- Missing fields: `gen_ai.system_instructions` (`Be brief.`) is not shown in the normalizer output printed by `h.py`; where it goes was not checked. `gen_ai.provider.name` is present, unlike SK.

### What the normalizer would need (facts only)

- Nothing for the `chat` spans: they are read.
- `invoke_agent` spans: `span_kind="agent"` is already read. Whether its tokens should count is the plan's decision; today they do not, and reading them would double count.
- Name matching for `invoke_agent` needs the prefix, not the full name.

### HTTP

- Requests: 3 `POST /v1/chat/completions`. The first has no `tools` (`tools: []`), the next two send `['read_file']`. Every request carries `Authorization: Bearer sk-fake...`. The first request's body is `{"messages":[{"role":"system","content":"Be brief."},{"role":"user",…}],"model":"gpt-4o-mini"}`, so instructions reach the model as a system message (`runs/maf-code/req/req-001.json`).

### Left loopback

- Every `connect()` in the three MAF runs went to `::ffff:127.0.0.1` (2 per run). Real-home: none (`real-home.txt` empty in all three).


## 5. Cross-program comparison (run evidence in § 4)

| | Microsoft.Extensions.AI 11.0.0 | Semantic Kernel 1.82.0 | Agent Framework 1.28.0 |
|---|---|---|---|
| Instrumentation entry | `ChatClientBuilder.UseOpenTelemetry(sourceName, configure)` | AppContext switches `Microsoft.SemanticKernel.Experimental.GenAI.EnableOTelDiagnostics` (+ `…Sensitive`) | `AIAgentBuilder.UseOpenTelemetry(sourceName, configure)` on the agent |
| Default | chat spans on, content off | **no chat spans**; only tool spans | spans on, content off |
| Content switch | `OpenTelemetryChatClient.EnableSensitiveData` (code) | the two AppContext switches (code only; env form not tested) | `OpenTelemetryAgent.EnableSensitiveData` (code) **or** env `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true` |
| Where content lands | span attributes `gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.tool.call.*` | span **events** `gen_ai.user.message` / `gen_ai.assistant.message` / `gen_ai.tool.message` / `gen_ai.choice` (`gen_ai.event.content` JSON); tool args/result are attributes | span attributes, as Extensions.AI, plus `gen_ai.system_instructions` |
| Normalizer I/O today | read (llm, tool, agent spans) | **not read** on chat spans (events); read on tool span in sensitive mode | read on llm, agent and tool spans |
| Tokens on llm spans | read | read | read |
| Provider attribute | `gen_ai.provider.name=openai` | `gen_ai.system=openai` only; `server.address` is a URL | `gen_ai.provider.name=openai` |
| Server address | host | `http://host:port/v1` (URL) | host |
| Finish reason key | `gen_ai.response.finish_reasons` (lowercase) | `gen_ai.response.finish_reason` (singular, `Stop`, `ToolCalls`) | `gen_ai.response.finish_reasons` (lowercase) |
| Tool span | `execute_tool <name>`, child of `orchestrate_tools` | `execute_tool <name>`, own root trace, no parent | `execute_tool <name>`, child of `invoke_agent` |
| Parent links | wrapper root → chat and tool children | **none** (every span is a root) | root `invoke_agent` → chat and tool children |
| Wrapper span usage | `orchestrate_tools` carries summed usage (tool round only) | none | `invoke_agent` carries summed usage (both cases) |
| Log records | none | `ILogger` Trace content, independent of the switches (no OTLP log export without a bridge; not run) | none (not tested) |
| Extra OTLP signals | none in runs | none in runs | none in runs |

Shared facts: the tool name in SK is `FilePlugin-read_file` (sent to the model) while the span name is `execute_tool read_file`; the fake provider returned its text for any text reply, so the answer text is not evidence of the tool round (the `tool_calls` finish reasons and the `execute_tool` spans are).

## 6. Capture-runner answers (facts only; the plan decides)

### 6.1 Restrictions in `tools/capture` that a .NET runner meets

- `tools/capture/src/run.rs` `program_env`: the program sees a cleared environment. Only `PATH`, `HOME` and every `UV_*` and `XDG_*` variable are copied from the parent. `DOTNET_ROOT`, `DOTNET_CLI_HOME`, `NUGET_PACKAGES`, `DOTNET_CLI_TELEMETRY_OPTOUT` and the like are dropped unless the runner adds them (run.rs lines 160–220).
- `tools/capture/src/scenario.rs` `check_env` (line ~452): every `scenario.env` key must start with `OTEL_`, `OPENINFERENCE_`, `TRACELOOP_`, `LANGFUSE_`, `LMNR_`, `ANTHROPIC_` or `ADK_`; no value may contain `sk-`, `pk-`, `Bearer ` or `@` (`ENV_FORBIDDEN`, line 54). So a `DOTNET_*` or `NUGET_*` key cannot be set through a scenario; a .NET runner needs a new prefix and a copy rule in `program_env`.
- The runner always sets `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_COMPRESSION=none`, `OTEL_TRACES_SAMPLER=always_on`, `OTEL_SERVICE_NAME=capture-app`, and the mock URL `CETRA_CAPTURE_MOCK_URL` (the program builds `…/v1` from it). The .NET exporter reads these standard variables (OTel spec; not run in the capture harness).
- Its fake keys are `capture-mock-key` for `OPENAI_API_KEY` and similar (run.rs `MOCK_KEY`).

### 6.2 Language and discovery

- `tools/capture/src/write.rs:22`: `enum Language { Python, Typescript }`. `Program::for_language` (run.rs ~44–80) builds argv `uv run --project … --frozen python main.py` or `node main.ts`. `discover` (run.rs ~100–150) scans `py/scenarios`, `ts/scenarios` and `envs/*/scenarios` (needs `pyproject.toml`). A .NET runner needs a third variant, a discovery root, and an argv (`dotnet <prebuilt dll>`); `Program::argv` exists for tests and can carry it.
- The program is spawned with the argv only; it must already be built. The build is outside the runner (a `just` recipe), as for the Python and TypeScript tools.

### 6.3 Meta contract

- `CETRA_CAPTURE_META` names a JSON file, written after the tracer provider is flushed: `{"libraries": {"<name>": "<version>", …}}` (`harness/src/capture_py/harness.py` `_write_meta`; `ts/harness.ts` line 48). `CETRA_CAPTURE_LIBRARIES` is a comma list of package names; `read_libraries` (run.rs ~610) rejects a missing file, unparsable JSON or a non-string version. A .NET program would read `CETRA_CAPTURE_LIBRARIES`, take each version from its own assembly, and write this file after `TracerProvider.Dispose()` (which flushes).

### 6.4 Build, restore and run offline (run evidence)

Commands, as run in `runner/offline-check.sh` (copy of the project into a run dir, then inside `netrun.sh`):

```
dotnet restore --locked-mode --nologo -p:NuGetAudit=false
dotnet build --no-restore --nologo -p:NuGetAudit=false
dotnet <bin>/Debug/net10.0/<name>.dll
```

with `NUGET_PACKAGES` pointing at a pre-filled cache, `NUGET_HTTP_CACHE_PATH` and `NUGET_PLUGINS_CACHE_PATH` empty and inside the run dir, `DOTNET_CLI_HOME` inside the run dir, and `RestorePackagesWithLockFile=true` in each csproj (lock files: `logs/extai-packages.lock.json`, `src/sk/packages.lock.json`, `src/maf/packages.lock.json`).

Results:
- `offline-extai` (run `runs/offline-extai`): restore rc=0 (5.8 s, with the audit warning), build rc=0, run rc=0; stdout matches § 4.1.
- `offline-extai` rerun with `-p:NuGetAudit=false`: restore rc=0 with 0 `NU1900` lines (`runs/offline-extai`).
- `offline-sk` and `offline-maf`: restore rc=0, build rc=0, run rc=0, correct stdout (CHAT and TOOL lines). The wrapper then exited 124 (the 300 s timeout in `netrun.sh`) even though `real-home` was clean.
- Empty-cache check (`runs/offline-extai-empty`): restore rc=1 with `NU1301` ("Name or service not known (api.nuget.org:443)"), so the cache is required; build and run then fail as well.

### 6.5 Wrapper timeout (exit 124)

- The program and the build finished. The trace of `offline-sk` shows a Roslyn build server, `sdk/sdk/10.0.404/Roslyn/bincore/VBCSCompiler`, started by the build and still running. `strace -f` kept waiting on it, and it was stopped by SIGTERM at the timeout (the trace's last lines are `+++ killed by SIGTERM +++`, all from this run's own processes).
- Rerun: `runs/offline-sk-noserver` with `-p:UseSharedCompilation=false -nodeReuse:false` on the build and `dotnet build-server shutdown` before the program starts (`runner/offline-check-noserver.sh`). Result: `runs/offline-sk-noserver` restore rc=0, build rc=0, shutdown done, run rc=0, **wrapper rc=0** (previously 124). The trace still opens `VBCSCompiler.dll` (from the build or the shutdown call; not separated), and no compiler process was left running afterwards. Both changes were applied at once, so which one ended the wait is not isolated. Open: one run with only `build-server shutdown`, or only `UseSharedCompilation=false`, would separate them.

### 6.6 Cache and download sizes (measured)

- NuGet cache for the three programs: `nuget/packages` 305 MB extracted (95 package versions, `.nupkg` files 61.7 MB total). A cache for one program is smaller; the three together share most packages.
- .NET SDK 10.0.404: tarball 231.9 MB; installed 626 MB (`sdk/`). The shared runtime `shared/` is 105 MB, enough to run `dotnet <dll>` (the runner would not need the SDK for runs, only for builds).
- Offline restore needs the cache: an empty-cache restore inside the namespace fails with `NU1301` (`runs/offline-extai-empty`).

### 6.7 Versions and reproducibility notes

- `NU1900` (NuGet audit: "Unable to load the service index") is a warning in the namespace; set `-p:NuGetAudit=false` for offline runs, or `<NuGetAudit>false</NuGetAudit>` in the csproj.
- Restore with `--locked-mode` used the committed lock files; no package resolved to a different version.

## 7. Open questions (need the user or the plan)

1. Semantic Kernel: is the SK page setup-only (two AppContext switches, then the events shape), or should the normalizer read `gen_ai.*` events? With the switches off SK emits no chat spans at all, so an SK page must say so.
2. Capture runner: a .NET runner needs a `Language` variant, a discovery root and env pass-through (`DOTNET_*`, `NUGET_*`, which `program_env` and `scenario.env` currently block). Does the plan take that change in `tools/capture`?
3. Wrapper timeout: which invocation ended the wait (`build-server shutdown` or `UseSharedCompilation=false`)? One rerun with only one change would settle it (§ 6.5).
4. Not run: the OTLP logs bridge for SK's `ILogger` records; the SK environment-variable switch forms; Agent Framework's `ILogger` output; every Java, Go, Ruby, Rust and C++ runner.

## 8. Network and downloads

Outside loopback (all deliberate):
- `https://builds.dotnet.microsoft.com/dotnet/release-metadata/releases-index.json`, `…/10.0/releases.json` (metadata, `logs/`).
- `https://builds.dotnet.microsoft.com/dotnet/Sdk/10.0.404/dotnet-sdk-10.0.404-linux-x64.tar.gz`, 231.9 MB, sha256 `c0610757a818fde4b960d27a0ac421f273068a05dc5ec7abafc01dbf84e9cb4d` (sha512 matches `releases.json`), into `dl/`.
- nuget.org: 95 `.nupkg` files for the three programs (sha256 and sizes in `logs/nuget-hashes.tsv`), and the registration indexes `logs/idx-*.json` and `logs/nuget_versions.py` output (`logs/nuget-versions.txt`).
- `dotnet restore` (online) for each project, outside the namespace.

Inside the namespace: every program run and the offline checks. Every `connect()` went to `::ffff:127.0.0.1` (AF_INET6 mapped loopback); the AF_UNIX connects in the offline check (`/var/run/nscd/socket`, MSBuild pipes) are local. No real-home touch remained after the `PWD` fix.

Not done: no commit, no push, no build in the repo, no change to any repo file except this spike.
