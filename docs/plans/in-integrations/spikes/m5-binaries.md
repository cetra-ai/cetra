# M5 spike 2: gateway and Temporal binaries run for real (Bifrost, Envoy AI Gateway, Kong skipped, Temporal)

Run 2026-11-23/24 (M5), worktree `integrations` at `2ec7d25`; second spike after `m5-gateways.md` (docs-only triage of 15 gateways, LiteLLM run).
The user approved (2026-11-23) downloading and running the Temporal dev server, Bifrost, Kong and `aigw`, from official sources only.
Scratch: `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m5bin/` (`py/` harness, `norm/` = the real `cetra_otel::normalize` through a path dependency on this worktree, `out_*/` runs, `temporal-py/`, `capcopy/`).

## Setup common to every run

- **Harness** (`py/common.py`): an in-process fake OpenAI server (chat completions, one text turn 31/12 tokens, a tool-call turn 82/17 when `tools` is sent, Responses API for Temporal, `GET /v1/models` answers 404, `FAKE_FAIL=1` answers 500), an in-process OTLP/HTTP receiver that saves each POST body and headers, and a driver that starts the binary as a subprocess, waits for readiness, sends one chat request with the OpenAI Python client (`openai` 3.51.0 for Bifrost and Envoy, 3.45.0 for Temporal, the capture project's pin) and sends `SIGINT`. `dump.py` decodes the protobuf bodies and prints every span; `norm/` runs cetra's real normalizer with the built-in price table.
- **Offline proof.** Every run is inside `unshare -rn` (a network namespace with only loopback) under `strace -f -e trace=connect`: nothing leaves the machine, and any outbound attempt shows as a failed `connect` or a DNS lookup in the gateway's log. (`unshare -rnm` plus a bind mount over `/root` and a tmpfs `/mnt` for `aigw`, see below.) With the configs below, **no run made a DNS lookup or any non-loopback connection** (Bifrost needs three config keys for that, finding 1).
- **Docker.** `docker info` cannot run: there is no `docker` (or `podman`, `nerdctl`) binary on this machine (`/var/run/docker.sock` is a dangling symlink). **Kong was skipped** as instructed; its section is docs and registry metadata only.

Downloads (all verified against the vendor's own checksum; versions released on or before 2026-11-16 where the vendor published one):

| Tool | Version (released) | URL | Size (bytes) | SHA-256 |
|---|---|---|---|---|
| Temporal CLI (dev server) | 1.10.1 (2026-11-13; server 1.32.2, UI 2.56.4) | `https://github.com/temporalio/cli/releases/download/v1.10.1/temporal_cli_1.10.1_linux_amd64.tar.gz` | 38,357,804 | `4c1500e2ce2d4de136e5fe8852d9b7f82eb62f19b6b383b3e0c040519a9d20b1` (matches the release's `digest` and `checksums.txt`). The extracted `temporal` is 125,718,690 bytes, sha256 `960afddde142595ea4bf6c74f0213b4634a4ef2ef8778c2e930af2cce30d7c8a` |
| Bifrost gateway (`bifrost-http`) | v2.5.1 (transports tag, 2026-11-10). v2.6.0 is 2026-11-19, too new | `https://downloads.getmaxim.ai/bifrost/v2.5.1/linux/amd64/bifrost-http` (the host Bifrost's own `npx @maximhq/bifrost` launcher downloads from; the GitHub releases of `maximhq/bifrost` carry no binaries) | 152,623,104 | `2b629808bdd54dca5abb38f0e62f6250e7fbd8c336c45d568d83d683974c0734` (matches the published `bifrost-http.sha256` next to it) |
| Envoy AI Gateway CLI (`aigw`) | v1.2.1 (2026-11-06; v1.3.0 is 2026-11-18, too new) | `https://github.com/envoyproxy/ai-gateway/releases/download/v1.2.1/aigw-linux-amd64` (the repository now redirects to `github.com/theagentrouter/agent-router`: the project was renamed "Agent Router"; the redirect is GitHub's repo transfer and the digest below is the release's own) | 296,261,464 | `b97af14fc3f116ccbdce7cdb7ae98da999f41623285bbf253095f09d214523d7` (matches the release `digest`) |
| Envoy proxy (needed by `aigw`) | 1.39.1 (2026-08-27, the version aigw 1.2.1 pins) | `https://github.com/envoyproxy/envoy/releases/download/v1.39.1/envoy-1.39.1-linux-x86_64` | 105,039,832 | `002c6e1c69ed0fa0ea381887247cadadfaec9481375fa8d8d2b1731eeabf40b8` (matches the release `digest`) |
| Kong | not downloaded (no Docker) | see the Kong section | | |

**Not in the approved list, flagged:** `aigw run` downloads Envoy by itself on first start, through the vendored `tetratelabs/func-e` v1.6.0 from `archive.tetratelabs.io` (Tetrate's archive, not the vendor org). I did not let it: I fetched Envoy 1.39.1 from the `envoyproxy/envoy` GitHub release (vendor org, one more file, 100 MiB) and pre-placed it where func-e looks, `$AIGW_DATA_HOME/envoy-versions/1.39.1/bin/envoy`, with `ENVOY_VERSION=1.39.1`; aigw then logs `1.39.1 is already downloaded`. If the user wants no Envoy download, `aigw` cannot run.

PyPI (Temporal, `exclude-newer = 2026-11-17T22:00:00Z`, Python 3.13): `temporalio` 1.36.1, `temporalio-openai-agents[openinference]` **2.0.0**, `openai-agents` 0.24.0, `openai` 3.45.0, `openinference-instrumentation-openai-agents` 2.7.0, `opentelemetry-sdk` and the OTLP/HTTP exporter 1.45.2 (what `temporal-py/pyproject.toml` pins). Bifrost and Envoy runs: `openai` 3.51.0, `opentelemetry-proto` 1.45.2 (decoding only).

## 1. Bifrost v2.5.1 (run)

### Config that works (files and flags)

```json
// <app-dir>/config.json   (flags: -app-dir <dir> -port <port> -host 127.0.0.1 -log-style pretty)
{
  "client":    {"enforce_auth_on_inference": false},
  "framework": {"pricing": {
      "pricing_url": "file:///<app-dir>/pricing.json",
      "model_parameters_url": "file:///<app-dir>/model-parameters.json",
      "pricing_sync_interval": 86400,
      "mcp_library_sync_interval": 0}},
  "providers": {"openai": {
      "keys": [{"name": "k1", "value": "env.UPSTREAM_KEY", "models": ["*"], "weight": 1.0}],
      "network_config": {"base_url": "http://127.0.0.1:<mock-port>"}}},
  "plugins": [{"enabled": true, "name": "otel", "config": {
      "service_name": "bifrost",
      "collector_url": "http://127.0.0.1:<receiver-port>/v1/traces",
      "trace_type": "genai_extension", "protocol": "http",
      "headers": {"Authorization": "env.CETRA_OTLP_AUTH"}}}]
}
```

- Client: `OpenAI(base_url="http://127.0.0.1:<port>/v1")`, model `openai/gpt-4o-mini` (`<provider>/<model>`; the upstream sees `gpt-4o-mini`). `POST /v1/chat/completions` on the gateway; the mock saw `POST /v1/chat/completions` with `Authorization: Bearer <key>` from the provider key.
- `pricing.json` and `model-parameters.json` are the two small files of the vendor's own example `examples/configs/withlocalpricingfiles/` (v2.5.1: `gpt-4o`, `gpt-4o-mini`, `gpt-4.1`, `claude-sonnet-4-20250514`, `text-embedding-3-small`).
- `headers` values: `env.NAME` reads an environment variable; a literal `"Bearer literal-key"` works too (run `out_bifrost8`: the receiver saw it). The `profiles: [{... "enabled": true}]` form (v1.5.8+) also works (`out_bifrost9`).
- The receiver saw `POST /v1/traces`, `Content-Type: application/x-protobuf`, `User-Agent: Go-http-client/1.1`, `Authorization: Bearer test-key`, an **uncompressed** body (it advertises `Accept-Encoding: gzip` only for the response).
- `disable_content_logging: true` in the otel plugin config removes `gen_ai.input.messages` and `gen_ai.output.messages` from both spans (run `out_bifrost_B`); the usage, cost and model keys stay.

### Start-up, phone-home, flush, nondeterminism

| Fact | Measured |
|---|---|
| Ready (`GET /health` 200) | 1.0 s offline with the config above; 1.75 s online with `getbifrost.ai` reachable and no local pricing; **8.0 s** offline with local pricing but without `mcp_library_sync_interval: 0` (the MCP catalog lookup times out); offline with **no** local pricing it dies at start (see below) |
| Export after the request | first POST 0.14 to 0.16 s after the response (one POST holds both spans and the 24 plugin spans) |
| Stop | `SIGINT` flushes: a run stopped immediately after the request had 0 posts before the stop and 1 after (`out_bifrost10`); exit is immediate |
| Phones home | **Yes, three things, all at start**: (1) the pricing datasheet from `getbifrost.ai` (`https://getbifrost.ai/datasheet`, model parameters `.../datasheet/model-parameters`): when it cannot be reached and no data is stored, **the gateway dies** (`FTL failed to initialize pricing manager ... lookup getbifrost.ai`); `framework.pricing.pricing_url` and `model_parameters_url` with a `file://` URL replace it. (2) the MCP library catalog (`https://getbifrost.ai/mcp-library`), only a warning; `mcp_library_sync_interval: 0` disables it. (3) `GET /v1/models` on the provider's `base_url`, **twice**, at start (also with `models: ["gpt-4o-mini"]`); a 404 or HTML answer is a warning ("falling back onto the static datasheet"). The schema also lists a `telemetry` key (not examined); with the config above `strace` shows no connection beyond loopback and **zero DNS lookups** |
| Local state | `config.db` (SQLite, 1.6 MB), `logs.db`, the pricing files in `-app-dir`; first start logs "No admin account ... /api surface is locked" warnings (harmless) |
| Inference auth | `client.enforce_auth_on_inference` defaults to **true**: without `false`, the request fails `401 virtual key is required` |
| Nondeterminism | Trace and span ids, timestamps, durations; `bifrost.request.id` (uuid), `bifrost.selected_key.id` (uuid), `bifrost.overhead.duration_ms`, `bifrost.upstream.duration_ms`, `bifrost.worker.handoff_ms`, `service.instance.id` (**the machine's hostname**, e.g. the user's host name: needs scrubbing; `scrub.rs` already rewrites `service.instance.id` and `host.name`). The span names, kinds and attribute key sets were **identical across three runs** (`sig.py`: 26 spans each) |
| Gateway behaviour with the OpenAI client's `traceparent` | Honoured: both Bifrost spans joined the client's trace and the root's parent is the client's span id (`out_bifrost12`). With the app also instrumented, the same call is therefore two LLM spans in one trace (finding 5) |

### Span dump (one chat request, `gpt-4o-mini`, `out_bifrost3`)

Resource: `service.name=bifrost`, `service.version=v2.5.1`, `telemetry.sdk.name=bifrost`, `telemetry.sdk.language=go`. Scope `bifrost@v2.5.1` on every span. 26 spans: the root, 24 `plugin.<name>.{prerequesthook,prehook,posthook}` INTERNAL spans (8 plugins: telemetry, prompts, logging, governance, routing, otel, compat, model-catalog-resolver; no attributes, 0.0 ms), and the LLM call. Values are verbatim (long ones cut at 120).

```
### #0  `/v1/chat/completions`   kind=SERVER   parent=-   status=OK
  bifrost.overhead.duration_ms [double] = 108.909898      bifrost.request.id [str] = c3403929-a3e9-47dc-8a18-775ffe9eb3d9
  bifrost.upstream.duration_ms [double] = 1.231382         bifrost.worker.handoff_ms [double] = 0.03757
  gen_ai.input.messages [str] = What is the weather in Paris?        <- a bare string: the LAST user message only
  gen_ai.output.messages [str] = It is 18C and sunny in Paris.       <- a bare string
  gen_ai.provider.name = openai   gen_ai.request.model = gpt-4o-mini   gen_ai.response.model = gpt-4o-mini-2024-07-18
  gen_ai.response.finish_reasons [array] = ["stop"]
  http.method = POST   http.request.method = POST   http.response.status_code [int] = 200   http.status_code [int] = 200
  http.route = /v1/chat/completions   http.url = /v1/chat/completions   url.path = /v1/chat/completions   url.scheme = http
  http.user_agent = OpenAI/Python 3.51.0   user_agent.original = OpenAI/Python 3.51.0   service.instance.id = <hostname>
### #17 `chat gpt-4o-mini`       kind=CLIENT   parent=#0  status=OK   (24 plugin spans are children of #0 or #17)
  bifrost.cost.input [double] = 4.65e-06    bifrost.cost.input.text [double] = 4.65e-06
  bifrost.cost.output [double] = 7.2e-06    bifrost.cost.output.text [double] = 7.2e-06
  bifrost.fallback_index [int] = 0   bifrost.provider.name = openai   bifrost.retries [int] = 0
  bifrost.selected_key.id = 38c914f9-efe2-4ed1-b10b-d3f8641d529b   bifrost.selected_key.name = k1
  gen_ai.input.messages [str] = [{"role":"system","content":"You are a weather bot."},{"role":"user","content":"What is the weather in Paris?"}]   <- OpenAI chat shape
  gen_ai.output.messages [str] = [{"role":"assistant","content":"It is 18C and sunny in Paris."}]
  gen_ai.operation.name = chat   gen_ai.provider.name = openai   gen_ai.request.message_count [int] = 2
  gen_ai.request.model = gpt-4o-mini   gen_ai.request.user = user-1
  gen_ai.response.created [int] = 1   gen_ai.response.finish_reason = stop   gen_ai.response.finish_reasons [array] = ["stop"]
  gen_ai.response.id = chatcmpl-1   gen_ai.response.model = gpt-4o-mini-2024-07-18   gen_ai.response.object = chat.completion
  gen_ai.usage.cost [double] = 1.185e-05
  gen_ai.usage.input_tokens [int] = 31   gen_ai.usage.output_tokens [int] = 12   gen_ai.usage.total_tokens [int] = 43
  request.type = chat_completion
```

With `tools` in the request (`out_bifrost4`): the `chat` span adds `gen_ai.request.tools = [{"name":"get_weather","description":"Get weather"}]` (no parameters), finish `tool_calls`, usage 82/17, cost 2.25e-05, and the output message is `[{"role":"assistant","content":"","tool_calls":[{"id":"call_1","type":"function","name":"get_weather","args":"{\"city\":\"Paris\"}"}]}]`: the tool call is **flattened** (`name` and `args` on the element, no `function` object, `args` not `arguments`). The root's output holds the same JSON.

On an upstream 500 (`out_bifrost_A`; the OpenAI client retried twice, so three traces): both spans `status=ERROR`, with `bifrost.error.type=provider_server_error`, `error.type=server_error`, `gen_ai.error=<message>`, `gen_ai.error.code=boom`, `http.response.status_code=500`; no usage, no cost, no events.

### Mapping through spec 02 (real normalizer, `out_bifrost3`)

| Span | kind | provider | request / response model | tokens in/out/total | cost | input | output |
|---|---|---|---|---|---|---|---|
| `/v1/chat/completions` (SERVER root) | **llm** (model fallback) | openai | gpt-4o-mini / gpt-4o-mini-2024-07-18 | none | none | "What is the weather in Paris?" | "It is 18C and sunny in Paris." |
| `chat gpt-4o-mini` (CLIENT) | llm (`gen_ai.operation.name=chat`) | openai (`gen_ai.provider.name`) | gpt-4o-mini / gpt-4o-mini-2024-07-18 | 31/12/43 | **reported** `cost_total` 1.185e-05 (`gen_ai.usage.cost`); `cost_input` and `cost_output` null | OpenAI chat messages converted | converted |
| 24 `plugin.*` spans | span | | | | | | |

The tool-call turn: the `chat` span has 82/17/99, reported 2.25e-05, and an output whose parts are `[{"type":"text","content":""},{"id":"call_1","type":"function","name":"get_weather","args":"{...}"}]`: the second part is **not** a canonical `tool_call` part (cetra stores the raw element; `output_preview` is empty). Errors: both spans `llm`, status `error`, cost `none`.

Exact keys, by field: kind `gen_ai.operation.name`; provider `gen_ai.provider.name` (Bifrost's provider id, the key under `providers` in `config.json`, here `openai`; **only `openai` was run**: another id (`anthropic`, `azure`, `bedrock`, `vertex`, `gemini`, a custom name such as `vllm-local`) was not observed); model `gen_ai.request.model` (the model without the `openai/` prefix) and `gen_ai.response.model`; tokens `gen_ai.usage.{input,output,total}_tokens`; cost `gen_ai.usage.cost` (total) plus `bifrost.cost.input` and `bifrost.cost.output` (and `.text` copies); content `gen_ai.input.messages` and `gen_ai.output.messages` (JSON, OpenAI chat shape, on the `chat` span).

### Normalizer gaps for Bifrost (exact)

1. **Root span counted as an LLM call.** `/v1/chat/completions` (SERVER, no `gen_ai.operation.name`) is `llm` by the "model fallback" row (it has `gen_ai.request.model`); it has no tokens and no cost, so no cost is double counted, but every Bifrost call is **two `llm` spans** (counts, latency charts, the trace's "LLM calls"). Needs a rule (proposed rule B1).
2. **Flattened tool calls** in `gen_ai.output.messages` (and, on the next turn, probably in the history of `gen_ai.input.messages`: not run) are stored as a bogus part (proposed rule B2).
3. **`bifrost.cost.input` / `bifrost.cost.output` are not read**, so a reported-cost span has `cost_total` but null `cost_input` and `cost_output` (proposed rule B3). Their sum equals `gen_ai.usage.cost` (4.65e-06 + 7.2e-06 = 1.185e-05).
4. `gen_ai.request.user` (the request's `user`) is not read as the user id (optional, as `llm.user` for LiteLLM).
5. **Double counting with an instrumented client** (not a normalizer bug): the gateway honours `traceparent`, so an app with OpenAI instrumentation plus the gateway has two llm spans, parent and child, both with tokens and cost. The page must say so (see "Page notes").
6. 24 plugin spans per request stay `span`; the capture keeps only the two LLM-relevant spans with `span_names` (the runner supports it).
7. Other providers' ids, streaming, embeddings and the Responses/Anthropic routes were not run.

### Capture approach (Bifrost)

A Python scenario program: pick two free ports; write `app/config.json`, `app/pricing.json`, `app/model-parameters.json` from a template (the pricing files committed in the scenario directory, so the reported cost is stable); `subprocess.Popen([bin, "-app-dir", app, "-port", p, "-host", "127.0.0.1", "-log-style", "pretty"], env={"CETRA_OTLP_AUTH": "Bearer capture", "UPSTREAM_KEY": "sk-capture", ...})` with `collector_url` = `CETRA_CAPTURE_OTLP_ENDPOINT` and the provider `base_url` = `CETRA_CAPTURE_MOCK_URL`; poll `GET /health` until 200 (timeout 30 s); `OpenAI(base_url=f"http://127.0.0.1:{p}/v1").chat.completions.create(model="openai/gpt-4o-mini", ...)`; `SIGINT` and `wait()` (flushes before exit); write the version meta; exit. Scenario fields: `span_names: ["/v1/chat/completions", "chat gpt-4o-mini"]` (or only the second), `volatile_attributes: ["bifrost.request.id", "bifrost.selected_key.id"]`, `timing_attributes: ["bifrost.overhead.duration_ms", "bifrost.upstream.duration_ms", "bifrost.worker.handoff_ms"]`, one `openai.chat` turn.

## 2. Kong (not run)

`docker info` is unavailable, so nothing was pulled. Registry metadata only (Docker Hub API, no pull): the official **`kong`** (OSS) image's newest tag is **3.9.5** (pushed 2026-11-20; 3.9.4 was released 2026-10-12), amd64 compressed size about 120 MB; `kong/kong-gateway` (the enterprise image, runs in free mode without a licence) has **3.13.0.x** builds (e.g. `3.13.0.13`, 2026-11-19, and a 3.4.3.30 line rebuilt on 2026-11-23). The first spike read the docs: the OpenTelemetry Gen AI attributes (`gen_ai.*` on `ai-proxy` spans) appear from **3.13**, which exists only in `kong/kong-gateway`; the OSS image's newest tag is 3.9.5 (Docker Hub, observed); whether its `opentelemetry` plugin writes Gen AI attributes is unverified. So a Kong capture would need `kong/kong-gateway:3.13.x` (free mode: whether `ai-proxy` and `opentelemetry` run unlicensed is **unverified**) in DB-less mode with a declarative `kong.yml` (`ai-proxy` with `route_type: llm/v1/chat`, `model.provider: openai`, `model.options.upstream_url: <mock>/v1/chat/completions`; `opentelemetry` with `traces_endpoint` and `headers`; `KONG_TRACING_INSTRUMENTATIONS=request` and `KONG_TRACING_SAMPLING_RATE=1`, `KONG_ANONYMOUS_REPORTS=off` to stop its usage pings). All of that is unrun. **Verdict: skip Kong for M5's captures** (Docker is a new required tool for the capture project, a 135 to 210 MB image, and the Gen AI path needs the enterprise image); the page documents the setup from the vendor docs, labelled "not captured", or is dropped. A user with Docker can run the spike in about an hour.

## 3. Envoy AI Gateway `aigw` v1.2.1 + Envoy 1.39.1 (run)

### Config that works (files and flags)

`aigw run <config.yaml> --admin-port <port>`; no `--debug`. The config is Gateway API and Envoy Gateway resources (the vendor's `examples/aigw/ollama.yaml` with three changes: the listener `port`, the Backend as `ip: {address: 127.0.0.1, port: <mock-port>}` instead of `fqdn`, and the `EnvoyProxy` access-log block dropped). Full file used:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata: {name: aigw-run}
spec: {controllerName: gateway.envoyproxy.io/gatewayclass-controller}
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: aigw-run, namespace: default}
spec:
  gatewayClassName: aigw-run
  listeners: [{name: http, protocol: HTTP, port: <listener-port>}]
  infrastructure: {parametersRef: {group: gateway.envoyproxy.io, kind: EnvoyProxy, name: envoy-ai-gateway}}
---
apiVersion: gateway.envoyproxy.io/v1alpha1
kind: EnvoyProxy
metadata: {name: envoy-ai-gateway, namespace: default}
spec: {logging: {level: {default: error}}}
---
apiVersion: aigateway.envoyproxy.io/v1beta1
kind: AIGatewayRoute
metadata: {name: aigw-run, namespace: default}
spec:
  parentRefs: [{name: aigw-run, kind: Gateway, group: gateway.networking.k8s.io}]
  rules:
    - matches: [{headers: [{type: RegularExpression, name: x-ai-eg-model, value: ".*"}]}]
      backendRefs: [{name: openai, namespace: default}]
      timeouts: {request: 120s}
  llmRequestCosts:
    - {metadataKey: llm_input_token, type: InputToken}
    - {metadataKey: llm_output_token, type: OutputToken}
    - {metadataKey: llm_reasoning_token, type: ReasoningToken}
---
apiVersion: gateway.envoyproxy.io/v1alpha1
kind: Backend
metadata: {name: openai, namespace: default}
spec: {endpoints: [{ip: {address: 127.0.0.1, port: <mock-port>}}]}
---
apiVersion: aigateway.envoyproxy.io/v1beta1
kind: AIServiceBackend
metadata: {name: openai, namespace: default}
spec:
  timeouts: {request: 3m}
  schema: {name: OpenAI}
  backendRef: {name: openai, kind: Backend, group: gateway.envoyproxy.io, namespace: default}
---
apiVersion: gateway.envoyproxy.io/v1alpha1
kind: ClientTrafficPolicy
metadata: {name: client-buffer-limit, namespace: default}
spec:
  targetRefs: [{group: gateway.networking.k8s.io, kind: Gateway, name: aigw-run}]
  connection: {bufferLimit: 50Mi}
---
apiVersion: v1
kind: Secret
metadata: {name: openai-apikey, namespace: default}
type: Opaque
stringData: {apiKey: "${OPENAI_API_KEY}"}          # aigw expands ${ENV} in the file
---
apiVersion: aigateway.envoyproxy.io/v1beta1
kind: BackendSecurityPolicy
metadata: {name: openai-apikey, namespace: default}
spec:
  targetRefs: [{group: aigateway.envoyproxy.io, kind: AIServiceBackend, name: openai}]
  type: APIKey
  apiKey: {secretRef: {name: openai-apikey}}
```

Environment: `OPENAI_API_KEY=<key>` (read by the secret above), `ENVOY_VERSION=1.39.1`, and the standard OTel variables the **capture runner already sets**: `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:<receiver>/v1/traces`, `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`, `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer test-key`, `OTEL_SERVICE_NAME` (default `aigw`), `OTEL_METRICS_EXPORTER=none`, `OTEL_LOGS_EXPORTER=none`; `OTEL_TRACES_EXPORTER=console` prints spans to stdout instead (not run beyond start-up). Directories: `AIGW_CONFIG_HOME`, `AIGW_DATA_HOME` (must contain `envoy-versions/1.39.1/bin/envoy`), `AIGW_STATE_HOME`, `AIGW_RUNTIME_DIR` (default `/tmp/aigw-$UID`; **it holds a Unix socket, so its path must stay under about 100 characters**: with the scratchpad path aigw fails `listen unix ...uds.sock: bind: invalid argument`). Without a config file, `aigw run` generates one from `OPENAI_API_KEY` and `OPENAI_BASE_URL`, but its listener is the fixed port 1975.

Request: `OpenAI(base_url="http://127.0.0.1:<listener>/v1")`, model sent as is (`gpt-4o-mini`; the route matches the `x-ai-eg-model` header aigw derives from the body). It also serves the Anthropic Messages API at `/anthropic/v1/messages` (run, below) and registers processors for `/v1/completions`, `/v1/embeddings`, `/v1/responses`, audio, images, cohere rerank and `/v1/models` (answered locally: `{"data":[],"object":"list"}`).

### Start-up, phone-home, flush, nondeterminism

| Fact | Measured |
|---|---|
| Ready | admin `/health` at 0.5 s, the data port answers at 2.0 s |
| Export after the request | first POST **2.4 to 2.5 s** after the response (the SDK's default batch delay); `OTEL_BSP_SCHEDULE_DELAY=100` makes it immediate; **`SIGINT` flushes** (a run stopped right after the request: 0 posts before, 1 after) and aigw exits in 0.03 s with code 0, leaving no `envoy` process |
| Phones home | Only the Envoy download (above). With the binary pre-placed: **no DNS lookup and no non-loopback connection** (`strace`); Envoy dials its control plane on `0.0.0.0:18000`. No telemetry flag exists in `aigw run --help` |
| Ports | **Many are fixed and not configurable in the generated bootstrap**: listening sockets seen: 1064 (`--admin-port`), 9856 (MCP proxy, always on), 1061, 18000, 18002, 19000, 19001, 19003, 8081, the Gateway listener (any), plus one ephemeral. Two concurrent aigw instances on a host collide; a second one cannot start while the first runs |
| Local files | `$AIGW_STATE_HOME/runs/<ts>/` (generated Envoy Gateway config), `envoy-runs/<ts>/` logs, `$AIGW_RUNTIME_DIR/<ts>/uds.sock`; Envoy Gateway also writes certificates to `$HOME/.config/envoy-gateway/certs` (it failed `mkdir /root/.config: permission denied` where `$HOME` was not writable; in my sandbox I bind-mounted a writable directory over `/root`: with a real user account `HOME` works) |
| Nondeterminism | Only the ids, timestamps and durations (the one span's key set was identical in two plain runs; `resource` has `service.name=aigw`, `telemetry.sdk.*`, **no `service.instance.id`**). Nothing in the attributes varies. Content: `input.value` and `output.value` carry the full request and response JSON (the response `id` `chatcmpl-1` came from the mock) |
| `traceparent` | Honoured: the span joined the client's trace with the client's span as parent (`out_aigw9`). Same double counting caveat as Bifrost |

### Span dump (chat, `out_aigw1`)

One span per request, no Envoy HTTP span, no plumbing: resource `{service.name: aigw, telemetry.sdk.language: go, telemetry.sdk.name: opentelemetry, telemetry.sdk.version: 1.46.0}`, headers `User-Agent: OTel OTLP Exporter Go/1.46.0`, `Authorization: Bearer test-key`.

```
### #0 `ChatCompletion` scope=envoyproxy/ai-gateway (no version) kind=INTERNAL parent=- status=OK dur_ms=39.0
  input.mime_type = application/json
  input.value = {"model":"gpt-4o-mini","messages":[{"role":"system","content":"You are a weather bot."},{"role":"user","content":"What is the weather in Paris?"}],"user":"user-1"}
  llm.input_messages.0.message.{role,content} = system / You are a weather bot.     (also .1 = user / What is the weather in Paris?)
  llm.invocation_parameters = {"model":"gpt-4o-mini","user":"user-1"}
  llm.model_name = gpt-4o-mini-2024-07-18          <- the RESPONSE model on success; the request model when the call failed
  llm.output_messages.0.message.{role,content} = assistant / It is 18C and sunny in Paris.
  llm.system = openai
  llm.token_count.{prompt,completion,total} = 31 / 12 / 43
  openinference.span.kind = LLM
  output.mime_type = application/json
  output.value = {"id":"chatcmpl-1","choices":[{"finish_reason":"stop","index":0,"message":{...}}],"created":1,"model":"gpt-4o-mini-2024-07-18","object":"chat.completion", ...}
```

With `tools` (`out_aigw2`): adds `llm.tools.0.tool.json_schema` (the full tool JSON) and the output as `llm.output_messages.0.message.tool_calls.0.tool_call.{id,function.name,function.arguments}`, usage 82/17/99. On an upstream 500 (`out_aigw3`): status ERROR, an `exception` event (`exception.type=InternalServerError`, `exception.message=Error code: 500 - {...}` with the upstream body), no usage, `llm.model_name` = the request model. The Anthropic Messages route (`POST /anthropic/v1/messages` to a backend with `schema: Anthropic` and `BackendSecurityPolicy type: AnthropicAPIKey`, `out_aigw5`): span `Message`, `llm.system = anthropic`, tokens 31/12/43 with `llm.token_count.prompt_details.cache_{read,creation}` 0, `llm.model_name = claude-3-5-sonnet-20241022`; the system prompt appears only in `llm.invocation_parameters`, not in `llm.input_messages`. The OpenAI-schema route to an `Anthropic` backend is refused (`422 unsupported API schema`).

### Mapping through spec 02 (real normalizer)

| Span | kind | provider | request / response model | tokens | cost | input | output |
|---|---|---|---|---|---|---|---|
| `ChatCompletion` (text) | llm (`openinference.span.kind`) | openai (`llm.system`) | `gpt-4o-mini-2024-07-18` (`llm.model_name`) / none | 31/12/43 | **computed** 1.185e-05 from the built-in table (aigw reports no cost) | converted | converted |
| `ChatCompletion` (tool) | llm | openai | same | 82/17/99 | computed 2.25e-05 | converted, tool definitions | `tool_call` part `{id:"call_1", name:"get_weather", arguments:{"city":"Paris"}}` |
| `ChatCompletion` (500) | llm, status error | openai | `gpt-4o-mini` / none | none | none | present | none |
| `Message` (Anthropic route) | llm | anthropic | `claude-3-5-sonnet-20241022` / none | 31/12/43 | none (no price row for that dated id) | present | present |

Exact keys: kind `openinference.span.kind`; provider `llm.system`; model `llm.model_name`; tokens `llm.token_count.{prompt,completion,total}` (+ `prompt_details.cache_*`); content `llm.input_messages.*`, `llm.output_messages.*`, `input.value`, `output.value`, `llm.tools.N.tool.json_schema`. No cost keys.

### Normalizer gaps for Envoy AI Gateway

None blocks a page. Notes:

1. `request_model` is `llm.model_name`, which aigw sets to the **response** model when there is one (dated, e.g. `gpt-4o-mini-2024-07-18`) and to the request model on errors; `response_model` stays empty and the real request model is only inside `llm.invocation_parameters`. Pricing still matches (the dated id has a row). Optional rule E1.
2. `llm.system` is the API family of the endpoint (`openai` for `/v1/chat/completions`, `anthropic` for `/anthropic/v1/messages`); **whether it is the backend's provider when the backend is Bedrock, Azure or Vertex was not run** (those need real cloud endpoints). If it is the ingress schema, a gateway fronting Bedrock reports `openai`.
3. No user, session or cost keys (aigw can add request headers as attributes through `OTEL_AIGW_*_REQUEST_HEADER_ATTRIBUTES`-style settings, not run).
4. Scope `envoyproxy/ai-gateway` has no version; the span is INTERNAL.

### Capture approach (Envoy)

A Python scenario program: `tempfile.mkdtemp()` under `/tmp` (short path) for `AIGW_*`; copy or symlink the pinned Envoy to `$AIGW_DATA_HOME/envoy-versions/1.39.1/bin/envoy`; render the YAML with the listener port (any free port) and `CETRA_CAPTURE_MOCK_URL`'s port; `subprocess.Popen([aigw, "run", cfg, "--admin-port", p_admin], env={HOME, AIGW_*, ENVOY_VERSION: "1.39.1", OPENAI_API_KEY: "sk-capture", OTEL_*: as the runner sets, OTEL_METRICS_EXPORTER: "none", OTEL_LOGS_EXPORTER: "none"})`; poll `GET /v1/models` on the listener until 200 (2 s); one chat request; `SIGINT` and `wait()`; exit. One `openai.chat` turn (and a second for the tool case). No `volatile_attributes` needed (none vary); no `timing_attributes`. Blockers: the fixed ports (cases are sequential, so only a foreign process holding 1061, 8081, 9856, 18000, 18002, 19000, 19001 or 19003 breaks it), the two binaries (about 400 MB on disk), and the Envoy pre-placement.

## 4. Temporal dev server 1.10.1 + OpenAI Agents plugin (run)

### Setup that works

`WorkflowEnvironment.start_local(dev_server_existing_path=<temporal>, dev_server_extra_args=["--headless", "--log-level", "warn"], ui=False)` uses the pre-fetched binary (without the path the SDK downloads one at runtime). The server printed `Temporal Server: localhost:<port>`, `Persistence: in-memory`, `Metrics: http://localhost:<port>/metrics`; up in **0.12 s**; the workflow ran in 0.6 to 0.7 s. Free ports chosen by the SDK. **No DNS lookup, no non-loopback connection**; no telemetry flag needed (the CLI made none). The plugin is a Temporal client plugin:

```python
tp = create_tracer_provider(resource=Resource.create({"service.name": "capture-app"}))   # temporalio.contrib.opentelemetry
tp.add_span_processor(BatchSpanProcessor(OTLPSpanExporter(endpoint=<.../v1/traces>, headers={...}, compression=None)))
trace.set_tracer_provider(tp)
plugin = OpenAIAgentsPlugin(model_provider=OpenAIProvider(openai_client=AsyncOpenAI(base_url=<mock>/v1, api_key=..., max_retries=0), use_responses=True),
                            model_params=ModelActivityParameters(start_to_close_timeout=timedelta(seconds=30)),
                            use_otel_instrumentation=True)          # add_temporal_spans=True is the default
client = await Client.connect(env.client.service_client.config.target_host, plugins=[plugin])   # + OpenTelemetryPlugin(add_temporal_spans=True) for StartWorkflow-style spans
async with Worker(client, task_queue="tq", workflows=[WeatherWorkflow], activities=[get_weather]):
    await client.execute_workflow(WeatherWorkflow.run, "What is the weather in Paris?", id="wf-capture", task_queue="tq")
```

The workflow must live in its own module (`workflows.py`): defined in `__main__` the worker fails `Failed validating workflow` (the sandbox re-imports the module). It builds `Agent(name="weather", model="gpt-4o-mini-2024-07-18", tools=[activity_as_tool(get_weather, start_to_close_timeout=...)])` and runs `await Runner.run(agent, question)`. The model call goes through the `invoke_model_activity` activity (Responses API, two `POST /v1/responses`), the tool through the `get_weather` activity. **No `instrument=` and no `OpenAIAgentsInstrumentor`**: the plugin instruments OpenAI Agents itself with OpenInference (`use_otel_instrumentation=True`); adding the instrumentor on top would instrument twice. The tracer provider must be the replay-safe one and set globally **before** `cetra.init()`, which then adds its processor to it (D-F-4 branch 1, as M3 reasoned).

**`temporalio-openai-agents` version.** 1.1.0 needs `openai-agents>=0.22,<0.23` (conflicts with the capture pin 0.24.0). **2.0.0 was uploaded 2026-11-17T21:07:32Z**; its requirements are `openai-agents>=0.22,<0.25`, `openai>=3,<4`, `temporalio[opentelemetry,pydantic]>=1.36.0,<2`, `mcp>=1.24,<3`. On the run date by calendar (2026-11-24) it is 7 days old; by the clock (the run was at 2026-11-23T23:15Z) it is 6 days 2 hours old, and it reaches 168 hours at **2026-11-24T21:07Z**. It works with `openai-agents` 0.24.0 and `openinference-instrumentation-openai-agents` 2.7.0 (the capture pins): the run above used exactly those. **Capture-project lock:** copying `tools/capture/py/pyproject.toml` and `uv.lock`, adding `temporalio==1.36.1` and `temporalio-openai-agents[openinference]==2.0.0` and `exclude-newer-package = {temporalio-openai-agents = "2026-11-17T22:00:00Z"}` (the project's `exclude-newer = "7 days"` is rolling and keeps 2.0.0 out until then) gives `uv lock`: **Resolved 327 packages, Added nexus-rpc 1.5.0, temporalio 1.36.1, temporalio-openai-agents 2.0.0, types-protobuf 7.35.1.20260906**; no existing pin moves, `mcp` is already in the lock. After 2026-11-24T21:07Z the `exclude-newer-package` line is unneeded.

### Span dump (`out_temporal1`: the plugin only; `out_temporal2`: plus `OpenTelemetryPlugin(add_temporal_spans=True)`)

Resource: `service.name=capture-app`, `telemetry.sdk.*`, `service.instance.id=<uuid>`. 14 spans from the plugin alone, all in scope `openinference.instrumentation.openai_agents@2.7.0`, one trace, `status OK`; the second run adds 8 Temporal spans in scope `temporalio.contrib.opentelemetry._otel_interceptor` (22 in total). In start order (kinds are the OTel span kinds; `OI` = `openinference.span.kind`):

```
[otel plugin only: StartWorkflow:WeatherWorkflow  CLIENT, attrs temporalWorkflowID]
Agent workflow            INTERNAL OI=AGENT   root (or child of RunWorkflow:WeatherWorkflow SERVER with the otel plugin)
  Agent workflow          INTERNAL OI=CHAIN
    weather               INTERNAL OI=AGENT   agent.name=weather graph.node.id=weather
      turn                INTERNAL OI=CHAIN        (x2: one per model turn)
        temporal:startActivity / temporal:executeActivity   INTERNAL OI=CHAIN   (the model activity; x2, and one pair for get_weather)
          response        INTERNAL OI=LLM     the model call, in the invoke_model_activity
          get_weather     INTERNAL OI=TOOL    input.value {"city":"Paris"}, output.value {...}, tool.name, tool.description, tool.parameters
[otel plugin only, per activity: StartActivity:<name> CLIENT and RunActivity:<name> SERVER with temporalActivityID, temporalRunID, temporalWorkflowID]
LLM span #1 `response`: llm.model_name=gpt-4o-mini-2024-07-18, llm.system=openai, llm.token_count.{prompt 82, completion 17, total 99, prompt_details.cache_read 0, completion_details.reasoning 0},
   llm.input_messages.1 (user), llm.output_messages.0.message.tool_calls.0.tool_call.{id call_1, function.name get_weather, function.arguments {"city":"Paris"}},
   llm.finish_reason=stop (also on the tool turn), llm.invocation_parameters {"id":"resp_1","created_at":1.0,"model":...}, input.value/output.value (the Responses JSON)
LLM span #2: tokens 131/12/143, input messages user + assistant tool call + tool result, output text
```

### Mapping through spec 02 (real normalizer)

| Span | kind | provider | model | tokens | cost | in/out |
|---|---|---|---|---|---|---|
| `response` x2 | **llm** | openai | gpt-4o-mini-2024-07-18 | 82/17 and 131/12 | computed 2.25e-05 and 2.685e-05 | yes; turn 1 output is the tool call part, turn 2 text |
| `get_weather` | tool | | | | | input `{"city":"Paris"}`, output |
| `weather` | agent | | | | | |
| `Agent workflow` | agent (root), chain (inner) | | | | | |
| `turn`, `temporal:startActivity`, `temporal:executeActivity` | chain | | | | | |
| `StartWorkflow:*`, `RunWorkflow:*`, `StartActivity:*`, `RunActivity:*` (otel plugin) | **span** (no convention) | | | | | `temporalWorkflowID`, `temporalRunID` (a uuid), `temporalActivityID` stay in `attributes` |

This is the same output as the `openai-agents-tool` capture plus Temporal's chains: **no normalizer gap**; the design's note ("Temporal's own spans stay `span`") holds, and the plugin's `temporal:*` activity spans are `chain` through OpenInference. Totals are not double counted (one `llm` span per model call).

### Capture approach (Temporal)

A Python scenario program in the existing Python capture project: `workflows.py` beside `main.py`; `subprocess` is not needed: `WorkflowEnvironment.start_local(dev_server_existing_path=<CETRA_CAPTURE_TOOLS>/temporal/1.10.1/temporal, ...)`, the tracer provider from `create_tracer_provider` with `BatchSpanProcessor(OTLPSpanExporter())` reading the runner's `OTEL_EXPORTER_OTLP_*` environment, a fixed workflow id (`id="wf-capture"`), then `tp.force_flush(); tp.shutdown(); await env.shutdown()`. Two `openai.responses` turns (tool call, text) exactly as `openai-agents-tool`. `volatile_attributes: ["temporalRunID"]`; `span_names` can drop nothing (22 spans are fine) or keep the plugin-only spans if the page documents the plugin alone (14 spans). The scenario uses the existing harness for the mock URL; it cannot use `setup()` (it builds a plain `TracerProvider`), see the capture-tool changes.

## Proposed normalizer rules

Everything traces to a run above. IDs are local to this spike (the plan assigns D-IN numbers).

- **B1 (Bifrost root span, run-backed).** Spec 02 § Span kind, scope-gated like `mirascope` and `agent_framework`: on a span whose instrumentation scope name is `bifrost`, with no `gen_ai.operation.name`, the kind is `chain` (the root `/v1/chat/completions` repeats its `chat` child's model and content). A `chat` span has `gen_ai.operation.name=chat` and stays `llm`. The 24 `plugin.*` spans have no model: `span`. Test: the captured trace gives one `llm` span, one `chain`, 24 `span`.
- **B2 (flattened tool calls, run-backed on output).** In the OpenAI-chat message converter (spec 02 § Input / output, GenAI row): a `tool_calls` element with `name` and `args` and no `function` object (Bifrost) becomes a `tool_call` part `{id, name, arguments}`, with `args` parsed as JSON as the existing path does for `function.arguments`; `content: ""` beside tool calls adds no empty text part. Fixture: the captured tool-turn `chat` span; and the history (assistant tool call, then a `tool` message) on a second turn once captured.
- **B3 (Bifrost cost split, run-backed).** Spec 02 § Cost step 1: `bifrost.cost.input` joins `llm.cost.prompt` and `gen_ai.usage.input_cost` as a `cost_input` key, and `bifrost.cost.output` joins `llm.cost.completion` and `gen_ai.usage.output_cost`. `gen_ai.usage.cost` is already read. (`bifrost.cost.input.text` and `.output.text` are modality sub-costs: not read.)
- **E1 (optional, Envoy request model).** When the OpenInference span's scope name is `envoyproxy/ai-gateway` and `llm.invocation_parameters` has a `model` string, it is the `request_model`, and `llm.model_name` is the `response_model` (today `llm.model_name` fills `request_model` and nothing fills `response_model`). Skip it if the plan accepts the page note below.
- **No rule** for Temporal (no gap), for the 24 Bifrost plugin spans, for Envoy's provider (`llm.system` works for the OpenAI-schema and Anthropic routes run), or for `gen_ai.request.user` (optional user id).
- **Gateway host list (`ENDPOINT_PROVIDERS`, D-IN-13).** Unchanged: Bifrost and aigw run on the user's own host and name the upstream provider themselves (`gen_ai.provider.name`, `llm.system`).
- **Spec and decisions.** The rules above add rows to spec 02 § Span kind and § Cost, and a D-IN-n decision for "scope-gated kind for Bifrost"; the first spike's D-IN-16 (LiteLLM keys) is a separate item.

## Page notes (what the M5 pages must say)

- **Bifrost**: required config (the otel plugin block; `trace_type: genai_extension`; `protocol: http` and a full `/v1/traces` URL; `headers.Authorization`), plus the three keys for an air-gapped host (`framework.pricing.pricing_url` and `model_parameters_url` as `file://`, `mcp_library_sync_interval: 0`) and `client.enforce_auth_on_inference` (default true, needs a virtual key). The gateway **exits at start when it cannot fetch its pricing datasheet and has none stored**. Each call is two `llm` spans until B1 ships; cost comes from Bifrost (its datasheet), reported.
- **Envoy AI Gateway (aigw)**: standard `OTEL_EXPORTER_OTLP_*` variables; HTTP protobuf works; `SIGINT` flushes; default batch delay 2.4 s (`OTEL_BSP_SCHEDULE_DELAY`); first start downloads Envoy from Tetrate's `archive.tetratelabs.io` (func-e); the model column shows the response model; no cost reported (cetra prices it from the table).
- **Both** honour `traceparent`: with the app's own OpenAI or framework instrumentation on, the gateway's LLM span is a child of the app's, both carry tokens and cost, so the same call is counted twice. Recommend instrumenting one side (the gateway, for several apps and languages) or filtering one.
- **Temporal**: install and setup as above; the dev server needs the CLI binary (not downloaded for CI: the SDK's runtime download is avoided with `dev_server_existing_path`); the order `create_tracer_provider`, `set_tracer_provider`, then `cetra.init()`; `temporalio-openai-agents` 2.0.0 is the version for `openai-agents` 0.24.x (1.1.0 needs 0.22.x).
- **Kong**: setup from the vendor docs only, labelled not captured, or dropped.

## Capture-tool changes (what blocks a binary scenario, and the minimum)

Blocking, in `tools/capture`:

1. **The mock records an error for any unscripted request** (`mock.rs`: `no route <METHOD> <path>` goes to `errors`, and `run.rs` fails the capture with `mock_log.errors`). Bifrost sends **`GET /v1/models` twice at start**. Add `GET .../models` (any prefix) as a no-turn route answering `{"object":"list","data":[]}`, as `HEAD /api/hello` is today (`mock.rs` `router`: one more `.route`/match, plus a unit test). Without it no Bifrost capture passes. aigw and Temporal send no extra request.
2. **`harness.finish()` records versions with `importlib.metadata.version(name)` for every `libraries` entry, and `case.json` needs `instrumentation` in `libraries`** (`scenario.rs`, `run.rs` `source_from`). A binary is not a Python package. Add `capture_py.harness.finish_tool(provider_or_none, versions: dict[str, str])` (writes `{"libraries": {...}}` from the given strings; `provider=None` skips the flush), and let a gateway scenario list `libraries: ["bifrost"]` (or `["aigw", "envoy"]`) with `instrumentation: "bifrost"`. No Rust change: `read_libraries` reads any string versions from the meta file. Temporal keeps the importlib path (`temporalio`, `temporalio-openai-agents`, `openinference-instrumentation-openai-agents`) and needs only the Temporal CLI version as an extra string.
3. **Where the binaries live.** The runner copies only `PATH`, `HOME`, `UV_*` and `XDG_*` into the program's environment (`program_env`), and `scenario.env` is "instrumentation switches only". Add `CETRA_CAPTURE_TOOLS=<repo>/target/capture-tools` to `program_env`, and a `just capture-tools` recipe (a fetch script like the other pinned tools: a table of URL, size and SHA-256 per tool, verified, extracted into `target/capture-tools/<name>/<version>/`; Envoy placed in `aigw`'s data layout by the scenario) that `capture-py-sync` or `check-capture` depends on, plus an `AGENTS.md` § Required tools line (and the 7-day rule: pins at 2.5.1, 1.2.1, 1.39.1, 1.10.1). About 400 MB for Bifrost, aigw and Envoy, 126 MB for Temporal; `target/` is git-ignored.
4. **Orphaned subprocesses.** The runner spawns the program with `kill_on_drop(true)` and kills only that child on timeout (`wait_program`); a scenario that launches a gateway under `uv run python main.py` leaves the gateway running if the runner kills `uv`. Spawn the program in its own process group (`Command::process_group(0)`) and kill the group on timeout and on drop; the scenario also stops the gateway in `try/finally`.
5. **Python capture project.** Add `temporalio==1.36.1` and `temporalio-openai-agents[openinference]==2.0.0` to `tools/capture/py/pyproject.toml` with the temporary `exclude-newer-package` line (or wait until 2026-11-24T21:07Z), and regenerate `uv.lock` (verified: 4 packages added, nothing else changes). Bifrost and aigw need no Python package (the harness uses `openai`, already pinned).
6. **Scenario fields already cover the rest**: `volatile_attributes` (Bifrost's uuids, Temporal's run id), `timing_attributes` (Bifrost's `*_ms`), `span_names` (drop the 24 plugin spans), `turns` (`openai.chat`, `openai.responses`). `scrub.rs` already handles `service.instance.id`. The receiver accepts any path and an identity body (all three send identity bodies); the runner's `OTEL_EXPORTER_OTLP_COMPRESSION=none` is only needed for aigw/Temporal and is already set.

Not blocking, but a decision for the plan:

- **Ports.** Bifrost takes `-port`; aigw's listener and admin ports are free, **but it also binds fixed ports** (1061, 8081, 9856, 18000, 18002, 19000, 19001, 19003). `run --all` is sequential, so a clean machine works; a CI runner with something on one of those ports breaks only the aigw case. The scenario picks free ports with `bind(0)` for everything configurable.
- **Timeouts.** `PROGRAM_TIMEOUT` is 120 s and `IDLE_MAX` 5 s: start-up (1 to 2 s), export (0.15 s Bifrost, 2.4 s aigw unless stopped first) and stop (under 1 s) fit. The scenario must stop the gateway before exiting (both flush on `SIGINT`) so the exporter's batch is out before the runner's idle wait.
- **Env clearing.** The scenario passes the environment explicitly to the gateway process (`env=` of `subprocess.Popen`); for aigw it must include `HOME` (writable) and the `AIGW_*` directories under a short `/tmp/...` path.
- **Docker** is not needed for Bifrost, aigw or Temporal; only Kong would add it.

## Summary

- Bifrost v2.5.1, `aigw` v1.2.1 (+ Envoy 1.39.1) and the Temporal dev server 1.10.1 all ran **offline in a loopback-only namespace** against an in-process fake OpenAI server; all four sha256 match the vendors' own digests; spans were dumped and normalized with the real `cetra_otel::normalize`. Kong was skipped: no Docker binary on this machine.
- Bifrost writes the GenAI convention (scope `bifrost`): the `chat` CLIENT span has provider, model, tokens and a **reported** cost (`gen_ai.usage.cost`, plus `bifrost.cost.input|output`), but its SERVER root is also classified `llm` (a second LLM span per call), and tool calls are flattened (`name`/`args`) so cetra stores a malformed part. Rules B1, B2, B3 fix these.
- Bifrost phones home at start (pricing datasheet from `getbifrost.ai`; it **exits** if it cannot get it) and probes `GET /v1/models` twice; three config keys (`file://` pricing, `mcp_library_sync_interval: 0`, `client.enforce_auth_on_inference: false`) make it run with zero DNS lookups in 1.0 s. It flushes on `SIGINT`.
- `aigw` emits one OpenInference `ChatCompletion` LLM span per request (also `Message` on the Anthropic route): kind, provider (`llm.system`), tokens, content, tool calls all map with no gap; cost is computed (aigw reports none). `llm.model_name` is the response model. It needs a pre-placed Envoy binary or it downloads one from Tetrate's archive (flagged, not in the approved list), a short `AIGW_RUNTIME_DIR`, and it binds about eight fixed ports.
- Temporal: `temporalio-openai-agents` **2.0.0** works with `openai-agents` 0.24.0 and OpenInference 2.7.0 (uploaded 2026-11-17T21:07Z: 7 days by date, 168 h at 2026-11-24T21:07Z); the capture project's lock resolves with 4 added packages and no pin moved. The spans are the known OpenInference agent/LLM/tool spans plus `temporal:*` chains; Temporal's own `StartWorkflow`/`RunActivity` spans stay `span`. No normalizer gap.
- Both gateways honour `traceparent`, so an instrumented client plus a gateway counts one call twice; the pages must say so.
- Capture-tool changes: answer `GET /v1/models` in the mock (blocks Bifrost), a `finish_tool` harness function for non-pip versions, `CETRA_CAPTURE_TOOLS` plus a checksummed fetch recipe, a process group for the program, and the two Temporal packages in the Python project.
- Doc: `docs/plans/in-integrations/spikes/m5-binaries.md` (uncommitted).
