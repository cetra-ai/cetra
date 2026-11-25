# M8 follow-up spike: Spring AI instrumentor, Spring Boot route, openai-go v3 with OpenInference Go

Status: spike, not a plan. Nothing here changes the design or the decisions. Open questions are in § 7 and need the user or the plan. Clock: 2026-11-24, about 12:30 UTC. The 7-day cutoff is **2026-11-17 12:30 UTC** (the install clock was 2026-11-24 12:30 UTC); a release published after it is not eligible.

Evidence root (F): `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m8/followup`. Paths below are relative to F unless absolute. The Java runtime, Maven and the Maven cache are reused from the Java spike (J, read-only: the JDK and Maven are symlinked or read; the cache was hard-linked into `F/cache/m2`, and new downloads went to F only). Each fact is tagged:

- **run**: observed by a command in this session, with the evidence path under F.
- **docs**: read from a published source (a jar's class files or sources, a pom, a README, a Phoenix page), with the path or URL.

## 1. Scope and result (Q1, Q2, Q3)

Questions, in order (from the brief):

1. **OpenInference Spring AI instrumentor (Q1, done, § 5.1).**
2. **Spring Boot starter route (Q2, § 5.2).**
3. **openai-go v3 with OpenInference Go (Q3, § 5.3).**

Q1 result (facts):

- The instrumentor `com.arize:openinference-instrumentation-springAI:0.1.10` (newest eligible, published 2026-10-01) **runs with Spring AI 2.0.2** in plain Java (no Boot): one chat call and a `ChatClient` tool round, no exception, the tool round executed, the answer correct (run `oi-both-1`, `oi-oi-1`, `oi-logs`). The Phoenix page's snippet itself does **not** compile against 2.0.2 (`OpenAiApi` is absent from 2.0.2; `OpenAiChatModel.Builder` has `openAiClient` and `options`, not `openAiApi` and `defaultOptions`; javap, run).
- Its spans are named `generate`, kind CLIENT, with `openinference.span.kind=LLM`, `llm.model_name`, `llm.provider=openai`, `llm.system=spring-ai`, messages and `input.value`/`output.value`, tokens (`llm.token_count.*`) and `llm.finish_reason`. One span per model call. No tool span and no agent span.
- **Content is on by default** (`TraceConfig.getDefault()` hides nothing; the library reads no environment variable for it). Content lands on the span as attributes, not events (run `oi-oi-1`, 0 events). `TraceConfig.builder().hideInputs(true).hideOutputs(true)` removes it (run `oi-hidden`), but only in code; this release reads no `OPENINFERENCE_HIDE_*` variable (docs: `dl/src-oi-core/com/arize/instrumentation/TraceConfig.java`, `OITracer.java`; grep of the library sources for `getenv` returns nothing).
- **Duplicates.** Spring AI's own Micrometer observations, exported by the Micrometer OTel bridge, produce the `chat <model>` span (gen_ai.*, tokens, no content) and the tool-round framework spans. With both handlers on one registry, every model call yields **two llm spans** (`chat gpt-4o-mini` and `generate`) with the same token counts (run `oi-both-1`: 9 spans; `both-hidden`: 9 spans). Only one set comes out if one handler is left off:
  - OI handler only (`oi-oi-1`): 2 `generate` spans, no framework and no tool spans, **each a root span in its own trace** (no parent; the instrumentor's `onStart` takes `Context.current()`, and nothing makes the Micrometer observation current without the Micrometer tracing handler).
  - Micrometer handler only (`oi-spring-1`): the 7 Spring AI spans (the same shape as the Java spike's `spring-ai-3`), no OI content.
  - Parenting with an app span: when the app makes its own OTel span current, the OI `generate` spans become children of it (`oi-app-1`: `app.request` → 2 × `generate`, one trace).
- **What cetra's normalizer makes of the OI-only spans** (run, `oi-oi-1/out/h.log`): both rows are `llm`, provider `openai`, request model `gpt-4o-mini`, tokens 82/17 and 131/12, cost computed, input preview read, output preview read on the second call only (the first call's assistant message has empty content and carries tool calls, which the preview does not read). `llm.*` keys stay in `attributes_left` (`llm.model_name`, `llm.token_count.*`, `llm.finish_reason`, `llm.provider`, `llm.system`), so the normalizer reads the OI tokens and model through its OpenInference rules (conventions `openinference`), not through `llm.*` keys directly.
- **Normalizer, both handlers** (run `oi-both-1`): 4 llm rows for 2 model calls (2 `chat gpt-4o-mini` with `gen_ai.*`, 2 `generate` with `llm.*`). Token sums double (82+82, 131+131 in the two sets). The normalizer does not dedupe. The framework spans (`spring_ai chat_client`, `tool _calling`, `call` ×2) survive as generic `span` rows with `operation=framework`; `execute_tool get_weather` is a tool row with no tool name.

Q2 result (facts):

- Boot 4.1.2 + Spring AI 2.0.2 (OpenAI starter) + Boot's OpenTelemetry starter: one chat call with a tool round works, and the JVM exits 0 by itself with `WebApplicationType.NONE` (run `boot-plain-hold`). The brief's OTLP keys (`management.otlp.tracing.*`) export no traces with this starter; `management.opentelemetry.tracing.export.otlp.*` do (run `boot-plain-b`). Sampling defaults to 0.1; metrics default to `localhost:4318`.
- Spans: the same Spring AI set as plain Java plus two HTTP client `POST` spans (CLIENT, Boot's noise). With the OpenInference handler registered as a bean, Boot invokes it (2 `generate` spans, run `boot-oi-b`), so the Boot wiring is a bean, not a registry call.
- Trap: without the Spring AI BOM, Maven resolves `spring-ai-model` to 1.0.1 (from the OpenInference pom), so the Boot project must import the BOM.

Q3 result (facts):

- openai-go v3.100.0 (newest eligible v3) + OpenInference Go openai-go middleware v0.1.2 compile and run (chat and a tool round, exit 0). The v3 client accepts the middleware unchanged.
- Spans: 2 `openai.chat.completions.create` per program run (one per call), INTERNAL, roots unless the app has a current span. Content on by default as attributes; `OPENINFERENCE_HIDE_*` env vars hide it. One llm span per call, no duplicates, no tool span.
- The binary links openai-go v1 as well (the instrumentor imports v1's `option`).
- Normalizer: llm rows with provider, model and tokens; I/O read where the output is text.

Corrections to the Java spike: its "kind unset" for Spring AI spans was wrong (INTERNAL, § 8).

## 2. Versions (the 7-day rule)

Rule: the newest release published at least 7 days before install (cutoff 2026-11-17 12:30 UTC).

| Item | Version pinned | Asset or package (URL) | Published (UTC) | sha256 | Notes |
|---|---|---|---|---|---|
| OpenInference Spring AI instrumentor | `0.1.10` | `https://repo1.maven.org/maven2/com/arize/openinference-instrumentation-springAI/0.1.10/openinference-instrumentation-springAI-0.1.10.jar` (9.5 KB), `.pom`, `-sources.jar` (4.4 KB) | 2026-10-01T18:26:19Z (Maven Last-Modified); `maven-metadata.xml` `latest` = 0.1.10 | jar `e9f0e7ecd64af5b552539b51ce03e200d6f7da69042678db934acf226770320f`; pom `893ddd5f35cc229a143944c7d2eb5388de94051cddf9fe3428cac1233fd326e6`; sources `29e8ff1dbc6bb3807759ac73d91c31cbbefe23988d27dfeec38db6c885189161` | Newest eligible. Older: 0.1.9 (2026-04-04, the Phoenix page's version), 0.1.8 (2026-03-23). Files in `F/dl/`. |
| OpenInference Java core (transitive) | `0.1.10` | `com/arize/openinference-instrumentation/0.1.10/…` (25,933 B) | 2026-09-15T04:58:12Z | jar `419c6e52553d0084eaba9e0242c8318adc8c9f7381e7cdaad8b13dd85629a31f`; sources `a618536683ed9cbb6c2142e76b477cd090b9f9f1c34a4183309c26d279ee9890` | Provides `OITracer`, `TraceConfig`. Resolved by Maven. |
| OpenInference semantic conventions (transitive) | `0.1.19` | `com/arize/openinference-semantic-conventions/0.1.19/…` (pom only read) | 2026-09-15T04:58:12Z | not downloaded (resolved by Maven, `F/cache/m2`) | |
| Spring AI BOM, `spring-ai-openai`, `spring-ai-client-chat`, `spring-ai-model` | `2.0.2` | `repo1.maven.org/maven2/org/springframework/ai/…/2.0.2/` | parent pom 2026-10-22T12:46:15Z | parent pom `23b1f4f7ccc7e6a168893b44e4c6b927dc629970de4099415e901e36d0ff05fe` (`F/dl/spring-ai-parent-2.0.2.pom`) | `2.0.3` and `2.1.0` (both 2026-11-20) are too new. The same jars as the Java spike (J/cache). |
| Micrometer tracing OTel bridge | `1.7.2` | `io.micrometer:micrometer-tracing-bridge-otel` | 2026-10-22T07:26:56Z | in J's cache | Same as the Java spike. |
| openai-java core and OkHttp client | `4.80.0` | `com.openai:openai-java-core`, `openai-java-client-okhttp` | 2026-10-10T00:51:41Z | in J's cache | As the Java spike. |
| OpenTelemetry BOM, alpha BOM, SDK, OTLP exporter, autoconfigure | `1.68.0`, `1.68.0-alpha` | `io.opentelemetry:opentelemetry-bom`, `-bom-alpha` | 2026-11-06T15:36:11Z | in J's cache | **Deviation**: the instrumentor's pom asks for `opentelemetry-api` 1.49.0 and `opentelemetry-instrumentation-api` 2.14.0; the BOM manages them up to 1.68.0 (`mvn dependency:tree`, `F/logs/oi-spring-tree.txt`). Same reason as the Java spike (alpha BOM for `api-incubator`). |
| Micrometer observation (transitive) | `1.15.1` | from the instrumentor's pom | | | Resolved by Maven nearest-wins (`F/logs/oi-spring-tree.txt`). Not pinned here. |
| Micrometer Brave bridge (transitive, not used) | `1.5.1` | `io.micrometer:micrometer-tracing-bridge-brave` (runtime scope in the instrumentor's pom) | | | Pulled by the instrumentor; not used. Left in place. |
| `spring-ai-model` 1.0.1 (runtime, in the instrumentor's pom) | not on the classpath | | | | Maven nearest-wins resolves `spring-ai-model` to **2.0.2** (from `spring-ai-openai`). `F/logs/oi-spring-tree.txt` has no `spring-ai-model:jar:1.0.1`. |
| SLF4J API | `2.0.18` (pinned by F's pom) | `org.slf4j:slf4j-api` | not checked here | in cache | Added after `oi-both-1`: `slf4j-api` 1.7.36 (from another dependency) with `slf4j-simple` 2.0.20 printed "Failed to load class StaticLoggerBinder" and silenced the instrumentor's logs. Runs `oi-logs`, `both-logs` use 2.0.18. |
| SLF4J simple | `2.0.20` | `org.slf4j:slf4j-simple` | 2026-09-22T14:50:55Z (J) | in cache | Spike only. |
| Temurin JDK | `21.0.13+9` | the Java spike's JDK, `J/bin/jdk/jdk-21.0.13+9` (read) | 2026-10-22T08:58:06Z | `d7f5d5cf865a2902b70ff549b97586a7d3f234e016f041ab447ea740c7d66d1c` (J) | Shared. |
| Apache Maven | `3.9.16` | `F/bin/maven` → `J/bin/apache-maven-3.9.16` (read) | 2026-05-13T21:39:02Z | `831a8591…` (J) | Shared. |

Ineligible for this spike (for the record): OpenInference Spring AI has no newer release than 0.1.10 (`maven-metadata.xml`, lastUpdated 2026-10-01).

### 2.1 Versions added for Q2 and Q3

| Item | Version pinned | Asset or package (URL) | Published (UTC) | sha256 / h1 | Notes |
|---|---|---|---|---|---|
| Spring Boot (parent, dependencies, BOM line) | `4.1.2` | `https://repo1.maven.org/maven2/org/springframework/boot/spring-boot-starter-parent/4.1.2/spring-boot-starter-parent-4.1.2.pom` (14,470 B); `https://repo1.maven.org/maven2/org/springframework/boot/spring-boot-dependencies/4.1.2/spring-boot-dependencies-4.1.2.pom` (141,480 B; `spring-framework.version` 7.0.10) | 2026-10-22T12:03:58Z (parent); 2026-10-22T12:03:45Z (dependencies pom) | parent `b7310a4672ba54ba552cc74e6f62137890f361c93be04aa56def10a27552f9f5` | Newest eligible 4.x. `4.0.9` has the same date (2026-10-22T12:03:45Z); `4.0.10`, `4.1.3`, `4.2.0` are 2026-11-19 (too new). Resolved Spring Framework: `spring-core` 7.0.10. See § 7 item 2. |
| Spring Boot jars | `4.1.2` | `spring-boot` 1,392,971 B `1fbf6443a301eb9a098bd024149c0edc6578b073f75da8c98bc1af84c43a8f59`; `spring-boot-opentelemetry` 72,850 B `13519c1b260d9d9cca48c43f8425f5dc35ae751577cecea2ef1b625307866d5f`; `spring-boot-micrometer-tracing-opentelemetry` 71,679 B `f1d0467a1e5ba45c377b9863c44b5271233b29c6fbcc532e8b1c1b715fa490e6`; `spring-boot-actuator-autoconfigure` 140,959 B `97faaf81fe3fb54e224592afdd8af154fd65cc62b303334db35b076b63e03e1e` | 2026-10-22T12:03:49Z (`spring-boot-opentelemetry` jar) | the four sha256 in the Notes | Jars in `F/cache/m2` (Maven verified sha1 by default); checksums taken from the cache copies. |
| Boot OpenTelemetry starter | `4.1.2` | `spring-boot-starter-opentelemetry` pom `F/dl/starter-otel-4.1.2.pom` | 2026-10-22T12:03:58Z | `4469abe6d4706cee75b1a3e956c9fded876a4a9c8cf3515016b0bd1a6c962db7` | Pulls `micrometer-tracing-bridge-otel` 1.7.2, `opentelemetry-exporter-otlp` 1.62.0 (Boot-managed), `micrometer-registry-otlp`. |
| Spring AI starter, OpenAI | `2.0.2` | `spring-ai-starter-model-openai` pom `F/dl/starter-openai-2.0.2.pom` (4,226 B); jars `spring-ai-starter-model-openai-2.0.2.jar` `b1bfa7cc…`, `spring-ai-autoconfigure-model-openai-2.0.2.jar` `a76233d4…`, `spring-ai-openai-2.0.2.jar` `4310280c…` | pom 2026-10-22T12:46:16Z | pom `70307e420167e902879fe66e9c56cd90e0ec4e7290296abbc70dc19f889612f1` | `spring-ai-bom` 2.0.2 imported in the Boot project (pom Last-Modified 2026-10-22T12:46:15Z, `sai-bom-2.0.2.pom` `c61c54a2…`). |
| Spring AI `spring-ai-model` (resolved) | `2.0.2` (after the BOM import) | from the BOM | | | Without the BOM it resolves to `1.0.1` (§ 5.2). |
| OpenInference Spring AI instrumentor | `0.1.10` | as § 2 | | | Used by the Boot project too. |
| Go toolchain | `go1.27.4` (linux/amd64) | `https://go.dev/dl/go1.27.4.linux-amd64.tar.gz`, 70,680,823 B, extracted to `F/go/root/go` | 2026-11-02T20:36:20Z (`https://proxy.golang.org/golang.org/toolchain/@v/v0.0.1-go1.27.4.linux-amd64.info`) | `7226736c2339719cda28fbf3a54d71aa63bcefe376e98cef00034268dab24840` (equal to the `sha256` in `https://go.dev/dl/?mode=json`, `F/meta/go-dl.json`) | Newest stable in go.dev's JSON (`go1.26.11` is the other stable line). `GOTOOLCHAIN=local`. `go version` → `go1.27.4 linux/amd64`. |
| openai-go v3 | `v3.100.0` | `github.com/openai/openai-go/v3` via `https://proxy.golang.org/github.com/openai/openai-go/v3/@v/v3.100.0.{info,mod,zip}` | 2026-11-16T22:37:15Z (proxy `.info`) | go.sum h1 `A1Ugtrbvz0uBQC23UpQXjilhJjdTHG59O36Uif9R2aU=` | Newest eligible v3. `v3.101.0` (2026-11-17T18:14:01Z) and later are after the cutoff (`F/meta/goinfo.txt`). The brief's floor was v3.73.0 (Phoenix's page). |
| OpenInference Go openai-go middleware | `v0.1.2` | `github.com/Arize-ai/openinference/go/openinference-instrumentation-openai-go` (proxy `.info`, `.mod`, `.zip`) | 2026-11-04T20:36:09Z | go.sum h1 `p6oqjrOsdY1GUC5PqyOeYbyeskkFBp/+GjTjYZgsI+s=` | Newest. Its `go.mod` (v0.1.2 and v0.1.1 alike) requires `github.com/openai/openai-go v1.12.0`: the v1 module is in the build graph and its `option` package is compiled in (§ 5.3). |
| OpenInference Go core | `v0.1.2` | `github.com/Arize-ai/openinference/go/openinference-instrumentation` | 2026-09-15T04:50:23Z | h1 `lz44nsgF9t6YE91Tdnb3YymMTfxIyN/K/rG9C+OmyQA=` | Newest. |
| OpenInference Go semantic conventions | `v0.1.11` (required by v0.1.2; not overridden) | `github.com/Arize-ai/openinference/go/openinference-semantic-conventions` | `v0.1.11` 2026-10-01T18:20:15Z (proxy list, `.info` not read) | h1 `Hs+pPtRSC5HzjacobGN48c4skrcyxq8Vqis//OBHgAM=` | Newest eligible is `v0.1.15` (2026-11-16T23:45:08Z), not required by anything here, so not used. |
| OpenTelemetry Go | `v1.47.0` (otel, sdk, trace, otlptrace, otlptracehttp) | `go.opentelemetry.io/otel/...` | 2026-10-02T08:10:46Z (newest eligible; `v1.48` not listed) | otel h1 `j7ALJ/zgkS7Z6aeJW09p8VC9804bC+PpeTfCD4XPnOM=`; sdk `zWXEr4j2lFefG87TU6Yg8a7ngfohIKFZHKp0Hf5hC6I=`; otlptracehttp `aXZXsZ012wOgVkqaiQv+Z/d8V0xKjmg70F6m4CE94lc=` | The instrumentor needs v1.43.0; MVS raised the graph to v1.47.0 because the program asks for it. |

Ineligible for Q3 (for the record): OpenInference Go `v0.1.2` is the newest and eligible; Spring Boot `4.2.0` etc. above.

## 3. Method

- Build: `F/bin/maven/bin/mvn -B -q -s F/settings.xml -Dmaven.repo.local=F/cache/m2 compile` in `F/projects/oi-spring` (outside the namespace). `dependency:build-classpath` writes `F/projects/oi-spring/cp.txt`.
- Runner: `F/runners/oi-spring.sh <mode>`, run as `m7/tools/netrun.sh <run dir> 180 -- bash F/runners/oi-spring.sh <mode>` (loopback only). The run's `env` file (`FAKE_TOOL=get_weather`, plus the switch under test) is sourced by `inner.sh`. Fake key `sk-fake-m8`.
- Normalization: `m7/tools/h.py` with the m7 venv, using cetra's real normalizer (`m7/norm-target/release/norm`, not rebuilt). Output: `runs/<run>/out/<run>.dump.txt` (raw spans) and `out/h.log` (normalized rows).
- Program: `F/projects/oi-spring/src/main/java/m8/OiSpringRun.java`. Mode argument: `oi` (instrumentor only, the Phoenix wiring), `both` (Micrometer OTel handler and the instrumentor on one registry), `spring` (Micrometer OTel handler only). Env switches: `M8_HIDE` (TraceConfig with hideInputs and hideOutputs), `M8_APP_SPAN` (the app opens and makes current its own span). The OTel SDK is `AutoConfiguredOpenTelemetrySdk` with the `OTEL_EXPORTER_OTLP_*` env route, as in the Java spike.
- The "real home" guard in `netrun.sh` reports the calling shell's working directory (the worktree, opened not read) in every run; no file under the real home was read or written (`runs/*/real-home.txt`).

## 4. Run index (Q1)

| Run (`F/runs/`) | What it tests | Spans | Result |
|---|---|---|---|
| `oi-both-1` | both handlers, tool round | 9 | 2 `chat` (Spring) and 2 `generate` (OI) llm spans; framework spans; one tool span. Answer correct. (SLF4J NOP, before the slf4j pin.) |
| `oi-oi-1` | OI handler only | 2 | 2 `generate` spans, both roots, separate traces; content on |
| `oi-spring-1` | Micrometer OTel handler only (kind 1 = INTERNAL on all 7 spans) | 7 | the Java spike's shape; no content |
| `oi-app-1` | OI only, app span made current (`M8_APP_SPAN=1`) | 3 | 2 `generate` spans with the app span as parent (one trace) |
| `oi-hidden` | OI only, `M8_HIDE=1` | 2 | no `input.value`, `output.value` or message content attributes |
| `both-hidden` | both, `M8_HIDE=1` | 9 | OI `generate` without content; Spring's own spans unchanged (they never had content) |
| `oi-logs` | OI only, slf4j-api 2.0.18 | 2 | no instrumentor warnings in the log; exit 0 |
| `both-logs` | both, slf4j-api 2.0.18 | 9 | as `oi-both-1`; exit 0 |

Evidence per run: `runs/<run>/run.log` (where captured), `runs/<run>/recv/*.bin` (OTLP protobuf bodies), `runs/<run>/http.jsonl`, `runs/<run>/out/<run>.dump.txt`, `runs/<run>/out/h.log` (normalizer output, from the `NORMALIZED` section).

### 4.1 Run index for Q2 and Q3

| Run (`F/runs/`) | What it tests | Spans | Result |
|---|---|---|---|
| `boot-plain-1` | Boot, route A keys (brief's `management.otlp.tracing.*`) | 0 | no trace post; metrics WARN to `localhost:4318` |
| `boot-oi-1` | route A, OI bean | 0 | same |
| `boot-plain-b` | route B (`management.opentelemetry.tracing.export.otlp.*`), no OI | 9 | OK, exit 0 with `SpringApplication.exit` |
| `boot-oi-b` | route B, OI bean (`-Dm8.oi=true`) | 11 | 2 `generate` spans under the Spring spans; exit 0 |
| `boot-plain-hold` | route B, no explicit exit (`-Dm8.hold`) | 9 | the JVM ends by itself (7 s, exit 0) |
| `go-1` | openai-go v3 + OI Go, chat and tool round | 2 | 2 `openai.chat.completions.create`, roots |
| `go-hidden` | as `go-1` with `OPENINFERENCE_HIDE_INPUTS/OUTPUTS=true` | 2 | no I/O attributes |
| `go-app2` | as `go-1` with an app span current (`M8_APP_SPAN=1`) | 3 | the 2 OI spans are children of `app.request` (`go-app` lost the app span: the program ended it after the flush; fixed in `go-app2`) |

## 5. Findings

### 5.1 OpenInference Spring AI 0.1.10 on Spring AI 2.0.2 (plain Java)

**Compatibility (run and docs).**

- The instrumentor is `com.arize.instrumentation.springAI.SpringAIInstrumentor implements ObservationHandler<Observation.Context>` (`dl/src-oi-spring/com/arize/instrumentation/springAI/SpringAIInstrumentor.java`). It casts to `ChatModelObservationContext` and calls `getRequest().getInstructions()`, `getResponse().getResults()`, `getResponse().getMetadata().getUsage()`, `getResult().getMetadata().getFinishReason()`, and the `ToolCall`, `ToolResponse` and `Message` accessors. javap on the 2.0.2 jars shows these exist (`ChatModelObservationContext extends ModelObservationContext<Prompt, ChatResponse>`; `Prompt.getInstructions()`), and the class is compiled for Java 17 (major 61).
- The instrumentor's own pom asks for `spring-ai-model` 1.0.1 at runtime; Maven resolves 2.0.2 (nearest-wins). No `NoSuchMethodError` or `NoClassDefFoundError` appeared in any run (run `oi-both-1`, `oi-oi-1`, `oi-logs`), and all the attributes below were written.
- Phoenix's snippet (`springai-tracing.mdx`, the "Run Spring AI" block) does not compile on 2.0.2: `org.springframework.ai.openai.api.OpenAiApi` is absent and `OpenAiChatModel.Builder` has no `openAiApi(...)` or `defaultOptions(...)` (javap, run; the Builder's methods are `openAiClient`, `options`, `observationRegistry`, `toolCallingManager`, `meterRegistry`, `build`). The working wiring in the run is the Java spike's (`OpenAIOkHttpClient` → `openAiClient`), plus `ChatClient.builder(model, registry, null, null)` for the tool round (the Java spike, finding 1).

**Wiring that works (run `oi-both-1`, `oi-oi-1`; docs: `F/projects/oi-spring/src/main/java/m8/OiSpringRun.java`).**

```java
TraceConfig cfg = TraceConfig.getDefault();                    // content on
OITracer oiTracer = new OITracer(tracer, cfg);                 // tracer = otel.getTracer(...)
registry.observationConfig().observationHandler(new SpringAIInstrumentor(oiTracer));
OpenAiChatModel model = OpenAiChatModel.builder().openAiClient(client).observationRegistry(registry).build();
ChatClient chat = ChatClient.builder(model, registry, null, null).defaultToolCallbacks(weather).build();
```

The model's observations reach the handler because the model is built with the registry. The tool round needs the `ChatClient` (2.0.2 `OpenAiChatModel` does not run tools; Java spike finding 1).

**Spans and attributes (run `oi-oi-1`, raw keys; `oi-both-1` for the Spring spans).**

| Span (raw name) | Kind | Parent | Keys (raw) |
|---|---|---|---|
| `generate` (call 1) | CLIENT (OTLP kind 3, run: `h.load` on `oi-oi-1`) | none (root, own trace) | `openinference.span.kind=LLM`, `llm.model_name=gpt-4o-mini`, `llm.system=spring-ai`, `llm.provider=openai`, `llm.input_messages.0.message.role=user`, `…0.message.content`, `input.value` (JSON), `input.mime_type=application/json`, `llm.output_messages.0.message.role=assistant`, `…content=""`, `…message.tool_calls.0.tool_call.id=call_m7_1`, `…tool_call.function.name=get_weather`, `…tool_call.function.arguments`, `output.value`, `output.mime_type`, `llm.token_count.prompt=82`, `.completion=17`, `.total=99`, `llm.finish_reason=TOOL_CALLS` |
| `generate` (call 2) | CLIENT (kind 3) | none | as call 1, plus `llm.input_messages.1` (assistant with the tool call), `llm.input_messages.2` (`role=tool`, `message.content` = the tool's result, `message.tool_call_id`), tokens 131/12/143, `llm.finish_reason=STOP`, `llm.output_messages.0.message.content` = the answer |

- Span kinds (run, `h.load` on `oi-oi-1` and `oi-spring-1`): the OI `generate` spans are CLIENT (3); the Spring spans are INTERNAL (1). The Java spike (§ 5.1) wrote "kind unset (the dump shows kind=None)" for the Spring spans; that was wrong: the dump's `kind` field is not printed for every span, and the raw kind of `J/runs/spring-ai-3` is 1 (INTERNAL) on all seven spans (run; § 8 corrections).
- No `setInvocationParameters` attribute: the fake call sets no temperature, max tokens or top_p (no `llm.invocation_parameters` key in the run).
- Span events: 0 on every span (`events=0`). Content is in attributes, not in events, and not in logs. (The Java spike found Spring's own content handlers log at INFO; the OI handler does not log content.)
- `onEvent` logs "event occurred" at INFO when a Micrometer event fires; no such event was fired in these runs.

**Content default and switch (run `oi-oi-1`, `oi-hidden`, `both-hidden`; docs `TraceConfig.java`, `SpringAIInstrumentor.java`).**

- Default: **on** (`TraceConfig.getDefault()` builds with every hide flag false). The instrumentor gates `llm.input_messages`, `input.value`, `llm.output_messages`, `output.value` on `hideInputs`/`hideOutputs`/`hideInputMessages`/`hideOutputMessages`.
- Switch: code only (`TraceConfig.builder().hideInputs(true).hideOutputs(true)`). This library version reads no environment variable for it (`grep -rn getenv` over `dl/src-oi-core` finds none). `TraceConfig.suppressTracing` exists but nothing reads it (docs).
- Run `oi-hidden`: the `generate` spans carry tokens, model and finish reason, and no `input.value`/`output.value`/message attributes (`out/h.log`: `input_preview` absent, `attributes_left` has only `llm.*` and `openinference.span.kind`).

**Duplicates and how to get one set (run, § 1).**

- Both handlers (`oi-both-1`): 9 spans, 2 llm pairs. The OI `generate` is a child of the advisor span `call` (the Micrometer handler makes its span current, so `Context.current()` finds it). Each model call has a `chat gpt-4o-mini` (`gen_ai.*`, tokens, no content) and a `generate` (`llm.*`, tokens, content).
- The OI handler alone: 2 roots; no tool and no agent structure. This is the set that has content and no duplicate.
- The Micrometer handler alone: the Spring spans only; no content.
- So a single set with both structure and content needs either a normalizer rule that drops one of the two llm spans of the same call, or a decision to take the OI spans alone and accept that they are roots unless the app opens a span (§ 7).

**Normalizer (run, `out/h.log` in each run).**

- `generate` (OI only, `oi-oi-1`): kind `llm`, provider `openai` (from `llm.provider`), request model `gpt-4o-mini` (from `llm.model_name`), tokens 82/17 and 131/12, cost computed (2.25e-05 and 2.685e-05), `input_preview` read, `output_preview` read on the second call only. Conventions: `openinference`. Attributes left: `llm.finish_reason`, `llm.model_name`, `llm.provider`, `llm.system`, `llm.token_count.*`, `openinference.span.kind`, `input.mime_type`, `output.mime_type`.
- `generate` with `M8_HIDE` (`oi-hidden`): kind `llm`, provider, model, tokens and cost as above; no I/O (the normalizer reports no preview).
- Both handlers (`oi-both-1`): the Spring `chat gpt-4o-mini` rows are also `llm` (tokens from `gen_ai.usage.*`, model from `gen_ai.request.model`), so 4 llm rows for 2 calls. The OI `generate` rows and the Spring `chat` rows carry the same tokens; a token sum over the trace counts each call twice.
- Tool row: `execute_tool get_weather` (Spring's tool span) is kind `tool`, no tool name (`spring.ai.tool.definition.name` is not read; Java spike finding 3). The OI tool span is absent in this release (the instrumentor has no tool handling).
- Framework spans (`spring_ai chat_client`, `tool _calling `, `call`) are `span` rows with `operation=framework`; they carry `gen_ai.system=spring_ai`, which cetra's framework table already covers (Java spike).

**Findings, short.**

1. The 0.1.10 instrumentor works on Spring AI 2.0.2 for the model path and the tool round (run). The Phoenix snippet does not compile on 2.0.2 (javap).
2. Content is on by default, on the span as attributes, with no event and no log. The switch is code-only in 0.1.10.
3. With Micrometer's OTel handler on the same registry, each call produces two llm spans with the same tokens. Alone, the OI handler gives one llm span per call, as roots unless the app span is current.
4. The OI spans carry the tool-call messages and the tool result in `llm.input_messages.*`; no tool span exists.
5. The OI handler needs no Spring Boot wiring to run; in Boot, see § 5.2.

### 5.2 Spring Boot route (Boot 4.1.2, Spring AI 2.0.2 starter, Boot's OpenTelemetry starter)

**Versions and why this line (docs and run).**

- Spring Boot **`4.1.2`** (`spring-boot-starter-parent`, `spring-boot-dependencies`; both published 2026-10-22T12:03Z). The newest 4.x eligible: `4.0.9` has the same date; `4.0.10`, `4.1.3` and `4.2.0` are 2026-11-19 (too new) (`F/dl/*`, Last-Modified). The spring-ai-2.0.2 poms do not name a Boot version (`spring-ai-parent-2.0.2.pom`, `spring-ai-bom-2.0.2.pom`, `spring-ai-starter-model-openai-2.0.2.pom` have no `spring-boot` version; the starter names Boot starters without versions). So the line is a choice, not a requirement read from Spring AI. Resolved Spring Framework: `spring-core` 7.0.10 (`F/projects/boot-ai/cp.txt`), the same Framework as the Java spike's Spring AI jars.
- Spring AI starter `spring-ai-starter-model-openai` **2.0.2** (pom 2026-10-22T12:46:16Z), with the Spring AI BOM 2.0.2 imported (dependencyManagement).
- Boot's OpenTelemetry starter: `spring-boot-starter-opentelemetry` 4.1.2 (pulls `micrometer-tracing-bridge-otel` 1.7.2, `opentelemetry-exporter-otlp` 1.62.0 and `micrometer-registry-otlp`; `F/dl/starter-otel-4.1.2.pom`).
- OpenInference instrumentor `0.1.10` (as Q1).

**Trap found (run, `F/logs/boot-ai-tree.txt` and the first resolve).** The instrumentor's pom declares `spring-ai-model` **1.0.1** at runtime. Without the Spring AI BOM in the Boot project, Maven's nearest-wins picks `spring-ai-model` **1.0.1** (depth 2, from the instrumentor), not 2.0.2 (depth 3, through the starter). Reproduced with the BOM block removed (`F/projects/boot-ai-nobom/pom.xml`): `mvn dependency:tree` shows `com.arize:openinference-instrumentation-springAI:0.1.10` → `org.springframework.ai:spring-ai-model:jar:1.0.1:compile` (`F/logs/boot-ai-nobom-tree2.txt`, lines 24-25), and the first classpath had `spring-ai-model-1.0.1.jar`. With the BOM imported, the same resolution gives 2.0.2 (`F/projects/boot-ai/cp.txt`). The plan must import the Spring AI BOM (or pin `spring-ai-model`) in the Boot project.

**Program** (`F/projects/boot-ai/src/main/java/m8/BootRun.java`). `@SpringBootApplication`, `WebApplicationType.NONE`, one `ApplicationRunner` that makes one `ChatClient` call with the `@Tool` method `getWeather`. Key lines:

```java
public static void main(String[] args) {
    int code = SpringApplication.exit(new SpringApplicationBuilder(BootRun.class).web(WebApplicationType.NONE).run(args));
    System.exit(code);
}
@Bean ApplicationRunner oneCall(ChatClient.Builder chat, WeatherTools tools) {
    return args -> System.out.println("answer: " + chat.defaultTools(tools).build().prompt()
            .options(OpenAiChatOptions.builder().model("gpt-4o-mini")).user("What is the weather in Lyon? Use the tool.")
            .call().content());
}
@Bean @ConditionalOnProperty(name = "m8.oi", havingValue = "true")          // the OI variant
ObservationHandler<Observation.Context> openInferenceHandler(OpenTelemetry otel) {
    return new SpringAIInstrumentor(new OITracer(otel.getTracer("m8-boot"), TraceConfig.getDefault()));
}
```

Properties (`F/projects/boot-ai/props/route-b.properties`, the file that works; `route-a.properties` is the brief's key set):

```properties
spring.ai.openai.api-key=${OPENAI_API_KEY}
spring.ai.openai.base-url=${M8_BASE_URL}/v1
spring.ai.openai.chat.options.model=gpt-4o-mini
spring.ai.chat.observations.log-prompt=false
spring.ai.chat.observations.log-completion=false
management.tracing.sampling.probability=1.0
management.opentelemetry.tracing.export.otlp.endpoint=${M8_BASE_URL}/v1/traces
management.opentelemetry.tracing.export.otlp.headers.Authorization=Bearer sk-fake-m8
management.otlp.metrics.export.url=${M8_BASE_URL}/v1/metrics
management.otlp.metrics.export.headers.Authorization=Bearer sk-fake-m8
```

**Property keys (docs and run).**

- **Route A, the brief's keys (`management.otlp.tracing.endpoint`, `management.otlp.tracing.headers.*`), does not export traces with the OTel starter.** Run `boot-plain-1` (route A): 0 trace posts (`recv/` empty); the batch exporter connected to `localhost:4318` (`trace.strace`, two connects to port 4318: metrics and traces). The property keys exist in Boot's metadata (`spring-boot-micrometer-tracing-opentelemetry-4.1.2`'s `spring-configuration-metadata.json`, `management.otlp.tracing.endpoint`, `.headers`, `.export.enabled`), but the OTel starter's tracer does not use them (run).
- **Route B** (`management.opentelemetry.tracing.export.otlp.endpoint` and `.headers`, metadata in `spring-boot-opentelemetry-4.1.2`/`spring-boot-micrometer-tracing-opentelemetry-4.1.2`): traces posted to the fake, protobuf (`recv/0002-traces.bin`), header `Authorization: Bearer sk-fake...` (17 characters) decoded (`runs/boot-plain-b/http.jsonl`).
- Metrics: Boot's OTLP metrics registry (`micrometer-registry-otlp`) publishes to `http://localhost:4318/v1/metrics` by default and logs a WARN when that fails (run `boot-plain-1`, `boot-plain-b` before the metrics URL was set). With `management.otlp.metrics.export.url` set (route B), metrics go to the fake (`recv/0001-metrics.bin`) and no WARN.
- Sampling: the default is `management.tracing.sampling.probability = 0.1` (metadata, run value; `tracing` default). The runs set 1.0. A capture without this key would sample 10 percent of traces.
- Resource: `service.name` = `unknown_service` (no `spring.application.name` set; run `boot-oi-b`, resource attributes).
- Content: Spring AI's `spring.ai.chat.observations.log-prompt` and `log-completion` (metadata, default `False`) log content (not spans). Set to false in the runs; no content on Spring's spans in any run (no events, no content attributes; `boot-plain-b`, `boot-oi-b`).

**Runs (`F/runs/boot-*`).**

| Run | Route | OI bean | Spans | Result |
|---|---|---|---|---|
| `boot-plain-1` | A (brief keys) | no | 0 (no trace post) | `answer` correct; WARN for metrics at `localhost:4318`; exit 0 via `SpringApplication.exit` |
| `boot-oi-1` | A | yes | 0 | as above (OI bean registered; no span exported) |
| `boot-plain-b` | B | no | 9 | the Spring spans and two HTTP client spans |
| `boot-oi-b` | B | yes | 11 | 9 Spring-side spans and two `generate` spans (scope `m8-boot`) |
| `boot-plain-hold` | B, no `SpringApplication.exit` and no `System.exit` (`-Dm8.hold`) | no | 9 | the JVM ended by itself after the runner (7 s, exit 0); all 9 spans flushed |

**Span inventory (run `boot-plain-b`, `boot-oi-b`; scope `org.springframework.boot@4.1.2` for Spring and Boot's own spans, `m8-boot` for the OI handler; kinds from `h.load`, the raw OTLP `kind` field).**

| Span (raw name) | Kind (raw) | Parent | Keys (raw, cut) | cetra's normalizer |
|---|---|---|---|---|
| `spring_ai chat_client` | INTERNAL (1) | root | `gen_ai.operation.name=framework`, `gen_ai.system=spring_ai`, `spring.ai.kind=chat_client` | `span`, `framework` |
| `tool _calling ` | INTERNAL | #0 | `spring.ai.advisor.name=Tool Calling Advisor` | `span`, `framework` |
| `call` (×2) | INTERNAL | #1 | `spring.ai.advisor.name=call` | `span`, `framework` |
| `chat gpt-4o-mini` (×2) | INTERNAL | the `call` | `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.usage.input_tokens` 82/131, `gen_ai.usage.output_tokens` 17/12, `gen_ai.response.finish_reasons` | `llm`, `openai`, `gpt-4o-mini`, tokens; no I/O |
| `POST` (×2) | CLIENT (3) | the `chat` span | `method`, `uri`, `status`, `host`, `target.host`, `target.port`, `target.scheme`, `outcome` (Micrometer HTTP client observation names) | `span`, status unset; attributes left; **noise** |
| `execute_tool getWeather` | INTERNAL | `tool _calling ` | `spring.ai.tool.definition.name=getWeather`, `spring.ai.tool.call.id` | `tool`, no tool name |
| `generate` (OI, ×2) | CLIENT (3) | the `call` (parent set by Micrometer's bridge, as in Q1) | the Q1 `llm.*` set, `openinference.span.kind=LLM`, `input.value`, `output.value` | `llm`, `openai`, `gpt-4o-mini`, tokens 82/17 and 131/12, I/O read on the 2nd |

- Noise: no HTTP server spans (no web), no Boot startup spans. The two `POST` spans are the only Boot-generated spans (HTTP client, one per model request). The framework spans are the Q1 ones.
- Duplicates: with the OI bean, each model call has `chat` (gen_ai, tokens) and `generate` (llm.*, tokens, content), as in Q1. Token totals double in a trace sum.
- Exit: with `SpringApplication.exit` and `System.exit`, the JVM exits 0 after the runner. Without them (`boot-plain-hold`), the JVM also ended by itself (7 s, exit 0; `F/runs/boot-plain-hold/run.log`). The Boot OpenTelemetry starter flushed all 9 spans at exit (`recv/0002-traces.bin`, 9 spans in `out/h.log`).

**Boot wiring for the OI handler (docs and run).**

- Boot registers `ObservationHandler` beans on its `ObservationRegistry` automatically: the bean in `openInferenceHandler` was invoked with no code on the registry (run `boot-oi-b`: 2 `generate` spans with the Spring Boot `ChatClient`). The mechanism is Boot's `ObservationRegistryPostProcessor` (`spring-boot-micrometer-observation-4.1.2.jar`, class `org.springframework.boot.micrometer.observation.autoconfigure.ObservationRegistryPostProcessor`; class present, body not read; docs). So the Boot wiring is "a bean", not `observationConfig().observationHandler(...)`.
- The OTel instance comes from Boot's OTel starter (`io.opentelemetry.api.OpenTelemetry` bean, used in `openInferenceHandler`; it worked, so the bean exists in this starter).
- Both Boot's tracing bridges are on the classpath (`micrometer-tracing-bridge-otel` 1.7.2 and `micrometer-tracing-bridge-brave` 1.7.2 through the instrumentor's transitive Brave bridge). Boot used the OTel bridge (the spans are OTel, from the OTel exporter). Brave's presence was not isolated.

**Normalizer (run `out/h.log` in `boot-oi-b`).** Same as Q1 for the Spring and OI spans. New: the two `POST` rows are generic `span` rows with `status=unset` and their `method`/`uri`/`status`/`host`/`target.*`/`outcome` keys left; they need a rule (drop or keep as `span`) if they reach a user's trace list.

**Findings, short.**

1. Route B is the working Boot key set for OTLP traces (route A, the brief's keys, exports nothing with this starter). Metrics need their own URL or they go to `localhost:4318`.
2. The Boot run is one JVM, exits by itself after the runner with `WebApplicationType.NONE` (no explicit exit needed). Sampling must be set (default 0.1).
3. The OI handler works as a Boot bean. The spans and the duplicates are the same as in plain Java (Q1).
4. Boot's own noise is two HTTP client `POST` spans per call. No HTTP server span.
5. The Spring AI BOM must be imported (the instrumentor's pom would pin Spring AI model 1.0.1 otherwise).

### 5.3 openai-go v3.100.0 with OpenInference Go middleware v0.1.2

**Compile and run (run).** Yes, both. The program compiles against `github.com/openai/openai-go/v3` v3.100.0 with the middleware from `openinference-instrumentation-openai-go` v0.1.2 passed to `option.WithMiddleware` unchanged. The build succeeded on the first attempt once `-buildvcs=false` was given (the first attempt only failed on VCS stamping of the scratch module, `F/logs/go-build1.log`). Runs `go-1` (chat and one tool round) exit 0.

Two facts behind "unchanged":

- v3's `option.Middleware` is a type alias of `func(*http.Request, MiddlewareNext) (*http.Response, error)` (`F/go/gopath/pkg/mod/github.com/openai/openai-go/v3@v3.100.0/option/requestoption.go:98`, `type Middleware = func(...)`). The v1 middleware returned by the instrumentor has the same underlying signature, so the value assigns to v3's `WithMiddleware` (docs and build).
- The instrumentor's package imports `github.com/openai/openai-go/option` (v1) (`openai.go`, go.mod requires v1.12.0). So the binary links **both** SDKs: `go list -deps` shows the v1 `option` package and internal packages of v1 next to 25 v3 packages (`F/logs/go-deps.txt`, run). The v3 client does not use the v1 code; the link is a supply-chain and size fact, not a behaviour one.

**Program** (`F/projects/go-openai/main.go`, module `m8.followup/goopenai`). Key lines:

```go
exp, _ := otlptracehttp.New(ctx,
    otlptracehttp.WithEndpointURL(base+"/v1/traces"),
    otlptracehttp.WithHeaders(map[string]string{"Authorization": "Bearer sk-fake-m8"}))
tp := sdktrace.NewTracerProvider(sdktrace.WithBatcher(exp), sdktrace.WithResource(res))
otel.SetTracerProvider(tp)
client := openai.NewClient(
    option.WithAPIKey("sk-fake-m8"),
    option.WithBaseURL(base+"/v1/"),
    option.WithMiddleware(openaiotel.Middleware(otel.Tracer("m8-openai-go"))))
// tools: openai.ChatCompletionFunctionTool(shared.FunctionDefinitionParam{Name: "get_weather", …})
// two calls: Chat.Completions.New, then a tool message (openai.ToolMessage) and a second New
```

Exporter: OTLP/HTTP protobuf (`recv/0001-traces.bin`), header `Authorization: Bearer sk-fake...` (17 characters) decoded at the sink (`runs/go-1/http.jsonl`). Resource `service.name=m8-openai-go`. The program passes the endpoint and header in code; no `OTEL_EXPORTER_OTLP_*` variable was set for the run.

**Spans (run `go-1`, `F/runs/go-1/out/go-1.dump.txt`).** Two spans, both `openai.chat.completions.create`, scope `m8-openai-go`, kind INTERNAL (raw kind 1; the instrumentor does not set CLIENT), both roots in separate traces (the program's context has no parent; run `go-1`).

| Span | Parent | Keys (raw, selected) | cetra's normalizer |
|---|---|---|---|
| call 1 | none | `openinference.span.kind=LLM`, `llm.system=openai`, `llm.provider=openai`, `llm.model_name=gpt-4o-mini`, `llm.invocation_parameters={"model":"gpt-4o-mini"}`, `llm.input_messages.0.*` (user), `input.value` (the user text as a plain string, not JSON), `llm.tools.0.tool.json_schema` (the tool), `llm.output_messages.0.message.tool_calls.0.tool_call.{id,function.name,function.arguments}`, `llm.finish_reason=tool_calls`, `llm.token_count.prompt=82`, `.completion=17`, `.total=99` | llm; provider `openai`; model `gpt-4o-mini`; tokens 82/17/99; cost computed; `input_preview` read; no output preview (the output is a tool call, not text) |
| call 2 | none | as call 1, plus `llm.input_messages.1` (assistant with the tool call), `llm.input_messages.2` (`role=tool`, `content` = the tool result, `tool_call_id`), `llm.output_messages.0.message.content` (the answer), `output.value`, `llm.finish_reason=stop`, tokens 131/12/143 | llm; provider `openai`; model; tokens 131/12; `input_preview` and `output_preview` read |

- The middleware creates no tool span. The local tool's execution is not traced (the program calls it; the middleware sees only HTTP).
- Attribute keys the normalizer leaves: `llm.finish_reason`, `llm.invocation_parameters`, `llm.model_name`, `llm.provider`, `llm.system`, `llm.token_count.*`, `openinference.span.kind` (the same `openinference` convention keys as the Java OI path, § 5.1).
- Note the content in `llm.tools.0.tool.json_schema` (a tool definition) and in `llm.input_messages`; cetra's normalizer does not read the tool definitions (no change here).

**Content default and switch (runs `go-hidden`, `go-1`; docs `traceconfig.go:15-28`, `openai.go:103`).**

- Default: **on**. Attributes `input.value`, `output.value`, `llm.input_messages.*`, `llm.output_messages.*` are present (run `go-1`).
- Switch: environment, read by the Go package: `OPENINFERENCE_HIDE_INPUTS`, `OPENINFERENCE_HIDE_OUTPUTS`, `OPENINFERENCE_HIDE_INPUT_MESSAGES`, `OPENINFERENCE_HIDE_OUTPUT_MESSAGES`, `…_INPUT_TEXT`, `…_OUTPUT_TEXT`, `…_LLM_INVOCATION_PARAMETERS`, `…_LLM_TOOLS`, `OPENINFERENCE_HIDE_PROMPTS`, `…_INPUT_IMAGES` (`traceconfig.go:19-28`). Run `go-hidden` (`OPENINFERENCE_HIDE_INPUTS=true OPENINFERENCE_HIDE_OUTPUTS=true`): no `input.value`, `output.value` or message attributes; the tokens, model and finish reason stay. This is the environment switch that the Java 0.1.10 instrumentor does not have (Q1: code only).

**Parenting (run `go-app2`).** When the program opens its own span and passes its context (`M8_APP_SPAN=1`), both middleware spans are children of `app.request` (one trace, scope `m8-app`). Without it, each call is a root (run `go-1`). The same as Java's OI-only mode (Q1, `oi-app-1`).

**Normalizer (runs `go-1`, `go-hidden`, `go-app2`, `F/runs/*/out/h.log`).** The two rows are `llm` with provider, model and tokens, as in the table above. With content hidden, no I/O is read. There is no duplicate: the OpenInference Go path emits one span per call and no Micrometer-like second set (no framework spans), so the llm count is 1 per call.

**Findings, short.**

1. openai-go v3.100.0 + OpenInference Go v0.1.2 compile and run; the middleware is accepted as is. The Phoenix page's claim ("v0.1.1+ works with v3") is consistent with the build (v0.1.1 was not run; only its `go.mod` was read, § 7 item 9).
2. Content on by default, as attributes; env switches `OPENINFERENCE_HIDE_*` hide it. Span kinds INTERNAL (not CLIENT).
3. One llm span per call, no duplicates, no tool span, no framework spans. Tool names are inside `llm.output_messages`/`llm.input_messages`, not on a tool span.
4. The binary links openai-go v1 (option and internal packages) because the instrumentor imports v1.

## 6. Network

- Every program run was inside `unshare -Urn` through `m7/tools/netrun.sh` (loopback only), with the fake provider and OTLP sink (`m7/tools/server.py`). A scan of every run's `trace.strace` (`F/runs/*/trace.strace`, 17 runs with `connect()` calls) found **no connect to a non-loopback address** (run: `connect()` lines filtered to `127.*`, `::1`, `::ffff:127.*`). Connects to `localhost:4318` (Boot's default OTLP metrics URL, `boot-plain-1`, `boot-plain-b` before the metrics URL was set; Java's `spring-ai-2` in the earlier spike) are loopback refusals, not traffic.
- The "real home" guard reports one path, the calling shell's working directory (the worktree), in the runs whose `strace` opened it. No file under the real home was read or written by a program (`runs/*/real-home.txt`).
- Downloads and fetches (all official, § 9): Maven Central (repo1.maven.org), go.dev (the Go toolchain), proxy.golang.org (Go modules, the `.info` and `.mod` files and `list`). Nothing else was contacted. One deviation: `GOSUMDB=off` (see § 7 item 6).
- Builds ran outside the namespace (Maven, `go build`); runs ran inside it.

## 7. Open questions (for the user or the plan)

1. **Spring AI: one set of spans or two.** Q1 found three working shapes: OpenInference handler alone (content, one llm span per call, roots unless the app has a current span), Micrometer's OTel handler alone (framework and tool spans, no content), and both (framework and tool spans plus duplicate llm spans with the same tokens; `oi-both-1`, `both-hidden`). Choose one, or ask for a normalizer rule that drops the Micrometer `chat` span when an OpenInference `generate` span of the same call exists (the plan decides; facts in § 5.1).
2. **Boot line.** Spring AI 2.0.2's poms do not name a Boot version (`spring-ai-parent-2.0.2.pom`, `spring-ai-bom-2.0.2.pom`, the starter pom). The spike picked Boot `4.1.2` (newest eligible 4.x; `4.0.9` has the same date). Confirm the line, or ask the Spring AI project which Boot line 2.0.2 targets (docs not found in the pom).
3. **Boot OTLP keys.** The brief's keys (`management.otlp.tracing.endpoint`, `management.otlp.tracing.headers`) export **no traces** with Boot 4.1.2's OpenTelemetry starter (run `boot-plain-1`). The working keys are `management.opentelemetry.tracing.export.otlp.endpoint` and `…headers` (run `boot-plain-b`). Confirm the capture uses the OpenTelemetry starter and these keys. Why the Micrometer keys are ignored was not traced.
4. **Boot sampling.** The default `management.tracing.sampling.probability` is `0.1` (metadata). A capture must set `1.0`, or drop 90 percent of traces. Decide the capture's sampling.
5. **Boot metrics.** Boot's OTLP metrics registry posts to `http://localhost:4318/v1/metrics` unless `management.otlp.metrics.export.url` is set (WARN on failure). Metrics are not spans; the capture must either set the URL or disable metrics.
6. **Go checksum database.** `sum.golang.org` is not on the allowed download list. The spike set `GOSUMDB=off` for the fetch and relied on the proxy zips and go.sum `h1` values (§ 2.1). Approve the checksum database (one more host) or accept the proxy-only trust for the capture build.
7. **Go binary links openai-go v1.** The OpenInference Go middleware (v0.1.2, and v0.1.1 by its `go.mod`) requires openai-go v1.12.0 and imports its `option` package, so the binary carries both SDKs (§ 5.3). Accept, or wait for a release without the v1 import (not checked for newer upstream commits).
8. **Java 0.1.10 content switch is code-only.** The Java instrumentor has no environment switch (`TraceConfig` only); the Go package has `OPENINFERENCE_HIDE_*`. The capture policy for content must be set in code for Java and by environment for Go. Decide the policy.
9. **OpenInference Go v0.1.1.** The brief's "v0.1.1+" claim is only checked here by `go.mod` (v0.1.1 requires openai-go v1.12.0, the same as v0.1.2). It was not built or run. The plan can pin v0.1.2 (run) or ask for a v0.1.1 run.
10. **Normalizer gaps seen again (facts, no change proposed here).** (a) Duplicate llm spans for one model call when two handlers run (Java and Boot, Q1 and Q2). (b) Boot's `POST` HTTP client spans (`method`, `uri`, `status`, `host`, `target.*`, `outcome`) are generic `span` rows, not dropped. (c) Tool names are not read from `spring.ai.tool.definition.name`, `gen_ai.tool.name` on tool spans, or `tool.name`; the OpenInference Go middleware has no tool span. (d) Tool calls in `llm.output_messages.*` are not read as output text (the first-call output is empty text). (e) `llm.tools.*` (tool definitions) and `llm.invocation_parameters` are left.
11. **Phoenix's Spring AI snippet does not compile on Spring AI 2.0.2** (`OpenAiApi` absent; builder methods differ). Any capture doc that copies it needs the 2.0 API (`OpenAIClient` + `openAiClient(...)`, § 5.1).
12. **Phoenix's OpenInference Go claim "v0.1.1+ works with v3"** is consistent with the build (§ 5.3), but Phoenix's own page imports the v1 path in its first snippet. Not a blocker; noted.

## 8. Corrections to earlier spikes (facts found here)

- **`m8-java.md` § 5.1, span kinds.** The Java spike wrote that the Spring AI spans have "kind unset (the dump shows `kind=None`)". The raw OTLP kind of every span in `J/runs/spring-ai-3` is **1 (INTERNAL)** (run: `h.load` on that run, this session). The `kind=None` in the dump is an artefact of `h.py`'s dump, which does not print the kind for these spans. Consequence: cetra's normalizer sees INTERNAL, not unset; the kind rule must not depend on "unset".
- **`m8-java.md` § 5.1, the Spring AI provider.** Unchanged: the Spring AI 2.0.2 spans carry `gen_ai.system=spring_ai` on noise spans and `openai` on the llm spans (confirmed again, Q1 and Q2).
- **`m8-java.md` § 5.1, "content in events".** Unchanged (no span events in Q1 or Q2).

## 9. Downloads

All under `F/` (evidence) or `F/cache/m2` (Maven). Every download is official (rule 3). Sizes in bytes; sha256 of the file as downloaded (Maven artifacts: the cache copy).

| File | Source URL | Bytes | sha256 |
|---|---|---|---|
| `F/dl/openinference-instrumentation-springAI-0.1.10.jar` | `https://repo1.maven.org/maven2/com/arize/openinference-instrumentation-springAI/0.1.10/openinference-instrumentation-springAI-0.1.10.jar` | 9,732 | `e9f0e7ecd64af5b552539b51ce03e200d6f7da69042678db934acf226770320f` |
| `F/dl/openinference-instrumentation-springAI-0.1.10.pom` | `…/0.1.10/openinference-instrumentation-springAI-0.1.10.pom` | 3,069 | `893ddd5f35cc229a143944c7d2eb5388de94051cddf9fe3428cac1233fd326e6` |
| `F/dl/openinference-instrumentation-springAI-0.1.10-sources.jar` | `…/0.1.10/…-sources.jar` | 4,551 | `29e8ff1dbc6bb3807759ac73d91c31cbbefe23988d27dfeec38db6c885189161` |
| `F/dl/openinference-instrumentation-0.1.10.jar` | `https://repo1.maven.org/maven2/com/arize/openinference-instrumentation/0.1.10/openinference-instrumentation-0.1.10.jar` | 25,933 | `419c6e52553d0084eaba9e0242c8318adc8c9f7381e7cdaad8b13dd85629a31f` |
| `F/dl/openinference-instrumentation-0.1.10-sources.jar` | `…/openinference-instrumentation/0.1.10/…-sources.jar` | 11,584 | `a618536683ed9cbb6c2142e76b477cd090b9f9f1c34a4183309c26d279ee9890` |
| `F/meta/oi-spring-meta.xml` | `https://repo1.maven.org/maven2/com/arize/openinference-instrumentation-springAI/maven-metadata.xml` | 543 | `9a8435d63def993a2c5e1aa1ad05c201d71fa2d5e9fc6cc8ef1b2cb65d2d6426` |
| `F/dl/spring-ai-parent-2.0.2.pom` | `https://repo1.maven.org/maven2/org/springframework/ai/spring-ai-parent/2.0.2/spring-ai-parent-2.0.2.pom` | 3,186 | `23b1f4f7ccc7e6a168893b44e4c6b927dc629970de4099415e901e36d0ff05fe` |
| `F/meta/sai-bom-2.0.2.pom` | `https://repo1.maven.org/maven2/org/springframework/ai/spring-ai-bom/2.0.2/spring-ai-bom-2.0.2.pom` | 37,174 | `c61c54a23d3dff82b0999198428db0023216b96faff5d6b6db5a85aaf04a8c84` |
| `F/dl/boot-starter-parent-4.1.2.pom` | `https://repo1.maven.org/maven2/org/springframework/boot/spring-boot-starter-parent/4.1.2/spring-boot-starter-parent-4.1.2.pom` | 14,470 | `b7310a4672ba54ba552cc74e6f62137890f361c93be04aa56def10a27552f9f5` |
| `F/dl/starter-otel-4.1.2.pom` | `…/spring-boot-starter-opentelemetry/4.1.2/…pom` | 3,480 | `4469abe6d4706cee75b1a3e956c9fded876a4a9c8cf3515016b0bd1a6c962db7` |
| `F/dl/starter-openai-2.0.2.pom` | `…/spring-ai-starter-model-openai/2.0.2/…pom` | 4,226 | `70307e420167e902879fe66e9c56cd90e0ec4e7290296abbc70dc19f889612f1` |
| `F/meta/boot-deps-meta.xml` | `https://repo1.maven.org/maven2/org/springframework/boot/spring-boot-dependencies/maven-metadata.xml` | 10,753 | not recorded (version list only) |
| `F/dl/boot-dependencies-4.1.2.pom` | `https://repo1.maven.org/maven2/org/springframework/boot/spring-boot-dependencies/4.1.2/spring-boot-dependencies-4.1.2.pom` | 141,480 | `3ebfa36de39b9231236456542b09daa0d071b6f4780209323dbe6795a4522124` (a 583-byte copy under `F/meta/` is a broken stub and is not evidence) |
| Spring Boot 4.1.2 jars and Spring AI 2.0.2 jars | Maven cache `F/cache/m2` (hard-linked from J's cache, and Maven downloads for the Boot project) | see § 2.1 | see § 2.1 |
| `F/go/dl/go1.27.4.linux-amd64.tar.gz` | `https://go.dev/dl/go1.27.4.linux-amd64.tar.gz` | 70,680,823 | `7226736c2339719cda28fbf3a54d71aa63bcefe376e98cef00034268dab24840` |
| `F/meta/go-dl.json` | `https://go.dev/dl/?mode=json` | 22,898 | `00c8b9cdf0143ac3cc91c55166d28c6c3c08e70cd114129f76d9aa3a0e076ab9` |
| Go modules (zips, in `F/go/gopath/pkg/mod`) | `https://proxy.golang.org/…/@v/<v>.zip` for the modules in § 2.1 | see `F/logs/go-get.log` | go.sum h1 values in § 2.1 |
| Go `.info`/`.mod` and `list` responses | `https://proxy.golang.org/…` | small | not recorded |
| `F/dl/src-oi-spring/`, `F/dl/src-oi-core/`, `F/dl/bootmeta/` | unpacked copies of the jars above (read only) | | |

Evidence paths: `F/runs/<run>/` (run logs, `recv/`, `http.jsonl`, `out/`), `F/logs/` (Maven, Go and build logs), `F/projects/{oi-spring,boot-ai,go-openai}/` (sources, poms, `cp.txt`), `F/runners/{oi-spring,boot-ai,go-openai}.sh`.

## 9. Follow-up 2 (spike 3)

Status: spike, not a plan. Nothing here changes the design or the decisions. Clock: 2026-11-24, about 14:30 CET. The 7-day cutoff is **2026-11-17 12:30 UTC**.

Evidence root (S3): `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m8/spike3`. Reused, read only: the Boot project and its classpath (`F` = `.../m8/followup`: `F/projects/boot-ai`, `F/cache/m2`), JDK 21.0.13+9 (`m8/java`), the .NET SDK 10.0.404 and the NuGet cache (`m8/dotnet`), the netns tools (`m7/tools`, `m7/norm-target`). Each fact is tagged **run** (evidence path) or **docs** (file:line or URL).

Questions (from the brief): Q1 Boot with the OpenInference handler only (§ 9.1); Q2 Boot metrics (§ 9.2); Q3 .NET traces endpoint (§ 9.3); Q4 Semantic Kernel event bodies, raw (§ 9.4); Q5 Arconia, docs only (§ 9.5).

### 9.0 Summary

- **Q1.** `management.tracing.enabled=false` (the brief's key) has no effect on Boot 4.1.2: the Micrometer `chat` spans still come out. `management.tracing.export.enabled=false` (the key that metadata names as its replacement) stops all trace export, OpenInference spans included. **Working variant:** `spring.autoconfigure.exclude=org.springframework.boot.micrometer.tracing.autoconfigure.MicrometerTracingAutoConfiguration`. It leaves exactly the two OpenInference `generate` spans, with tokens and content, and the JVM exits 0. Without an application span they are two roots (two traces). With the app making its own OTel span current, they are children of `app.request` in one trace. No Spring AI switch in 2.0.2 disables the observation (only content switches, § 9.1 (c)).
- **Q2.** `management.otlp.metrics.export.enabled=false` stops the metrics post (no `0001-metrics.bin`, no WARN). No second key was needed.
- **Q3.** With only `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` set (plus protocol, service name, compression, sampler), **no span arrives**: the .NET exporter connects to the default `localhost:4318` and nothing reaches the sink. With `OTEL_EXPORTER_OTLP_ENDPOINT=<base>` (no path) the same program delivers its spans. `.AddService("m8-sk")` in the program **overrides** `OTEL_SERVICE_NAME` (`m8-sk` with the call, `capture-app` without it).
- **Q4.** Raw event bodies of the three chat spans and the tool span are in § 9.4 (JSON copied byte for byte, the run's path replaced by `<RUN>`).
- **Q5.** Newest eligible Arconia is **0.32.0** (published 2026-11-03). Starter `io.arconia:arconia-opentelemetry-spring-boot-starter:0.32.0`; conventions `io.arconia:arconia-openinference-ai-semantic-conventions:0.32.0`. OTLP keys `arconia.otel.exporter.otlp.endpoint` and `arconia.otel.exporter.otlp.headers`. The OpenInference convention replaces Spring AI's chat-model attributes with OpenInference ones and captures content by default (docs and bytecode, not run).

### 9.1 Q1: Boot with the OpenInference handler only

**Versions.** As § 2 and § 2.1 (Boot 4.1.2, Spring AI 2.0.2 starter, OpenInference Spring AI 0.1.10, Boot OpenTelemetry starter). No new downloads. `BootRun3` (below) is compiled with `javac` from JDK 21.0.13+9 into `S3/projects/boot3/classes`.

**Setup (run, all Q1 runs).** Route B properties (`F/projects/boot-ai/target/classes/application.properties`, the copy of `route-b.properties`), `-Dm8.oi=true` (the OpenInference handler as a bean, `openInferenceHandler` in `F/projects/boot-ai/src/main/java/m8/BootRun.java`), the same fake provider and OTLP sink. Runner: `S3/runners/boot.sh` (JVM arguments from the run's `env` file, `M8_JVM_ARGS`). Wrapper: `S3/runners/run.sh <name> <timeout> "<jvm args>" <runner>`.

**Micrometer beans (docs: class and metadata of the reused jars).** The Micrometer tracing handler is `io.micrometer.tracing.handler.DefaultTracingObservationHandler`, bean `defaultTracingObservationHandler`, in `org.springframework.boot.micrometer.tracing.autoconfigure.MicrometerTracingAutoConfiguration` (`spring-boot-micrometer-tracing-4.1.2.jar`, `javap -p`). The metadata of the same jar marks `management.tracing.enabled` as `deprecation: {level: error, replacement: management.tracing.export.enabled, since: 4.0.0}`.

**Runs.**

| Run (`S3/runs/`) | Config (added to the setup above) | Spans | Result |
|---|---|---|---|
| `q1a-tracing-enabled-false` | (a) `-Dmanagement.tracing.enabled=false` | 11 | No effect: 9 Spring spans (`chat gpt-4o-mini` ×2, `POST` ×2, framework and tool spans) and 2 `generate`. Exit 0. |
| `q1a2-tracing-export-enabled-false` | (a2, extra) `-Dmanagement.tracing.export.enabled=false`, the replacement key | 0 | No trace post (`recv/` has `0001-metrics.bin` only). The OpenInference spans are gone too. Not a working variant. |
| `q1b-exclude-micrometer-tracing` | (b) `-Dspring.autoconfigure.exclude=org.springframework.boot.micrometer.tracing.autoconfigure.MicrometerTracingAutoConfiguration` | 2 | Only the two `generate` spans (scope `m8-boot`). No `chat`, no `POST`, no framework or tool span. Exit 0. |
| `q1b-app-span` | (b) plus `-Dm8.app=true`: `BootRun3` (below) opens an OTel span `app.request` from Boot's `OpenTelemetry` bean, makes it current around the call and ends it | 3 | `app.request` (root) and two `generate` spans as its children, one trace (`f02dbe0c…`). Exit 0. |

Runs not counted: a first attempt of `q1a`, `q1a2` and `q1b` passed the JVM arguments wrongly (an unquoted value in the `.env` file, fixed in `S3/runners/run.sh`), so they ran as plain Boot with 9 spans; they were rerun. A first `BootRun3` build failed with a bean override (`weatherTools`), because `F/projects/boot-ai/target/classes` was also on the classpath and `@SpringBootApplication` scanned both `BootRun` and `BootRun3`; `BootRun3` runs without that directory. `BootRun3` is a copy of `BootRun` (`S3/projects/boot3/src/m8/BootRun3.java`) with the `m8.app` option only.

**The working variant (runs `q1b-exclude-micrometer-tracing`, `q1b-app-span`).**

```properties
# route B, unchanged (F/projects/boot-ai/props/route-b.properties)
management.tracing.sampling.probability=1.0
management.opentelemetry.tracing.export.otlp.endpoint=${M8_BASE_URL}/v1/traces
management.opentelemetry.tracing.export.otlp.headers.Authorization=Bearer sk-fake-m8
management.otlp.metrics.export.url=${M8_BASE_URL}/v1/metrics
# the one new switch (a system property here; the same key works in the file)
spring.autoconfigure.exclude=org.springframework.boot.micrometer.tracing.autoconfigure.MicrometerTracingAutoConfiguration
```

`java -Dm8.oi=true -Dspring.autoconfigure.exclude=org.springframework.boot.micrometer.tracing.autoconfigure.MicrometerTracingAutoConfiguration -cp ... m8.BootRun`

- **Does OITracer still get a working OTel tracer from Boot?** Yes. The handler gets `otel.getTracer("m8-boot")` from Boot's OpenTelemetry starter `OpenTelemetry` bean, and the spans are exported over OTLP to the route B endpoint (`q1b-exclude-micrometer-tracing`: `recv/0002-traces.bin`, 2 spans in `out/h.log`).
- **Span list (run).** `generate` (call 1) and `generate` (call 2): scope `m8-boot`, kind CLIENT (3), no parent, separate traces. Keys as in § 5.1 (`openinference.span.kind=LLM`, `llm.model_name`, `llm.provider=openai`, `llm.system=spring-ai`, `llm.input_messages.*`, `llm.output_messages.*`, `input.value`, `output.value`, `llm.token_count.*`, `llm.finish_reason`).
- **Tokens and content (run, `h.log`).** Call 1: 82/17 (total 99), finish `TOOL_CALLS`. Call 2: 131/12 (total 143), finish `STOP`. `input_preview` read on both calls. `output_preview` read on call 2 (`hello.txt says: hello from the M7 spike.`). Content is on (default).
- **Parenting.** Without an app span: two roots. With the app span (`q1b-app-span`): `app.request` is the root and both `generate` spans are its children in one trace. The fix is the application span; Micrometer's handler is not needed for it.
- **JVM.** Exit 0 by itself after the runner (`run.log`: `java exit=0`), with `WebApplicationType.NONE`, as § 5.2.
- **Normalizer (run, `q1b-exclude-micrometer-tracing/h.log`).** Both rows `llm`, provider `openai`, model `gpt-4o-mini`, tokens 82/17 and 131/12, cost computed. With the app span, `app.request` is a `span` row (status unset, no attributes left).

**Question (c): documented switches.**

- Spring AI 2.0.2 has no switch in its metadata that turns the chat observation off. Its keys are `spring.ai.chat.observations.log-prompt`, `log-completion`, `include-error-logging` (default false; `spring-ai-autoconfigure-model-chat-observation-2.0.2.jar`, `META-INF/spring-configuration-metadata.json`) and `spring.ai.chat.client.observations.log-prompt`, `log-completion` (`spring-ai-autoconfigure-model-chat-client-2.0.2.jar`). These gate content in logs. Not run.
- Boot has `management.observations.enable.*` (`spring-boot-actuator-autoconfigure-4.1.2.jar`, metadata). Not run; whether it would also remove the OpenInference handler's input was not checked.
- OpenInference 0.1.10 has no environment switch (§ 5.1). The Arconia route has one (§ 9.5).

**Caveats for the plan.** (1) The exclusion removes Micrometer's tracing handler for every observation in the application, not only Spring AI's. This app has no web layer; the HTTP client `POST` spans are gone in `q1b` (run). A web app would lose its own Micrometer spans the same way (expected from the mechanism; not run). (2) The OpenInference span is a root unless the app opens a span.

### 9.2 Q2: Boot metrics off

- Run `q2-metrics-export-disabled`: route B, OpenInference bean on, plus `-Dmanagement.otlp.metrics.export.enabled=false`. Result: no `recv/0001-metrics.bin` (the default run `boot-oi-b` has one), no `Publishing metrics` log line, no WARN, 11 spans kept, exit 0 (`S3/runs/q2-metrics-export-disabled/`).
- Docs: `management.otlp.metrics.export.enabled` default `true`, "Whether exporting of metrics to this backend is enabled." (`spring-boot-micrometer-metrics-4.1.2.jar`, metadata). The default URL is `http://localhost:4318/v1/metrics` (§ 5.2).
- No second key was needed.

### 9.3 Q3: .NET traces endpoint

**Setup (run).** Program: `S3/src/sk3/Program.cs`, a copy of the `sk` program (`D/src/sk/Program.cs`) with three changes: no endpoint in code (`AddOtlpExporter()` with no arguments); the program's `.AddService("m8-sk")` only when `M8_ADD_SERVICE=1`; the chat endpoint still from `base.url`. Packages as `D/src/sk/m8-sk.csproj`: `Microsoft.SemanticKernel` 1.82.0, `OpenTelemetry` 1.21.0, `OpenTelemetry.Exporter.OpenTelemetryProtocol` 1.21.0. `dotnet build -c Debug` with SDK 10.0.404: 0 warnings, 0 errors; restore used the existing NuGet cache (no new package directory under `D/nuget/packages`).

**Environment (the capture runner's set, as `m8-dotnet.md` § 6.1 reads `tools/capture/src/run.rs`).** `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=<base>/v1/traces` (set by the wrapper from `base.url`), `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`, `OTEL_EXPORTER_OTLP_COMPRESSION=none`, `OTEL_TRACES_SAMPLER=always_on`, `OTEL_SERVICE_NAME=capture-app`. No `OTEL_EXPORTER_OTLP_ENDPOINT`, no headers (the brief's list had none). Env files `S3/runners/env-q3-noadd`, `env-q3-add`, `env-q3-noadd-tp`. Wrapper `S3/runners/sk3.sh`; controls `S3/runners/ctl.sh`, `ctl2.sh`.

**Results (run).**

| Run (`S3/runs/`) | OTLP env | `.AddService` | Spans at the sink | Service name |
|---|---|---|---|---|
| `q3-noadd-sensitive` | `TRACES_ENDPOINT` only (the capture set) | no | **0** | none |
| `ctl-traces` | same env as `q3-noadd-sensitive` (repeat, control) | no | **0** | none |
| `ctl-traces-with-traces-protocol` | as above, plus `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=http/protobuf` | no | **0** | none |
| `ctl-ep-noadd` | `OTEL_EXPORTER_OTLP_ENDPOINT=<base>` (control) | no | 4 | `capture-app` |
| `ctl-ep-add` | `OTEL_EXPORTER_OTLP_ENDPOINT=<base>` (control) | yes | 4 | `m8-sk` |

- **Do spans arrive with only the traces endpoint? No** (this program, OpenTelemetry .NET 1.21.0, `AddOtlpExporter()` with no arguments). The `trace.strace` of the three `TRACES_ENDPOINT` runs has `connect()` to port 4318 on `::1` and `::ffff:127.0.0.1` (the exporter's default), and the sink got no traces file (`recv/` has no `*-traces.bin`). The process did have the variable (`S3/runs/envtest/envdump.txt`: `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:<port>/v1/traces`). The exporter DLL does contain the string `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` (`OpenTelemetry.Exporter.OpenTelemetryProtocol.dll`, UTF-16). The cause was not traced (§ 9.6, 3).
- **`OTEL_EXPORTER_OTLP_ENDPOINT=<base>` works** (control: 4 spans, one `otlp` event on `/v1/traces`).
- **`.AddService("m8-sk")` overrides `OTEL_SERVICE_NAME`:** the resource is `service.name=m8-sk` with the call (`ctl-ep-add`) and `capture-app` without it (`ctl-ep-noadd`), both in `out/*.dump.txt` (RESOURCE line).
- Spans in `ctl-ep-noadd`: 4, all roots, the same shape as `m8-dotnet.md` § 4.2 (chat CLIENT ×3, tool INTERNAL ×1).

### 9.4 Q4: Semantic Kernel event bodies, raw

Source: `S3/runs/ctl-ep-noadd` (program `S3/src/sk3`, `M8_SK_SWITCH=sensitive`, one chat call, a tool round with `read_file`, and the final answer). Dump: `S3/runs/q4-raw-events.txt` (`S3/tools/rawevents.py`). In the tool arguments the run's own directory is replaced by `<RUN>` (the path `.../spike3/runs/ctl-ep-noadd/hello.txt`). Everything else is byte for byte from the protobuf string values.

Four spans, each its own trace, no parent: three `chat gpt-4o-mini` (kind CLIENT, 3, scope `Microsoft.SemanticKernel.Diagnostics`) and `execute_tool read_file` (kind INTERNAL, 1, scope `Microsoft.SemanticKernel`).

Chat span 1 (no tools): tokens 131/12, `gen_ai.response.finish_reason=["Stop"]`, events in order:
- `gen_ai.user.message`: `gen_ai.event.content` = `{"role":"user","name":null,"content":"Say hello in one sentence.","tool_calls":[]}`; `gen_ai.system` = `openai`
- `gen_ai.choice`: `gen_ai.event.content` = `{"index":0,"message":{"role":"Assistant","name":null,"content":"hello.txt says: hello from the M7 spike.","tool_calls":[]},"tool_calls":[],"finish_reason":"Stop"}`

Chat span 2 (asks for the tool): tokens 82/17, `gen_ai.response.finish_reason=["ToolCalls"]`, events:
- `gen_ai.user.message`: `{"role":"user","name":null,"content":"Read hello.txt and tell me what it says.","tool_calls":[]}`; `gen_ai.system` = `openai`
- `gen_ai.choice`: `{"index":0,"message":{"role":"Assistant","name":null,"content":null,"tool_calls":[{"id":"call_m7_1","function":{"name":"read_file","arguments":{"path":"<RUN>/hello.txt"}},"type":"function"}]},"tool_calls":[{"id":"call_m7_1","function":{"name":"read_file","arguments":{"path":"<RUN>/hello.txt"}},"type":"function"}],"finish_reason":"ToolCalls"}`

Chat span 3 (final answer): tokens 131/12, `["Stop"]`, events in order:
- `gen_ai.user.message`: `{"role":"user","name":null,"content":"Read hello.txt and tell me what it says.","tool_calls":[]}`; `gen_ai.system` = `openai`
- `gen_ai.assistant.message`: `{"role":"Assistant","name":null,"content":null,"tool_calls":[{"id":"call_m7_1","function":{"name":"read_file","arguments":{"path":"<RUN>/hello.txt"}},"type":"function"}]}`; `gen_ai.system` = `openai`
- `gen_ai.tool.message`: `{"role":"tool","name":null,"content":"hello from the M8 tool","tool_calls":[]}`; `gen_ai.system` = `openai`
- `gen_ai.choice`: `{"index":0,"message":{"role":"Assistant","name":null,"content":"hello.txt says: hello from the M7 spike.","tool_calls":[]},"tool_calls":[],"finish_reason":"Stop"}`

Tool span `execute_tool read_file` (no events): `gen_ai.operation.name`=`execute_tool`; `gen_ai.tool.name`=`read_file`; `gen_ai.tool.description`=`Reads a file and returns its text.`; `gen_ai.tool.call.arguments`=`{\n  "path": "<RUN>/hello.txt"\n}` (pretty-printed, with newlines); `gen_ai.tool.call.result`=`hello from the M8 tool`.

Facts from the dump:
- Each event has the attribute `gen_ai.event.content` (JSON string) and `gen_ai.system` (`openai`). Event names: `gen_ai.user.message`, `gen_ai.assistant.message`, `gen_ai.tool.message`, `gen_ai.choice`.
- Role spelling differs: `"Assistant"` (capitalized) in `gen_ai.choice` and `gen_ai.assistant.message`; `"user"` and `"tool"` lowercase.
- The `gen_ai.choice` content carries `tool_calls` twice: inside `message` and at the top level.
- The tool-role message has no `tool_call_id`.
- No span attribute carries message content (`gen_ai.input.messages` and `gen_ai.output.messages` are absent): the content is only in the events, as in § 4.2 of `m8-dotnet.md`.

### 9.5 Q5: Arconia (docs and jars only, not run)

**Versions (run: Maven metadata and poms; docs: GitHub).**

- `io.arconia:arconia-opentelemetry-spring-boot-starter`: `maven-metadata.xml` `latest` = `0.32.0`, `lastUpdated` 20261103004857 (`S3/arconia/arconia-opentelemetry-spring-boot-starter.maven-metadata.xml`). The 0.32.0 pom and jar: Maven `Last-Modified` 2026-11-03T00:40:49Z. GitHub release `v0.32.0`: 2026-11-03T00:49:03Z (`https://github.com/arconia-io/arconia/releases/tag/v0.32.0`). Eligible (before the cutoff). No newer release on the GitHub list (`https://github.com/arconia-io/arconia/releases`).
- `io.arconia:arconia-openinference-ai-semantic-conventions`: `latest` = `release` = `0.32.0` (metadata, same date).
- The starter pom (`S3/arconia/arconia-opentelemetry-spring-boot-starter-0.32.0.pom`, lines 10-11 and the dependency list) depends on `arconia-core-bootstrap`, `arconia-opentelemetry`, `arconia-opentelemetry-logback-bridge`, `arconia-opentelemetry-micrometer-registry-otlp` and `arconia-opentelemetry-exporter-sender-http` (all 0.32.0), `spring-boot-starter` 4.1.2, `micrometer-tracing-bridge-otel` 1.7.2 (runtime) and `opentelemetry-exporter-otlp` 1.67.0 (runtime). Its jar holds only `META-INF/MANIFEST.MF` (330 bytes; `unzip -l`).
- The conventions pom (`S3/arconia/arconia-openinference-ai-semantic-conventions-0.32.0.pom`) depends on `arconia-observation` 0.32.0, `openinference-semantic-conventions` 0.1.23, `spring-ai-model` 2.0.2 (with `spring-ai-bom` 2.0.2), `micrometer-observation` 1.17.2 and `jackson-databind` 3.1.7.

**Exact coordinates (pom, docs).** `io.arconia:arconia-opentelemetry-spring-boot-starter:0.32.0` and `io.arconia:arconia-openinference-ai-semantic-conventions:0.32.0`. The Phoenix page lists both without a version and says nothing about which version to use (`/home/sebastien/github/phoenix/docs/phoenix/integrations/java/arconia/arconia-tracing.mdx`, lines 24-25 and 42-48, read only).

**OTLP endpoint and headers (docs and jar metadata).**

- `arconia.otel.exporter.otlp.endpoint` (common). Per signal: `arconia.otel.traces.exporter.otlp.endpoint`, which overrides the common one. Default `http://localhost:4317` (gRPC) or `http://localhost:4318` (HTTP). (`https://docs.arconia.io/arconia/latest/opentelemetry/`, the properties list, and `arconia-opentelemetry-0.32.0.jar`, `META-INF/spring-configuration-metadata.json`.)
- `arconia.otel.exporter.otlp.headers` (map; signal headers are merged into the common ones). Same two sources.
- `arconia.otel.exporter.otlp.protocol`, default `http-protobuf` (or `grpc`). Same two sources.
- With HTTP/Protobuf the common endpoint is a base URL, and each signal exporter appends its path such as `/v1/traces`; a signal endpoint is used as given (`https://docs.arconia.io/arconia/latest/opentelemetry/`, the paragraph after the list, about line 222 of `doc-e46456a6.html.txt`).
- The Phoenix page says the default HTTP target is `http://localhost:4318`, not Phoenix's, and suggests `arconia.otel.exporter.otlp.endpoint=${PHOENIX_COLLECTOR_ENDPOINT}` and `arconia.otel.exporter.otlp.headers=Authorization=Bearer ${PHOENIX_API_KEY}` (`arconia-tracing.mdx`, lines 98-99).
- The Arconia docs' Phoenix and Arize examples use the canonical `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_EXPORTER_OTLP_HEADERS` (`doc-bbc8b419.html.txt`, lines 115-116 and 170). The OTel variables `OTEL_EXPORTER_OTLP_CERTIFICATE`, `..._CLIENT_KEY`, `..._CLIENT_CERTIFICATE` configure an SSL bundle (`doc-e46456a6.html.txt`, line 297). Whether `OTEL_EXPORTER_OTLP_ENDPOINT` maps to `arconia.otel.exporter.otlp.endpoint` was not checked.

**What it does to Spring AI's observations (docs and bytecode, not run).**

- Docs: "Arconia lets you configure your Spring AI applications to export telemetry data adopting the OpenInference Semantic Conventions instead of the default Micrometer conventions" (`https://docs.arconia.io/arconia/latest/observability/semantic-conventions/openinference/`, intro). Recorded, per the same page: chat model calls (model, parameters, tools, input and output messages, token usage), embedding calls, tool executions (arguments and result), chat client calls as agents, advisors as chains or guardrails.
- Content: "Content is captured by default, following the OpenInference specification" (same page). Hide properties `arconia.observations.conventions.openinference.ai.hide-inputs`, `hide-outputs`, `hide-input-messages`, `hide-output-messages`, `hide-input-text`, `hide-output-text`, `hide-llm-tools`, `hide-llm-invocation-parameters`, `hide-input-images`, `hide-embeddings-*` and others, all default `false` (`arconia-openinference-ai-semantic-conventions-0.32.0.jar`, `META-INF/spring-configuration-metadata.json`). The OpenInference environment variables `OPENINFERENCE_HIDE_INPUTS`, `OPENINFERENCE_HIDE_OUTPUTS`, `OPENINFERENCE_HIDE_INPUT_MESSAGES`, `OPENINFERENCE_HIDE_OUTPUT_MESSAGES`, `OPENINFERENCE_HIDE_INPUT_TEXT`, `OPENINFERENCE_HIDE_OUTPUT_TEXT`, `OPENINFERENCE_HIDE_LLM_TOOLS`, `OPENINFERENCE_HIDE_INPUT_IMAGES` (class `OpenInferenceEnvironmentPropertyAdapters`, read with `strings`) take precedence over the properties (same docs page). The OpenInference Java 0.1.10 instrumentor has no such variable (§ 5.1).
- Bytecode: `io.arconia.observation.openinference.ai.instrumentation.OpenInferenceChatModelObservationConvention` extends `org.springframework.ai.chat.observation.DefaultChatModelObservationConvention` (`javap -p`). Its `getLowCardinalityKeyValues` and `getHighCardinalityKeyValues` make no `invokespecial` call to the default's key-value methods (`javap -c`; the only `invokespecial` to the default class is its constructor). Its key strings are OpenInference keys only: `openinference.span.kind`, `input.value`, `output.value`, `input.mime_type`, `output.mime_type`, `llm.model_name`, `llm.request.model_name`, `llm.response.model_name`, `llm.provider`, `llm.system`, `llm.token_count.prompt|completion|total`, `llm.input_messages`, `llm.output_messages`, `llm.tools`, `llm.invocation_parameters`, `llm.finish_reason`. No `gen_ai.*` key string appears in the class. So the convention appears to replace Spring AI's attributes on the same observation rather than add a second span. **This is inferred from the bytecode; it was not run, so no span count is established.**
- `OpenInferenceChatModelObservationHandler` (`ObservationHandler<ChatModelObservationContext>`): in `onStop` it reads `TracingObservationHandler.TracingContext` from the observation context, takes its span and sets the cache-token attributes (`javap -p -c`). So it depends on Micrometer's tracing handler supplying that context. Whether it handles a missing context was not checked. This matters for § 9.1 (b): the exclusion of Micrometer's tracing handler may affect this handler (not run).
- Advisors: `OpenInferenceAdvisorObservationConvention` (`javap`). ChatClient: `OpenInferenceChatClientConfiguration` registers its convention and two handlers (`javap`).
- Conflicts: `arconia-observation` has `MultipleAiObservationConventionsException` and a failure analyzer for more than one AI convention (class names in `arconia-observation-0.32.0.jar`). What the docs say about choosing one was not checked further.

**Not verified (not inferred).** Span count and names with Spring AI 2.0.2; whether Micrometer's `chat` span disappears; the tool-call keys; whether the starter's exporter and Boot's OpenTelemetry starter coexist on one classpath; runtime compatibility with Boot 4.1.2.

### 9.6 Open questions (for the user or the plan)

1. **Q1 capture variant.** The working OpenInference-only variant excludes Micrometer's tracing handler for the whole application. Accept that (and test a web app), or take the Arconia route (§ 9.5, not run). Either way the capture needs an application span for one trace per request.
2. **Q1 with Arconia.** Does Arconia's OpenInference handler still work with the Micrometer tracing handler excluded (it reads `TracingObservationHandler.TracingContext`, § 9.5)? Not run.
3. **Q3 .NET traces endpoint.** Why `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` is not used by OpenTelemetry .NET 1.21.0's `AddOtlpExporter()` here is not traced. The capture runner sets only the traces variable (`m8-dotnet.md` § 6.1). Options for the plan: the runner also sets the base `OTEL_EXPORTER_OTLP_ENDPOINT` for .NET; the program passes the endpoint in code; or another SDK version. Each needs a run before it is chosen.
4. **Q3 service name.** The program must not call `.AddService(...)` if the runner's `OTEL_SERVICE_NAME` is to win.
5. **Q4 normalizer input.** The event bodies in § 9.4 are the inputs for a rule on the four event names. The plan decides whether events are read at all; the sensitive switch is what carries them.
6. **Q5 Arconia.** Needs a run before it is a plan input: span count and names, OpenInference content, whether the handler works without the Micrometer tracing handler, and the effect of Boot's OpenTelemetry starter on the same classpath.

### 9.7 Network

- Every run was inside `m7/tools/netrun.sh` (`unshare -Urn`, loopback only). The `connect()` lines of every run that has a `trace.strace` under `S3/runs/` were checked: **no connect to a non-loopback address** (run; a filter that drops `127.0.0.1`, `::1` and `::ffff:127.0.0.1`). The connects to port 4318 in the `TRACES_ENDPOINT` runs (`q3-noadd-sensitive`, `ctl-traces`, `ctl-traces-with-traces-protocol`) are the exporter's default on loopback, and no OTLP traffic reached the sink from them.
- `netrun.sh` reported `real-home touches: none` in the runs whose `run.log` was read.
- Downloads for Q5 (docs) used `curl` from `repo1.maven.org`, `github.com/arconia-io/arconia` and `docs.arconia.io` only, as the brief allows. The Phoenix page was read from the local checkout.

### 9.8 Downloads (this spike)

All under `S3/arconia/` (Q5). Bytes and sha256 of the file as downloaded. No other download was made in this spike (the Q1, Q2, Q3 and Q4 runs used the reused, already present toolchains and caches).

| File | Source URL | Bytes | sha256 |
|---|---|---|---|
| `arconia-opentelemetry-spring-boot-starter-0.32.0.pom` | `https://repo1.maven.org/maven2/io/arconia/arconia-opentelemetry-spring-boot-starter/0.32.0/arconia-opentelemetry-spring-boot-starter-0.32.0.pom` | 4,334 | `408c1239b0c6d0450c5ca46b2e56883fa7acb5ec3b985f555c825c1269b3a20a` |
| `arconia-opentelemetry-spring-boot-starter-0.32.0.jar` | `https://repo1.maven.org/maven2/io/arconia/arconia-opentelemetry-spring-boot-starter/0.32.0/arconia-opentelemetry-spring-boot-starter-0.32.0.jar` | 330 | `b7fae277d6c44ee4a037572ee47aed23d4e9dbf96bdd122cd7921ee60b477787` |
| `arconia-openinference-ai-semantic-conventions-0.32.0.pom` | `https://repo1.maven.org/maven2/io/arconia/arconia-openinference-ai-semantic-conventions/0.32.0/arconia-openinference-ai-semantic-conventions-0.32.0.pom` | 3,551 | `24ccd69462c956051bf78161b9a23474222fd8842526ed8b4b31ffdf7c1848b2` |
| `arconia-openinference-ai-semantic-conventions-0.32.0.jar` | `…/arconia-openinference-ai-semantic-conventions/0.32.0/arconia-openinference-ai-semantic-conventions-0.32.0.jar` | 44,179 | `44eb5dd70b0206188cd0179c1c8066eb7cd05d534b38dabbbb7230ea95a59626` |
| `arconia-opentelemetry-0.32.0.jar` | `https://repo1.maven.org/maven2/io/arconia/arconia-opentelemetry/0.32.0/arconia-opentelemetry-0.32.0.jar` | 190,106 | `512b9599b5f418f77552096f41b7b4b9bb00e73473f811f00454cc6a8c2d5a1f` |
| `arconia-core-bootstrap-0.32.0.jar` | `https://repo1.maven.org/maven2/io/arconia/arconia-core-bootstrap/0.32.0/arconia-core-bootstrap-0.32.0.jar` | 17,568 | `71a08356b213b4b7adf7130e3237e1bc8e76cafe9a34f14922224ee6e67ce464` |
| `arconia-observation-0.32.0.jar` | `https://repo1.maven.org/maven2/io/arconia/arconia-observation/0.32.0/arconia-observation-0.32.0.jar` | 6,479 | `53120c63e712c157483b4449fda002611a2f888beb9cf7689c96e3a10ecf8007` |
| `arconia-opentelemetry-exporter-sender-http-0.32.0.jar` | `https://repo1.maven.org/maven2/io/arconia/arconia-opentelemetry-exporter-sender-http/0.32.0/arconia-opentelemetry-exporter-sender-http-0.32.0.jar` | 14,709 | `c9dcc96c8fd5780c4691eb599a4b8e5ec9a74727404cbf398d7e8e57a1adfdf0` |
| `arconia-opentelemetry-micrometer-registry-otlp-0.32.0.jar` | `https://repo1.maven.org/maven2/io/arconia/arconia-opentelemetry-micrometer-registry-otlp/0.32.0/arconia-opentelemetry-micrometer-registry-otlp-0.32.0.jar` | 17,169 | `1ba97e77c5bca52f02eb699fcd91d5e04d9cc9874c930e1f32f7db0f74cf13af` |
| `arconia-opentelemetry-logback-bridge-0.32.0.jar` | `https://repo1.maven.org/maven2/io/arconia/arconia-opentelemetry-logback-bridge/0.32.0/arconia-opentelemetry-logback-bridge-0.32.0.jar` | 13,572 | `64a38d4b424810795fa925736cbe39261543d3d7ffabe4f6a482b22d19288626` |
| `arconia-opentelemetry-spring-boot-starter.maven-metadata.xml` | `https://repo1.maven.org/maven2/io/arconia/arconia-opentelemetry-spring-boot-starter/maven-metadata.xml` | 1,819 | `c9f44fdc3ad046e79411e66276469885e6bf7cacb3326d097c3771e9346606f9` |
| `arconia-openinference-ai-semantic-conventions.maven-metadata.xml` | `https://repo1.maven.org/maven2/io/arconia/arconia-openinference-ai-semantic-conventions/maven-metadata.xml` | 557 | `a788eab95fd60b7fbe5f76ffa665d3e19276265dba296cdf12e53aa71b63825b` |
| `doc-bbc8b419.html` | `https://docs.arconia.io/arconia/latest/observability/semantic-conventions/openinference/` | 50,782 | `95c673a76a4d98a31edde4cf80dd0bb4f5c57bbba558f72eeed8497e245b3001` |
| `doc-e46456a6.html` | `https://docs.arconia.io/arconia/latest/opentelemetry/` | 80,274 | `859ba3f20a3a0d56e678b369e7fe8988b5d1eadc26ccc1333035485274310f39` |
| `gh-release-0.32.0.html` | `https://github.com/arconia-io/arconia/releases/tag/v0.32.0` | 208,150 | `11f043533efa849ea3321e1a312fdd099036d69276c34242e2c05762ba940c91` |
| `gh-releases.html` | `https://github.com/arconia-io/arconia/releases` | (not hashed) | (not hashed) |

Not kept: `https://docs.arconia.io/arconia/latest/observability/opentelemetry/` (HTTP 404, 8,166 bytes).

Evidence paths: `S3/runs/<run>/` (run logs, `recv/`, `http.jsonl`, `trace.strace`, `out/`, `h.log`), `S3/runs/q4-raw-events.txt`, `S3/projects/boot3/src/m8/BootRun3.java`, `S3/src/sk3/Program.cs`, `S3/runners/`, `S3/tools/rawevents.py`, `S3/arconia/`.
