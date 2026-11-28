# M8 spike: Java (Spring AI, LangChain4j, Quarkus LangChain4j, Koog, Arconia, Embabel)

Status: spike, not a plan. Nothing here changes the design or the decisions. Open questions are in § 9 and need the user or the plan. Clock: 2026-11-24 about 12:00 UTC. The 7-day cutoff is **2026-11-17 11:53 UTC**; a release published after it is not eligible.

Evidence root (J): `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m8/java`. Paths below are relative to J unless absolute. Each fact is tagged:

- **run**: observed by a command in this session, with the evidence path under J.
- **docs**: read from a published source (a jar's class files or sources, a pom, a GitHub file or release page), with the path or URL.

## 1. Scope and result

- Done: Spring AI (§ 5.1), LangChain4j through OpenInference Java and through its native Micrometer observation (§ 5.2), Quarkus LangChain4j (§ 5.3), Koog (§ 5.4).
- Docs only, not run: Arconia and Embabel (§ 5.5). The brief allowed running them only if time allowed; they are recorded from their own sources.
- Also answered for the plan: the build tool and its offline mode, the lockfile and verification options, and the dependency cache size per integration (§ 7). Every run, its evidence and the normalizer output are in § 8. Open questions are in § 9.
- Headline (facts): the design's Spring AI claim "content in events" is not what 2.0.2 emits (content goes to logs); OpenInference Java emits two llm spans per LangChain4j call unless one listener kind is left out; Koog's default batch exporter exported nothing in our run; Embabel needs Java 21, which sets the JDK line.

## 2. Versions (the 7-day rule)

| Item | Version pinned | Asset or package (URL) | Published (UTC) | sha256 / sha512 / integrity | Notes |
|---|---|---|---|---|---|
| Temurin JDK, LTS line 21 | `21.0.13+9` | `https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.13%2B9/OpenJDK21U-jdk_x64_linux_hotspot_21.0.13_9.tar.gz` (207,858,352 bytes), listed by `https://api.adoptium.net/v3/assets/feature_releases/21/ga` | 2026-10-22T08:58:06Z (GitHub release `jdk-21.0.13+9`, not a pre-release) | sha256 `d7f5d5cf865a2902b70ff549b97586a7d3f234e016f041ab447ea740c7d66d1c`: equal to the API checksum and to the release's `.sha256.txt` file | Extracted to `J/bin/jdk/jdk-21.0.13+9`. `java -version` → Temurin-21.0.13+9 (run). |
| Temurin, newest LTS-21 build (not used) | `21.0.13.1+2` | GitHub release `jdk-21.0.13.1+2` | 2026-11-18T15:41:19Z | not downloaded | Too new (6 days). |
| Temurin, the other LTS lines (not used) | 25: `25.0.5+8` (2026-10-22T08:18:33Z, eligible); 17: `17.0.21+9` (2026-10-22T17:28:09Z, eligible) | api.adoptium.net, `J/dl/temurin25-assets.json`, `J/dl/temurin17-assets.json` | | | Line choice in § 2.1. |
| Apache Maven | `3.9.16` | `https://repo1.maven.org/maven2/org/apache/maven/apache-maven/3.9.16/apache-maven-3.9.16-bin.tar.gz` (9,278,065 bytes) | 2026-05-13T21:39:02Z (Maven Central pom Last-Modified) | sha512 `831a8591fe20c8243b1dbe7d71e3244f31d1665b0804b2e825e38cbbe5ce0cafb8338851f90780735568773e0a6cd07bbec107cda0b896b008b861075358b6f6`: equal to repo1's `.sha512` file; the `.asc` signature is on repo1 but was not verified | Extracted to `J/bin/apache-maven-3.9.16`. `mvn -v` → 3.9.16 (run). Newer `4.0.0-rc-7` is a pre-release. |
| Spring AI BOM, `spring-ai-openai`, `spring-ai-client-chat` | `2.0.2` | `repo1.maven.org/maven2/org/springframework/ai/…/2.0.2/` | GitHub release `v2.0.2` 2026-10-23T14:56:46Z; Maven pom 2026-10-22T12:46:15Z | sources jars in `J/dl/jars/` | `v2.0.3` and `v2.1.0` are 2026-11-20 (too new). |
| openai-java core and OkHttp client | `4.80.0` | `com.openai:openai-java-core`, `com.openai:openai-java-client-okhttp` | 2026-10-10T00:51:41Z | in the Maven cache | Pinned by `spring-ai-openai`'s pom (core); the OkHttp client is added by the program. `4.111.0`/`4.112.0` are too new. |
| Micrometer tracing bridge OTel | `1.7.2` | `io.micrometer:micrometer-tracing-bridge-otel` (pulls `micrometer-tracing`) | 2026-10-22T07:26:56Z | `J/dl/jars/micrometer-tracing-bridge-otel-1.7.2-sources.jar` | `1.7.3` and `1.8.0` are 2026-11-19 (too new). |
| OpenTelemetry BOM, SDK, OTLP exporter, SDK autoconfigure | `1.68.0` | `io.opentelemetry:opentelemetry-bom`, `-sdk`, `-exporter-otlp`, `-sdk-extension-autoconfigure` | 2026-11-06T15:36:11Z | in the Maven cache | `1.67.0` also eligible. |
| jackson-annotations | `2.21` (explicit pin) | `com.fasterxml.jackson.core:jackson-annotations` | 2026-01-18T22:47:14Z | in the Maven cache | Needed: see § 5.1 (run `spring-ai-1`). Chosen as the version the jackson-databind 3.1.7 javadoc links to (`J/dl/jackson-databind-3.1.7.pom`, line 221); the property value itself was not read. |
| slf4j-simple | `1.7.36` (runtime only) | `org.slf4j:slf4j-simple` | Maven, 2022 | in the Maven cache | Added only so that Spring AI's INFO logs print (without a binding SLF4J is a no-op). Matches the `slf4j-api` 1.7.36 on the classpath. |
| maven-compiler-plugin | `3.16.0` | `org.apache.maven.plugins` | 2026-08-30T16:10:32Z | in the Maven cache | Pinned in the pom. |
| maven-dependency-plugin | `3.11.0` | `org.apache.maven.plugins` | 2026-05-24T17:19:23Z | in the Maven cache | Pinned in the pom (`build-classpath`). Other lifecycle plugins: Maven 3.9.16's defaults; the versions Maven resolved are in `J/logs/spring-ai-compile.log`. |
| LangChain4j core, open-ai, observation | `1.23.0` | `dev.langchain4j:langchain4j-core`, `langchain4j-open-ai`, `langchain4j-observation` (1.23.0-beta33) | 2026-10-28T15:38:46Z (Maven); GitHub release `1.23.0` 2026-10-28T16:54:13Z | `J/dl/jars/langchain4j-core-1.23.0.jar`, `langchain4j-open-ai-1.23.0.jar` | `1.24.0` is 2026-11-18 (too new). `1.19.6` and `1.11.13` are 2026-11-17 16:56/16:58 UTC, after the cutoff. |
| OpenInference Java, LangChain4j | `0.1.9` | `com.arize:openinference-instrumentation-langchain4j` | 2026-09-25T17:02:16Z (Maven) | `J/dl/jars/openinference-instrumentation-langchain4j-0.1.9.jar` | Eligible; tag not checked on GitHub. |
| Koog (JetBrains) | `1.3.0` | `ai.koog:koog-agents`, `ai.koog:agents-core-jvm`, `ai.koog:agents-features-opentelemetry` | GitHub release `1.3.0` 2026-09-24T09:02:09Z; Maven 2026-09-23 | `J/dl/jars/agents-core-jvm-1.3.0.jar` | Eligible. |
| Quarkus platform | `3.40.1.2` (GitHub release, not a pre-release) | `io.quarkus.platform:quarkus-bom`, `io.quarkus:quarkus-core` 3.40.1 | 2026-11-09T16:17:21Z (GitHub `3.40.1.2`) | `J/dl/jars/quarkus-core-3.40.1.jar` | `4.0.0.CR1` (2026-11-11) is a pre-release and was not used. |
| Quarkus LangChain4j | `1.15.1` (GitHub release, not a pre-release) | `io.quarkiverse.langchain4j:quarkus-langchain4j-core` | 2026-11-12T06:31:33Z (GitHub); Maven 2026-11-12T06:55:11Z | `J/dl/jars/quarkus-langchain4j-core-1.15.1.jar` | `1.15.0` 2026-11-07; `1.13.5` and `1.7.10` are 2026-11-19 (too new). `2.0.0.*` are pre-releases. |
| Embabel | `1.5.4` (GitHub release) | `com.embabel.agent:embabel-agent-api` | 2026-10-28T07:35:07Z (GitHub `v1.5.4`) | `J/dl/jars/embabel-agent-api-1.5.4.jar` | `v1.5.3` 2026-10-05. Read only (step 5). |
| Arconia | `0.31.0` (Maven); GitHub `v0.32.0` 2026-11-03 | `io.arconia:arconia-core` | Maven 2026-10-14T22:40:59Z | `J/dl/jars/arconia-core-0.31.0.jar` | `v0.32.0` is eligible on GitHub but its Maven artifact was not found in `io/arconia/arconia-core` metadata at the time of the check. Read only (step 5). |

| Kotlin Maven plugin and serialization plugin | `2.3.21` | `org.jetbrains.kotlin:kotlin-maven-plugin`, `kotlin-maven-serialization`, `kotlin-bom` (import) | 2026-04-22T09:31:42Z | in the Maven cache | Koog's pom pins Kotlin 2.3.10 (`J/dl/koog-agents-1.3.0.pom`); 2.3.21 is the newest eligible, and the stdlib is managed by the BOM. `2.3.20` (2026-03-16) also eligible. |
| OpenTelemetry BOM 1.61.0 (Koog project) | `1.61.0` | `io.opentelemetry:opentelemetry-bom` | 2026-04-10T21:39:52Z | in the Maven cache | Koog's feature was built against this (`J/dl/koog-otel-jvm-1.3.0.pom`). Other projects use 1.68.0. |
| OpenTelemetry alpha BOM | `1.68.0-alpha` | `io.opentelemetry:opentelemetry-bom-alpha` (import) | 2026-11-06T15:36:11Z | in the Maven cache | Manages `opentelemetry-api-incubator`, which OpenInference's dependencies need (§ 5.2). |
| slf4j-simple (runtime, spike only) | `2.0.20` (LangChain4j project) | `org.slf4j:slf4j-simple` | 2026-09-22T14:50:55Z | in the Maven cache | Same role as `1.7.36` above; the slf4j-api on this classpath is 2.0.18. |
| Quarkus Maven plugin | `3.40.1.2` | `io.quarkus.platform:quarkus-maven-plugin` | 2026-11-09T11:40:07Z (Maven); GitHub `3.40.1.2` 2026-11-09T16:17:21Z | in the Maven cache | Build goal `build`; produces `target/quarkus-app/quarkus-run.jar`. |
| maven-compiler-plugin (Quarkus project) | `3.15.0` | `org.apache.maven.plugins` | not pinned in the pom | in the Maven cache | The compile step resolved this version (`J/logs/qlc4j-build2.log`); pin it if the capture needs the same plugin. |
| Quarkus platform BOM | `3.40.1.2` | `io.quarkus.platform:quarkus-bom` (import) | GitHub 2026-11-09T16:17:21Z | `J/dl/quarkus-bom-3.40.1.2.pom` | The BOM does not list `quarkus-langchain4j`; it is pinned separately. |
| OpenInference's transitive pins | `openinference-instrumentation` 0.1.10, `opentelemetry-instrumentation-api-incubator` 2.14.0-alpha, `jackson-databind` 2.17.0 (from `J/dl/oi-l4j-0.1.9.pom`) | Maven, transitive | not checked | in the Maven cache | Resolved by Maven nearest-wins; not checked for eligibility (they come from OpenInference's own pom). |

### 2.1 Why JDK 21

The rule "the frameworks' minimum Java version decides" (asked by the brief). Class-file major versions read from the pinned jars with `J/tools/classver.py` (run):

| Artifact | Major | Java |
|---|---|---|
| spring-ai-openai 2.0.2, spring-ai-model 2.0.2 | 61 | 17 |
| langchain4j-core 1.23.0, langchain4j-open-ai 1.23.0 | 61 | 17 |
| openinference-instrumentation-langchain4j 0.1.9 | 61 | 17 |
| agents-core-jvm (Koog) 1.3.0 | 61 | 17 |
| quarkus-core 3.40.1 | 61 | 17 |
| quarkus-langchain4j-core 1.15.1 | 61 | 17 |
| embabel-agent-api 1.5.4 | **65** | **21** |
| arconia-core 0.31.0 (6 classes only) | **65** | **21** |

Embabel needs 21, so 21 is the lowest LTS that satisfies every framework in scope. Java 25 would also run them but no framework here needs it. Java 17 would not run Embabel (or Arconia). Embabel and Arconia were read only (§ 5.5), but the line is fixed now so the runners do not change later. Caveat: the class-file check covers the jars' own classes; it does not prove no other part needs a newer JVM.

### 2.2 Downloads and integrity

- Every download went to `J/dl/` or to the Maven cache `J/cache/m2`. The Maven cache is `-Dmaven.repo.local=J/cache/m2`, and `HOME` is `J/home` for Maven runs.
- Maven Central dates are the `.pom` `Last-Modified` header (a close proxy for publication). GitHub dates are the release `published_at`. The rule's cutoff is applied to the date the source gives.
- The settings file `J/settings.xml` mirrors only `https://repo1.maven.org/maven2`.

## 3. Method

- Each run executes inside the loopback-only namespace via `m7/tools/netrun.sh` (`/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m7/tools/netrun.sh`), using the m7 fake provider and OTLP sink (`m7/tools/server.py`). Read in full: `netrun.sh`, `inner.sh`, `server.py` (chat routes; tool calls: the first chat call answers a tool call, the next answers text; `FAKE_TOOL` picks the tool). Fake key `sk-fake-m8`. The fake's final text is the m7 fixture text ("hello from the M7 spike.").
- Build (Maven, dependency download) runs outside the namespace with `mvn -s J/settings.xml -Dmaven.repo.local=J/cache/m2 compile dependency:build-classpath`. Running is `java -cp <classes>:<cp.txt>` inside the namespace (the classpath uses absolute paths under J).
- Normalization: `m7/tools/h.py` with the m7 venv (`m7/tools/.venv/bin/python`) and `m7/norm-target/release/norm` (cetra's real normalizer, built earlier from the worktree's `crates/otel`; not rebuilt). `h.py` writes `<run>/out/<name>.dump.txt` (raw spans) and `.norm.jsonl`.
- No cargo build; nothing built in the repo. One Maven build at a time.
- The "real home" guard in `netrun.sh` reports one path under the worktree in every run: a directory `openat` at the start of the trace (`trace.strace` line 46), which is the calling shell's working directory. No file under the real home was read or written by the program (run: `J/runs/spring-ai-3/real-home.txt` lists only that directory, and the program ran from `J/runs/…`).

## 4. Run index (Spring AI; all runs are in § 8)

| Run (`J/runs/`) | Program and runner | What it tests | Result |
|---|---|---|---|
| `spring-ai-1` | `SpringAiRun` (first version, `OpenAiChatModel.call` only), `runners/spring-ai.sh` | first wiring, jackson-annotations 2.18.9 | `NoClassDefFoundError: com/fasterxml/jackson/annotation/JsonSerializeAs` before any call |
| `spring-ai-2` | same program with `jackson-annotations` 2.21 | model call and OTLP route | One chat POST; answer empty (no tool execution, § 5.1 finding 2). Span export to `localhost:4318` failed (`OtlpHttpSpanExporter.getDefault()` does not read `OTEL_EXPORTER_OTLP_*`). |
| `spring-ai-3` | `ChatClient` + `AutoConfiguredOpenTelemetrySdk` (final program) | the tool round and the OTLP env route | 2 chat POSTs, 1 traces and 1 metrics POST, answer correct, 7 spans |
| `spring-ai-4-content` | as run 3, plus the four Spring AI content handlers registered (`M8_CONTENT_HANDLERS=1`), slf4j-simple added | content switch | Content printed to the log at INFO; spans unchanged (0 events, 0 content attributes) |

Evidence: `J/runs/<run>/run.log`, `J/runs/<run>/http.jsonl`, `J/runs/<run>/req/req-00N.json` (request bodies), `J/runs/<run>/out/<run>.dump.txt` and `.norm.jsonl`, `J/logs/*.h.txt`.

## 5. Findings per integration

### 5.1 Spring AI 2.0.2 (Micrometer observations exported as OTLP spans)

**Program** (`J/projects/spring-ai/src/main/java/m8/SpringAiRun.java`, Java 21, plain Java, no Spring Boot). The key lines:

```java
OpenTelemetrySdk otel = AutoConfiguredOpenTelemetrySdk.initialize().getOpenTelemetrySdk();
ObservationRegistry registry = ObservationRegistry.create();
registry.observationConfig().observationHandler(
        new DefaultTracingObservationHandler(new OtelTracer(tracer, new OtelCurrentTraceContext(), event -> { })));
OpenAIClient client = OpenAIOkHttpClient.builder().baseUrl(base + "/v1").apiKey(System.getenv("OPENAI_API_KEY")).build();
OpenAiChatModel model = OpenAiChatModel.builder().openAiClient(client).observationRegistry(registry).build();
ToolCallback weather = FunctionToolCallback.builder("get_weather", (WeatherRequest r) -> "Sunny in " + r.city() + ", 21 C")
        .description("Get the current weather for a city").inputType(WeatherRequest.class).build();
ChatClient chat = ChatClient.builder(model, registry, null, null).defaultToolCallbacks(weather).build();
String answer = chat.prompt().options(OpenAiChatOptions.builder().model("gpt-4o-mini"))
        .user("What is the weather in Lyon? Use the tool.").call().content();
```

Runner: `J/runners/spring-ai.sh` (env: `OTEL_SERVICE_NAME=m8-spring-ai`, `OTEL_EXPORTER_OTLP_ENDPOINT=$BASE`, `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`, `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer sk-fake-m8`, `FAKE_TOOL=get_weather` in the run's `env` file). Classpath: `J/projects/spring-ai/cp.txt`.

**Version and build (run and docs).** Spring AI 2.0.2 resolves with `spring-ai-openai` 2.0.2 and `openai-java-core` 4.80.0 (both pins in the pom). The sources of `spring-ai-openai` and `spring-ai-model` (`J/src/spring-ai/`) say what the 2.0 API is:

- `OpenAiChatModel.Builder.toolCallingManager(...)` is `@Deprecated(since = "2.0.0", forRemoval = true)`: "internal tool execution in OpenAiChatModel is superseded by ToolCallingAdvisor used via ChatClient" (`openai/.../OpenAiChatModel.java`, the Builder's javadoc). `OpenAiChatModel.call` therefore returns the model's tool-call message and does not run the tool (run `spring-ai-2`: one POST, empty answer).
- Tool execution is `ChatClient` with the default `ToolCallingAdvisor` (`client/.../ChatClient.java`, the builder javadoc: a default `ToolCallingAdvisor` is created with a `ToolCallingManager` backed by the registry when none is given).
- `ChatClient.builder(chatModel)` uses `ObservationRegistry.NOOP`; the registry must be passed to the 4-argument builder (`ChatClient.java`, `builder(ChatModel)` body).

**Dependencies (run).** Spring AI 2.0.2 uses Jackson 3 (`tools.jackson.databind` 3.1.7) and needs `com.fasterxml.jackson.annotation.JsonSerializeAs`. The Maven resolution kept `jackson-annotations` 2.18.9 (the Jackson 2.18.9 jars on the classpath), which lacks it: `NoClassDefFoundError` at `FunctionToolCallback.<clinit>` (run `spring-ai-1`, `J/runs/spring-ai-1/run.log`). Pinning 2.21 fixed it (run `spring-ai-2`).

**OTLP export (run and docs).**

- The program uses `AutoConfiguredOpenTelemetrySdk.initialize()`. The env route then applies: `OTEL_EXPORTER_OTLP_ENDPOINT` (base URL; the SDK appends `/v1/traces` and `/v1/metrics`), `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`, `OTEL_EXPORTER_OTLP_HEADERS`.
- `OtlpHttpSpanExporter.getDefault()` (run `spring-ai-2`) ignores the env variables and posts to `localhost:4318`. The env-driven route is the autoconfigure module (run `spring-ai-3`).
- Protocol: `http/protobuf` (run: `J/runs/spring-ai-3/recv/0001-traces.bin`, a protobuf body).
- Header form that reached the sink: `Authorization: Bearer sk-fake...` (17 characters, decoded) (run `spring-ai-3/http.jsonl`, `otlp` events).
- Signals: traces and metrics are both posted (`/v1/traces` and `/v1/metrics`, run `spring-ai-3`). No logs were emitted (no log exporter output). Metrics are on by default in the SDK; they are not spans.
- Logs of the program itself are separate (SLF4J; § 5.1 finding 6).

**Spans received (run `spring-ai-3`, `J/runs/spring-ai-3/out/spring-ai-3.dump.txt`).** Seven spans, all in scope `m8-spring-ai`, one trace:

| # | Name (raw) | Kind | Parent | Keys (raw) | cetra's normalizer |
|---|---|---|---|---|---|
| 0 | `spring_ai chat_client` | unset (the dump shows `kind=None`) | root | `gen_ai.operation.name=framework`, `gen_ai.system=spring_ai`, `spring.ai.kind=chat_client`, `spring.ai.chat.client.advisors`, `spring.ai.chat.client.stream`, `spring.ai.chat.client.tool.names` | kind `span`, operation `framework`, no provider; attributes left |
| 1 | `tool _calling ` (raw: a space before `_calling` and a trailing space) | unset | #0 | `gen_ai.operation.name=framework`, `gen_ai.system=spring_ai`, `spring.ai.kind=advisor`, `spring.ai.advisor.name=Tool Calling Advisor`, `spring.ai.advisor.order` | kind `span`, operation `framework` |
| 2 | `call` | unset | #1 | same shape, `spring.ai.advisor.name=call` | kind `span`, `framework` |
| 3 | `chat gpt-4o-mini` | unset | #2 | `gen_ai.operation.name=chat`, `gen_ai.system=openai`, `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.response.id`, `gen_ai.response.finish_reasons=["TOOL_CALLS"]`, `gen_ai.usage.input_tokens=82`, `gen_ai.usage.output_tokens=17`, `gen_ai.usage.total_tokens=99`, `spring.ai.model.request.tool.names` | llm; provider `openai`; model `gpt-4o-mini`; tokens 82/17/99; cost computed (2.25e-05); **no I/O** |
| 4 | `execute_tool get_weather` | unset | #1 | `gen_ai.operation.name=execute_tool`, `gen_ai.system=spring_ai`, `spring.ai.kind=tool_call`, `spring.ai.tool.call.id=call_m7_1`, `spring.ai.tool.definition.name=get_weather`, `spring.ai.tool.definition.description`, `spring.ai.tool.definition.schema` (225 bytes), `spring.ai.tool.type=function` | kind `tool`, operation `execute_tool`; **no tool name** in the normalized row (`tool_name` absent; the name is in `spring.ai.tool.definition.name`, not `gen_ai.tool.name`), **no arguments, no result** |
| 5 | `call` | unset | #1 | as #2 | kind `span`, `framework` |
| 6 | `chat gpt-4o-mini` | unset | #5 | as #3, tokens 131/12/143, `finish_reasons=["STOP"]` | llm; provider `openai`; tokens 131/12; **no I/O** |

Note the parent of #3 is the advisor span (#2), not the chat client span. Two llm spans, one per model call (no duplicate). The tool call did not create a second llm span.

**Attribute facts.**

- `gen_ai.system` is `spring_ai` on the framework, advisor and tool spans and `openai` on the llm spans. That matches cetra's `FRAMEWORK_NAMES` (which includes `spring_ai`, `crates/otel/src/context.rs:52`): the normalizer skips a framework value as provider (`context.rs:338-352`), so the provider `openai` comes out right.
- The design's text says "`gen_ai.system=spring_ai`, content in events". **Not confirmed (run, § 5.1 finding 4).** No span has an event, and no span has a prompt or completion attribute.
- `gen_ai.response.id` is the fake's `chatcmpl-m7`. Every span has `kind` unset (`kind=None` in the dump), so cetra's kind comes from the attributes, not from the OTel span kind.

**Content capture switch (run `spring-ai-4-content`, docs).**

- Spring AI 2.0.2 has no span-content switch in the plain-Java wiring. Its content path is four observation handlers that **log** at INFO: `ChatModelPromptContentObservationHandler` and `ChatModelCompletionObservationHandler` (`J/src/spring-ai/model/org/springframework/ai/chat/observation/`), and `ChatClientPromptContentObservationHandler` and `ChatClientCompletionObservationHandler` (`J/src/spring-ai/client/org/springframework/ai/chat/client/observation/`). Their `onStop` calls `logger.info("Chat Model Prompt Content:\n" + …)`.
- The program registers them only when `M8_CONTENT_HANDLERS` is set. Run `spring-ai-4-content` logged the prompt and completion text (`J/runs/spring-ai-4-content/run.log`: `["What is the weather in Lyon? Use the tool."]` and the answer), and the span dump has 0 events and no content attributes (`grep -c -i 'prompt|completion|content'` on the dump: 0).
- Default: the handlers are not registered in plain Java (docs: the source has no default registration; the Spring Boot starter's property for them was not read).

**Findings, short.**

1. The tool round needs `ChatClient` with a tool callback. `OpenAiChatModel.call` alone does not execute tools in 2.0.2 (run `spring-ai-2`; deprecation in § 5.1 above).
2. The framework spans are noise for cetra: a root `chat_client` span and two `call` advisor spans and a `Tool Calling Advisor` span (4 of 7 spans). Keys to discard on: `spring.ai.kind` ∈ {`chat_client`, `advisor`}, or `gen_ai.operation.name=framework`.
3. Tool spans lack `gen_ai.tool.name`, arguments and results. The tool name is `spring.ai.tool.definition.name`.
4. No span events and no content in spans, so the design's "content in events" needs a change or a user decision (§ 9).
5. `gen_ai.system` = `spring_ai` on noise spans; the framework table already covers it.
6. The program's own log: Spring AI logs through commons-logging → SLF4J; without a binding the logs are dropped (NOP). The slf4j-simple dependency is for this spike only.

**Normalizer: what a rule would need (facts only).**

- Framework/advisor/chat_client spans (`spring.ai.kind`): discard, or keep as `span`; with `gen_ai.operation.name=framework` they currently survive as generic rows.
- Tool name: read `spring.ai.tool.definition.name` (or `spring.ai.tool.call.id` for the call).
- Tool I/O: nothing to read in 2.0.2 without the content handlers (which log instead).
- Token double count: none in this run (llm spans are one per call).
### 5.2 LangChain4j 1.23.0 through OpenInference Java, and its native observation

**Does LangChain4j have its own OTel route? (docs).** No OpenTelemetry module: `dev.langchain4j:langchain4j-open-telemetry` is HTTP 404 on repo1.maven.org (run). The repo's modules (`gh api repos/langchain4j/langchain4j/contents`, docs) include `langchain4j-observation` and `langchain4j-micrometer-metrics`. The native route is therefore **Micrometer observations** (`langchain4j-observation`, `ObservationChatModelListener`), bridged to OTel like Spring AI's. The native listener needs a model-level listener only; AiService and tool spans are not produced by it (run `native`).

**Program** (`J/projects/langchain4j/src/main/java/m8/LangChain4jRun.java`). One `AiServices` interface with one `@Tool`, a model call, a tool call and a second model call. The mode is the first argument: `oi` (both OpenInference listener kinds), `oi-model`, `oi-service`, `native`. Key lines:

```java
LangChain4jInstrumentor instrumentor = LangChain4jInstrumentor.instrument(otel.getTracerProvider());
modelListeners = mode.equals("oi-service") ? List.of() : List.of(instrumentor.createModelListener());
serviceListeners = mode.equals("oi-model") ? List.of() : instrumentor.createAiServiceListeners();
// native:  new ObservationChatModelListener(registry, new SimpleMeterRegistry())
OpenAiChatModel model = OpenAiChatModel.builder().baseUrl(base + "/v1").apiKey(...).modelName("gpt-4o-mini").listeners(modelListeners).build();
Assistant assistant = AiServices.builder(Assistant.class).chatModel(model).tools(new WeatherTools()).registerListeners(serviceListeners).build();
```

Runner: `J/runners/lc4j.sh <mode>` (OTel env route as Spring AI; `OTEL_SERVICE_NAME=m8-langchain4j`).

**Versions and build (run).** `langchain4j` 1.23.0 (the AiServices module), `langchain4j-open-ai` 1.23.0, `langchain4j-observation` 1.23.0-beta33 (the 1.23 line's observation module), `openinference-instrumentation-langchain4j` 0.1.9, `micrometer-tracing-bridge-otel` 1.7.2. Resolved OTel: `opentelemetry-api-incubator` 1.68.0-alpha (the alpha BOM, `opentelemetry-bom-alpha` 1.68.0-alpha, was needed: OpenInference's pom brings `opentelemetry-instrumentation-api-incubator` 2.14.0-alpha, which needs the api-incubator classes; without the alpha BOM, `NoClassDefFoundError: io/opentelemetry/api/incubator/metrics/BoundDoubleGauge` on the first run, `J/runs/lc4j-oi-1`, `lc4j-native-1`). slf4j-api 2.0.18 (so slf4j-simple 2.0.20 for the log; 1.7.36 was ignored, `J/runs/lc4j-native-1/run.log`).

**OpenInference API (javap, run).** `LangChain4jInstrumentor.instrument(TracerProvider)` logs "Instrumenting LangChain4j with OpenInference" and "Registered LangChain4j interceptors". Its `registerInterceptors()` only creates the model listener (`javap -c`); nothing is registered globally. The caller must attach `createModelListener()` to the chat model and `createAiServiceListeners()` to `AiServices`. The run shows the two kinds produce different spans (below).

**Span inventory (runs `lc4j-oi-2`, `lc4j-oi-model-1`, `lc4j-oi-service-1`, `lc4j-native-2`; dumps under `J/runs/<run>/out/`).**

| Mode | Span (raw name) | Kind (openinference.span.kind) | Parent | Notes |
|---|---|---|---|---|
| `oi` and `oi-service` | `AiService.chat` | AGENT | root | input and output as messages (`input.value`, `output.value`, `input.mime_type`) |
| `oi` and `oi-service` | `LLM` (x2) | LLM | `AiService.chat` | `llm.model_name=unknown`, `llm.provider=openai`, `llm.system=langchain4j`, tokens (`llm.token_count.prompt/completion/total`), messages in `llm.input_messages.N.*` and `llm.output_messages.N.*`, tool calls in `llm.output_messages.0.message.tool_calls.0.*`, `llm.finish_reason` (`TOOL_EXECUTION`, then `STOP`) |
| `oi` and `oi-model` | `generate` (x2) | LLM | **none (its own trace per call)** | real `llm.model_name=gpt-4o-mini`, same tokens, same messages |
| `oi` and `oi-service` | `AiService.tool` | TOOL | `AiService.chat` | `tool.name=get_weather`, `tool_call.id`, `tool.parameters`, `input.value`, `output.value` |

- **Duplicate llm spans (run `lc4j-oi-2`).** In mode `oi` each model call makes two LLM spans with the same tokens: `LLM` (child, model `unknown`) and `generate` (root, its own trace, model `gpt-4o-mini`). The two come from the two listener kinds (`oi-model` gives only `generate`, `oi-service` only `LLM`). Counting llm spans gives 4 for 2 calls; token sums double.
- `llm.token_count.*` tokens on both; the same `82/17` and `131/12`.

**Native `langchain4j-observation` (run `lc4j-native-2`, `J/runs/lc4j-native-2/out/lc4j-native-2.dump.txt`).** Two spans, both roots (no parent), one trace per call, kind unset, scope `m8-langchain4j`:

- `chat gpt-4o-mini`: `gen_ai.operation.name=chat`, `gen_ai.provider.name=OPEN_AI` (an enum name, not `openai`), `gen_ai.request.model=gpt-4o-mini`, `gen_ai.response.model=gpt-4o-mini`, `input_tokens` and `output_tokens` (**not** `gen_ai.usage.*`), `outcome=SUCCESS`. Tokens 82/17 and 131/12.
- No AiService span, no tool span, no messages or content (no prompt or completion attribute on either span).
- No duplicates.

**What cetra's normalizer makes of them (run, `J/logs/lc4j-*-2.h.txt`).**

- `AiService.chat` → kind `agent`, input and output previews read (`fmt=json` / `text`).
- `LLM` (child) → kind `llm`, provider `openai`, model `unknown` (kept as request_model `"unknown"`), tokens 82/17, cost source `none`, input preview read.
- `generate` → kind `llm`, provider `openai`, model `gpt-4o-mini`, tokens 82/17, cost computed (2.25e-05).
- `AiService.tool` → kind `tool`, input and output previews read, **no tool name** (`tool.name` stays in attributes_left).
- Native `chat gpt-4o-mini` → kind `llm`, provider `open_ai` (not canonical `openai`), model `gpt-4o-mini`, **tokens missing** (`input_tokens` and `output_tokens` stay in attributes_left), cost source `none`.

**What a normalizer rule would need (facts only).**

1. OpenInference: an LLM span with no parent and no tool/agent parent duplicates the child `LLM` span; one of the two must be dropped, or the rule must pick the one with a real model name. Dedupe key candidate: the same `llm.token_count.*` and `llm.input_messages` on a span whose parent is an AGENT (run `oi`).
2. OpenInference tool spans: read `tool.name` (run: not read today).
3. Native LangChain4j: read `input_tokens` and `output_tokens`; canonicalize `gen_ai.provider.name` `OPEN_AI` to `openai`; `outcome=SUCCESS` is not an error signal to keep or drop, facts only.

### 5.3 Quarkus LangChain4j (native OTel: `quarkus-opentelemetry`)

**Versions and build (run).** Quarkus platform `3.40.1.2` (`io.quarkus.platform:quarkus-bom`, `quarkus-maven-plugin` 3.40.1.2), `io.quarkiverse.langchain4j:quarkus-langchain4j-openai` 1.15.1, `quarkus-opentelemetry` and `quarkus-arc` from the platform BOM. The compile step ran `maven-compiler-plugin` 3.15.0, which the pom does not pin (`J/logs/qlc4j-build2.log`). The build downloaded 1,782 artifact lines (`grep -c` on `J/logs/qlc4j-build1.log`). The jar: `J/projects/quarkus-lc4j/target/quarkus-app/quarkus-run.jar`.

**Program** (`J/projects/quarkus-lc4j/src/main/java/m8/`): `Assistant` (a `@RegisterAiService(tools = WeatherTools.class)` interface), `WeatherTools` (`@ApplicationScoped`, `@Tool`), and `QuarkusRun` (`@QuarkusMain`; the call runs in `run()`).

```java
@QuarkusMain
public class QuarkusRun implements QuarkusApplication {
    @Inject Assistant assistant;
    @Override
    @ActivateRequestContext   // jakarta.enterprise.context.control.ActivateRequestContext
    public int run(String... args) {
        System.out.println("answer: " + assistant.chat("What is the weather in Lyon? Use the tool."));
        return 0;
    }
}
```

Config: `application.properties` sets the key, model name, `quarkus.otel.exporter.otlp.traces.protocol=http/protobuf`; the runner sets `QUARKUS_LANGCHAIN4J_OPENAI_BASE_URL=$BASE/v1`, `QUARKUS_OTEL_EXPORTER_OTLP_ENDPOINT=$BASE`, `QUARKUS_OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer sk-fake-m8` (`J/runners/qlc4j.sh`, run as `java -jar`).

**Findings (run).**

- Startup call without a request context fails: `ContextNotActiveException: RequestScoped context was not active when trying to obtain a bean instance for a client` (`J/runs/qlc4j-1/run.log`). `@ActivateRequestContext` on `run()` fixes it (run `qlc4j-3`).
- OTLP: one protobuf POST to `/v1/traces` (`J/runs/qlc4j-3/http.jsonl`). No metrics POST: no `quarkus-micrometer`, so no `gen_ai.client.token.usage` metric is sent (docs: metrics need `quarkus-micrometer`, `J/docs: observability.adoc` line 9).
- Header form that reached the sink: `Authorization: Bearer sk-fake...` (17 characters, decoded).
- Calls: 2 POSTs to `/v1/chat/completions` (the fake).

**Spans received (run `qlc4j-3`, `J/runs/qlc4j-3/out/qlc4j-3.dump.txt`).** Six spans, scope `io.quarkus.opentelemetry`, one trace:

| # | Name (raw) | Parent | Keys (raw) | cetra's normalizer |
|---|---|---|---|---|
| 0 | `langchain4j.aiservices.Assistant.chat` | root | none (no attributes) | span, status unset |
| 1 | `completion gpt-4o-mini` | #0 | `gen_ai.operation.name=chat`, `gen_ai.provider.name=open_ai`, `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.response.id`, `gen_ai.response.finish_reasons=TOOL_EXECUTION`, `gen_ai.usage.input_tokens=82`, `gen_ai.usage.output_tokens=17`, `gen_ai.client.estimated_cost="0.00002250USD"`, `gen_ai.request.temperature`, `gen_ai.request.top_p` | llm; provider `open_ai`; tokens 82/17/99; **cost source `none`** (the estimated-cost string is not read) |
| 2 | `POST /chat/completions` | #1 | `http.request.method`, `http.response.status_code=200`, `url.full` (the fake's URL) | span (HTTP client, noise) |
| 3 | `langchain4j.tools.get_weather` | #0 | `gen_ai.operation.name=execute_tool`, `gen_ai.tool.name=get_weather`, `gen_ai.tool.call.id`, `gen_ai.tool.type=function` | tool, `execute_tool`, no I/O |
| 4 | `completion gpt-4o-mini` | #0 | as #1, tokens 131/12/143, finish `STOP` | llm; provider `open_ai`; tokens 131/12 |
| 5 | `POST /chat/completions` | #4 | as #2 | span |

No I/O on the llm spans and none on the tool span (content off).

**Content switch (run `qlc4j-content-4`, docs).** `quarkus.langchain4j.tracing.include-prompt`, `include-completion`, `include-tool-arguments`, `include-tool-result`; the docs say they are off by default (`J/dl/qlc4j-observability.adoc`, lines 279-282 and the "all off by default" note in the Opik section). With them set to `true` (env `QUARKUS_LANGCHAIN4J_TRACING_INCLUDE_*`), the spans gain `gen_ai.prompt` (on the llm spans, the tool-round prompt contains the tool result), `gen_ai.completion`, `gen_ai.tool.call.arguments` (`{"city": "hello.txt"}`) and `gen_ai.tool.call.result`. No span events (0 events in the dump).

**Normalizer on the content run.** The tool span takes input and output from `gen_ai.tool.call.arguments` and `gen_ai.tool.call.result` (previews read). The llm spans do **not** read `gen_ai.prompt` or `gen_ai.completion` (both stay in attributes_left, `J/logs/qlc4j-content-4.h.txt`). The estimated cost string is not read (cost source `none`, both runs).

**Design check.** The Quarkus LangChain4j route is native OTel (`quarkus-opentelemetry`). Its span names and attributes differ from OpenInference and from the Micrometer route; the same LLM call's tokens appear once here (no duplicate), unlike the OpenInference `oi` mode.


### 5.4 Koog 1.3.0 (JetBrains, Kotlin; native OTel feature)

**Versions and build (run and docs).** `ai.koog:koog-agents-jvm` 1.3.0, `ai.koog:agents-features-opentelemetry-jvm` 1.3.0 (the `-jvm` artifacts carry the classes; the un-suffixed `agents-features-opentelemetry` jar has 0 classes). Koog's own pom pins `kotlin-bom` 2.3.10 and `opentelemetry-bom` 1.61.0 (`J/dl/koog-agents-1.3.0.pom`, `J/dl/koog-otel-jvm-1.3.0.pom`). This project uses `kotlin-bom` and the `kotlin-maven-plugin` at **2.3.21** (published 2026-04-22; newest eligible, `J/logs`), `kotlin-maven-serialization` 2.3.21 (the compiler plugin, see finding 1), `opentelemetry-bom` **1.61.0** (matching the feature's pin; 1.61.0 published 2026-04-10). The Koog build resolves 144 jars (`J/projects/koog/cp.txt`). The Kotlin program is compiled by Maven (`J/projects/koog/pom.xml`), not Gradle.

**Program** (`J/projects/koog/src/main/kotlin/m8/Main.kt`). One `AIAgent` with one `SimpleTool`, the OpenAI client pointed at the fake, the OpenTelemetry feature:

```kotlin
val client = OpenAILLMClient(apiKey = "sk-fake-m8", settings = OpenAIClientSettings(baseUrl = base))
val agent = AIAgent(
    promptExecutor = MultiLLMPromptExecutor(client),
    llmModel = OpenAIModels.Chat.GPT4oMini,
    systemPrompt = "You are a helpful assistant.",
    toolRegistry = ToolRegistry { tool(WeatherTool()) },
) {
    install(OpenTelemetry) {
        setServiceInfo("m8-koog", "0.0.0")
        val exporter = OtlpHttpSpanExporter.builder().setEndpoint("$base/v1/traces").addHeader("Authorization", "Bearer sk-fake-m8").build()
        addSpanProcessor { simpleSpanProcessor(exporter.toOtelKotlinSpanExporter()) }
        setVerbose(verbose)   // the argument "verbose"
    }
}
println("answer: " + agent.run("What is the weather in Lyon? Use the tool."))
```

`WeatherTool` is `SimpleTool<WeatherArgs>` with `@Serializable data class WeatherArgs(val city: String)`, `execute(args)` (the abstract method is `execute`, `javap` on `ai.koog.agents.core.tools.SimpleTool`). Runner: `J/runners/koog.sh [verbose]`.

**Export route (run and docs).**

- No environment route. The Koog docs configure exporters in code only (`J/dl/koog-otel-index.md`: `install(OpenTelemetry) { addSpanExporter(...) }`); `OTEL_*` names do not appear in the docs page. The program therefore reads the endpoint from `M8_BASE_URL` (a runner variable), not from `OTEL_EXPORTER_OTLP_*`.
- `addSpanExporter` (the default) wraps the exporter in a **batch** processor. Run `koog-1` (that build, `addSpanExporter`, default verbose off): **0 OTLP posts** before the process exited, even with `agent.close()` (`J/runs/koog-1/http.jsonl`). The docs say the batch worker flushes spans (`koog-otel-index.md` line 205). Not investigated further.
- `addSpanProcessor { simpleSpanProcessor(exporter.toOtelKotlinSpanExporter()) }` (docs, `koog-otel-index.md` lines 209-221; the compat function is `io.opentelemetry.kotlin.tracing.export.toOtelKotlinSpanExporter`, the processor `io.opentelemetry.kotlin.tracing.export.simpleSpanProcessor`, `javap` on `compat-jvm-0.3.0.jar` and `exporters-core-jvm-0.3.0.jar`) exports **one POST per span** (11 OTLP POSTs for 11 spans, run `koog-6-default`).
- Protocol and header: `OtlpHttpSpanExporter` (`http/protobuf`, `Content-Type: application/x-protobuf`). Header received: `Authorization: Bearer sk-fake...` (17 characters, decoded). User agent `OTel-OTLP-Exporter-Java/1.61.0`.

**Content switch (run, docs).** `setVerbose(true)`. The docs say content is masked by default and `verbose` shows it (`koog-otel-index.md` lines 231-241, 431-432). Run `koog-6-default` (verbose off): `gen_ai.input.messages`, `gen_ai.output.messages`, `system_instructions`, `gen_ai.tool.definitions`, `gen_ai.tool.call.arguments`, `gen_ai.tool.call.result` all = `HIDDEN:non-empty`. Run `koog-5-verbose`: the same keys carry content, e.g. `gen_ai.tool.call.arguments = {"city":"hello.txt"}`, `gen_ai.tool.call.result = "Sunny in hello.txt, 21 C"`, `gen_ai.input.messages = [{"role":"System","parts":[{"type":"text","content":"You are a helpful assistant."}]},…]`. Content lands in **span attributes** (no span events).

**Spans received (run `koog-6-default`, `J/runs/koog-6-default/out/koog-6-default.dump.txt`).** Eleven spans, one trace, scope `m8-koog`:

| # | Name (raw) | Kind | Parent | Notes |
|---|---|---|---|---|
| 0 | `create_agent <session-uuid>` | agent | root | `gen_ai.operation.name=create_agent`, `gen_ai.agent.id`, `gen_ai.provider.name=openai`, `gen_ai.request.model=gpt-4o-mini`, `system_instructions` (masked) |
| 1 | `invoke_agent <uuid>` | agent | #0 | `gen_ai.usage.input_tokens=0`, `gen_ai.usage.output_tokens=0` (string values), `gen_ai.response.finish_reasons`, `gen_ai.conversation.id` |
| 2 | `strategy single_run` | span | #1 | `koog.strategy.name` |
| 3 | `node __start__` | span | #2 | `koog.node.id`, `koog.node.input`, `koog.node.output` |
| 4 | `node nodeCallLLM` | span | #2 | |
| 5 | `chat gpt-4o-mini` | llm | #4 | `gen_ai.usage.input_tokens=82`, `output_tokens=17` (string values), `gen_ai.response.finish_reasons=[tool_calls]`, `gen_ai.tool.definitions`, `gen_ai.input.messages`, `gen_ai.output.messages` |
| 6 | `node nodeExecuteTool` | span | #2 | |
| 7 | `execute_tool get_weather` | tool | #6 | `gen_ai.tool.name=get_weather`, `gen_ai.tool.call.id=call_m7_1`, `gen_ai.tool.call.arguments`, `gen_ai.tool.call.result`, `gen_ai.provider.name=koog` |
| 8 | `node nodeSendToolResult` | span | #2 | |
| 9 | `chat gpt-4o-mini` | llm | #8 | tokens 131/12; finish `[stop]` |
| 10 | `node __finish__` | span | #2 | |

Two model calls, two llm spans, no duplicate llm. Two agent-kind spans per run (`create_agent` and `invoke_agent`).

**What cetra's normalizer makes of them (run, `J/logs/koog-6-default.h.txt`, `koog-5-verbose.h.txt`).**

- Default (masked, `koog-6-default`): the agent and llm rows carry `input_preview = output_preview = "HIDDEN:non-empty"` (fmt text), and `cetra.normalize_error = "messages:genai:input,messages:genai:output"` on `invoke_agent` and both `chat` rows (the masked value is not a message array). The tool row has the masked previews too.
- Verbose (`koog-5-verbose`): **no normalize errors**; llm previews read properly (input `What is the weather in Lyon? Use the tool.`, `input_format=messages`); tool previews from `gen_ai.tool.call.arguments`/`result`.
- Tokens: read on the llm rows (82/17, 131/12, cost computed) even though Koog writes them as strings. The `invoke_agent` row has no tokens (0 on the span, not read as a sum).
- Tool row: kind `tool`, `provider="koog"` (the normalizer takes `gen_ai.provider.name=koog` as a provider, since `koog` is not in `FRAMEWORK_NAMES`, `crates/otel/src/context.rs:44-54`), `tool_name` present in the verbose run's attributes only through `gen_ai.tool.name` (not in attributes_left).
- Node and strategy spans: kind `span`, generic (6 rows), no discard rule.

**Findings.**

1. Missing `kotlinx.serialization` compiler plugin (my build error, found first): with `@Serializable` args and no serialization compiler plugin in `kotlin-maven-plugin`, the tool call failed inside Koog with `kotlinx.serialization.SerializationException`, `status=ERROR` on `execute_tool` (runs `koog-3`, `koog-4-verbose`), and the agent still answered (the fake does not check the result). Fixed by `kotlin-maven-serialization` 2.3.21 (run `koog-5-verbose`: result correct, no error). Plan note: any Koog program that declares `@Serializable` tool arguments needs that plugin in the Maven build.
2. Batch export exported nothing in the program's lifetime (`koog-1`), synchronous export worked (`koog-6-default`).
3. Two agent spans per run, six node/strategy spans per run: noise for cetra (a rule can drop `node …` and `strategy …` spans).
4. Masked content is the default and makes the normalizer report errors; cetra's capture of Koog's content depends on `setVerbose(true)`.

**Normalizer: what a rule would need (facts only).**

- Read `gen_ai.tool.call.arguments`/`result` as tool I/O (already done when content is on).
- Recognize `HIDDEN:non-empty` as "content withheld" (not a message array), so it does not raise `messages:genai:*` errors.
- Choose one agent row per run (`create_agent` vs `invoke_agent`).
- Do not count `gen_ai.usage.*` on `invoke_agent` (0 today; a sum over agent spans would double count if it were not 0).
- `koog` as a framework name for `gen_ai.provider.name` (provider `koog` is wrong).
tool row: kind `tool`, `provider="koog"` (the normalizer takes `gen_ai.provider.name=koog` as a provider, since `koog` is not in `FRAMEWORK_NAMES`, `crates/otel/src/context.rs:44-54`), and **no `tool_name`** in the normalized row (`gen_ai.tool.name` stays in attributes_left, as in the Quarkus and OpenInference tool rows).
- Node and strategy spans: kind `span`, generic (6 rows), no discard rule.

**Findings.**

1. Missing `kotlinx.serialization` compiler plugin (my build error, found first): with `@Serializable` args and no serialization compiler plugin in `kotlin-maven-plugin`, the tool call failed inside Koog with `kotlinx.serialization.SerializationException`, `status=ERROR` on `execute_tool` (runs `koog-3`, `koog-4-verbose`), and the agent still answered (the fake does not check the result). Fixed by `kotlin-maven-serialization` 2.3.21 (run `koog-5-verbose`: result correct, no error). Plan note: any Koog program that declares `@Serializable` tool arguments needs that plugin in the Maven build.
2. Batch export exported nothing in the program's lifetime (`koog-1`), synchronous export worked (`koog-6-default`).
3. Two agent spans per run, six node/strategy spans per run: noise for cetra (a rule can drop `node …` and `strategy …` spans).
4. Masked content is the default and makes the normalizer report errors; cetra's capture of Koog's content depends on `setVerbose(true)`.

**Normalizer: what a rule would need (facts only).**

- Read `gen_ai.tool.call.arguments`/`result` as tool I/O (already done when content is on).
- Recognize `HIDDEN:non-empty` as "content withheld" (not a message array), so it does not raise `messages:genai:*` errors.
- Choose one agent row per run (`create_agent` vs `invoke_agent`).
- Do not count `gen_ai.usage.*` on `invoke_agent` (0 today; a sum over agent spans would double count if it were not 0).
- `koog` as a framework name for `gen_ai.provider.name` (provider `koog` is wrong).


### 5.5 Arconia and Embabel (docs only, not run)

The brief allowed these only if time allowed. They were not run. Facts from their sources:

**Embabel 1.5.4 (Java 21).**

- Artifacts: `com.embabel.agent:embabel-agent-api` 1.5.4 (Maven 2026-10-28, `J/dl/jars/embabel-agent-api-1.5.4.jar`: class major 65, Java 21), and the observability starter `com.embabel.agent:embabel-agent-starter-observability` 1.5.4 (Maven 2026-10-28T07:27:36Z, on repo1). The GitHub release `v1.5.4` is 2026-10-28T07:35:07Z.
- The observability README is in the repo (`embabel-agent-observability/README.md`, read in `J/dl/embabel-observability-readme.md`). It says "Automatic tracing, metrics, and LLM call integration with zero code changes", and its badges show Java 21, Spring Boot 3.5.12 and OpenTelemetry 1.49.0 (docs, line 1-8).
- The OTel route is Spring Boot's own tracing configuration: `management.otlp.tracing.endpoint` with `opentelemetry-exporter-otlp` (README "Option D: OTLP", about line 173-190), with `management.tracing.export.enabled` (the README notes that `management.tracing.enabled` was removed in Spring Boot 4). So Embabel's spans are Micrometer observations bridged to OTel, as in Spring AI and LangChain4j's native route.
- Other exporters: `com.quantpulsar:opentelemetry-exporter-embabel` 0.6.0 for Langfuse and LangSmith (not on Maven Central here; not checked), and Zipkin.
- Span kinds: the README names `embabel.llm` and `embabel.tool` span types and maps them to LangSmith kinds (docs, "Option A").
- Content: `embabel.agent.platform.observability.capture-message-content`, default `true` per the README's table (about line 287): ChatModel `gen_ai.input.messages`/`gen_ai.output.messages` and `input.value`/`output.value` bodies on agent, action and tool-loop spans, tool-call arguments and results. Default on, the opposite of Koog.
- Standards: the README says spans follow the OpenTelemetry GenAI conventions (`gen_ai.operation.name`, `gen_ai.request.model`, `gen_ai.tool.name`).
- Not verified: whether the starter needs Spring Boot 3.5 or 4 (the README mentions both), the exact span names, and whether the tool and message keys match cetra's normalizer (no run).
- Build note (docs): the README's quick start asks for the repository `https://repo.embabel.com/snapshots`, which is **not** an allowed download source (rule 3). The Central copy of the starter (1.5.4) is what a capture would use, if the snapshot repo is not needed for its dependencies (not checked).

**Arconia 0.31.0 (Java 21) and 0.32.0 (GitHub only).**

- Artifacts: `io.arconia:arconia-core` 0.31.0 (Maven 2026-10-14T22:40:59Z; class major 65, Java 21, 6 classes, `J/dl/jars/arconia-core-0.31.0.jar`). `v0.32.0` is on GitHub (2026-11-03T00:49:03Z) but no `arconia-core` 0.32.0 metadata entry on repo1 at check time.
- Requirements (docs, `docs/modules/ROOT/pages/system-requirements.adoc` on `arconia-io/arconia`): Arconia 0.5+ needs Java 21+; 1.0 (under development) needs Java 25+. Spring Boot: 0.29+ needs Spring Boot 4.1+.
- The repo's modules (listing, `gh api repos/arconia-io/arconia/contents`): `arconia-opentelemetry` (with `arconia-opentelemetry-exporter-sender-http`, `arconia-opentelemetry-micrometer-registry-otlp`, `arconia-opentelemetry-micrometer-metrics-bridge`, `arconia-opentelemetry-logback-bridge`), `arconia-observation` (with `arconia-openinference-ai-semantic-conventions`, `arconia-opentelemetry-ai-semantic-conventions`, `arconia-opentelemetry-semantic-conventions`), and `arconia-ai`, `arconia-dev`, `arconia-integrations`.
- Route (docs, module names only): Arconia's OTel support is a set of Spring Boot and Quarkus modules around `arconia-opentelemetry` and an HTTP sender; GenAI and OpenInference attribute conventions come as separate modules under `arconia-observation`. The OTel page under `docs/modules/ROOT/pages/opentelemetry` was not reachable by the path tried (404), so the exact configuration keys are not read.
- Not verified: which GenAI framework instrumentation Arconia adds to Spring AI or LangChain4j, and its span names. Not run.

## 6. Network

- Every run was inside `unshare -Urn` through `m7/tools/netrun.sh`, so only loopback existed. Scan of every run's `trace.strace` (`J/runs/*/trace.strace`): every `connect()` is to loopback (`127.0.0.1`, `::1`, `::ffff:127.0.0.1`), except three lines in the Quarkus runs (`qlc4j-1`, `qlc4j-2`, `qlc4j-3`, `qlc4j-content-4`), which connect to `127.0.0.53:53` and `127.0.0.2:0` (loopback range, `connect` returns 0 with no route off the host). Those look like the JVM's resolver. No non-loopback address was reached.
- Failed connects that were refused: `spring-ai-2` tried `localhost:4318` (the OTel default, because the program used `OtlpHttpSpanExporter.getDefault()`, which does not read the env route) and got "Connection refused" (`J/runs/spring-ai-2/run.log`). Loopback only.
- The real-home guard: in some runs it reports the worktree directory (`openat` on the calling shell's working directory, `trace.strace` line 46 of `spring-ai-1`). No run read or wrote a file under the real home. The worktree directory is only opened, not read.
- Downloads went to repo1.maven.org (Maven Central, the Maven settings mirror), github.com (Adoptium release assets; GitHub release and repo reads through `gh api`), and `api.adoptium.net`. No other host. The fake provider key is `sk-fake-m8`; no real provider was called.

## 7. Build, reproducibility and cache size (for the plan)

**Tool choice: Maven 3.9.16, one tool for all four projects.** Reasons from the runs: `dependency:build-classpath` gives a plain classpath for `java -cp` or `-jar` runs; `-o` runs from a pre-populated repo; the Quarkus, Spring AI, LangChain4j and Koog docs are Maven-first (the Quarkus plugin is a Maven plugin; Koog's Kotlin compile is `kotlin-maven-plugin`). Gradle (wrapper, `verification-metadata.xml`) was not run; it would add a second toolchain and a Gradle distribution download.

**Offline build (run).** Each project was built in a fresh, empty repository directory (`J/cache/fresh-<project>`) with `mvn package -DskipTests`: Koog 130 MB (224 jars), Spring AI 120 MB, LangChain4j 40 MB, Quarkus 134 MB (382 jars). Then each was rebuilt with `mvn -o` against that directory: zero downloads, `BUILD SUCCESS` (`J/logs/offline-*.log`, `J/logs/fresh-*.log`). The shared cache for the whole spike is 353 MB (`J/cache/m2`, 689 jars).

**Runtime closure (run, `dependency:build-classpath`, the jars a run loads).** Spring AI 75 jars, 98.0 MB; LangChain4j (OpenInference, native observation) 51 jars, 15.7 MB; Koog 144 jars, 35.8 MB; Quarkus app `target/quarkus-app` 38 MB (its own classpath file is in the build output).

**Other fixed downloads.** Temurin JDK 21: 207.9 MB tar, 347 MB extracted (`J/bin/jdk`). Maven 3.9.16: 9.3 MB tar, 11 MB extracted.

**Lockfiles and verification (docs, not run).**

- Maven has no lockfile. The pins live in each pom (versions in the `<version>` fields and the imported BOMs). The transitive set is fixed by the build, and `cp.txt` (`dependency:build-classpath`) is the list to commit or compare. This spike hit one transitive drift: `jackson-annotations` 2.18.9 won over the 2.21 that Jackson 3 needs (§ 5.1), so the capture's pom must pin it.
- Integrity: Maven checks the `.sha1` (or `.sha512` where Central has it) of every downloaded artifact against Central. The default checksum policy for Central is `warn`; a settings `<checksumPolicy>fail</checksumPolicy>` on the repository makes a mismatch fail (Maven settings reference, docs; not run). The cache's sha256 manifest is `J/logs/cache-m2.sha256` (1,877 files).
- Stronger option (not tested): Gradle's `verification-metadata.xml`. Maven has no equivalent in the box.
- Maven wrapper: not used (`mvnw` needs `.mvn/wrapper`, not generated here). A capture runner could pin Maven by the sha512 above.
- The Kotlin program needs the serialization compiler plugin (§ 5.4, finding 1).

## 8. Run index (all integrations)

Evidence: `J/runs/<run>/` (`run.log`, `http.jsonl`, `req/`, `recv/`, `trace.strace`, `out/<run>.dump.txt`, `out/<run>.norm.jsonl`); normalizer logs `J/logs/<run>.h.txt`.

| Run | Program | What it tests | Result |
|---|---|---|---|
| `spring-ai-1` | Spring AI, first version (`OpenAiChatModel.call`), jackson-annotations 2.18.9 | first wiring | `NoClassDefFoundError: JsonSerializeAs`, no call |
| `spring-ai-2` | same, jackson-annotations 2.21 | model call, env route | 1 POST, empty answer (no tool execution); span export to `localhost:4318` refused |
| `spring-ai-3` | final (ChatClient + autoconfigured SDK) | tool round, env route | 2 POSTs, traces and metrics posted, 7 spans |
| `spring-ai-4-content` | final + content handlers + slf4j-simple | content switch | content in the log, 0 span events, 0 content attributes |
| `lc4j-oi-1`, `lc4j-native-1` | LangChain4j, before the alpha BOM | first run | `NoClassDefFoundError: BoundDoubleGauge` |
| `lc4j-oi-2` | `oi` (both listener kinds) | duplicates | 6 spans: agent, 2 LLM children, 2 `generate` roots, tool |
| `lc4j-oi-model-1` | `oi-model` (model listener only) | attribution | 2 `generate` roots, no agent or tool |
| `lc4j-oi-service-1` | `oi-service` (AiService listeners only) | attribution | `AiService.chat`, 2 `LLM` children, `AiService.tool` |
| `lc4j-native-2` | `native` (langchain4j-observation) | native route | 2 `chat` roots, `OPEN_AI` provider, tokens under `input_tokens` |
| `qlc4j-1` | Quarkus LC4J, no request context | startup call | `ContextNotActiveException` |
| `qlc4j-2` | stale jar (the rebuild failed on an import) | none | same error (not a valid test) |
| `qlc4j-3` | Quarkus LC4J with `@ActivateRequestContext` | native OTel | 6 spans, 1 trace, 1 POST |
| `qlc4j-content-4` | `qlc4j-3` + `quarkus.langchain4j.tracing.include-*` | content | `gen_ai.prompt`, `gen_ai.completion`, tool args and result |
| `koog-1` | Koog, `addSpanExporter` (batch) | batch export | 0 OTLP posts |
| `koog-2` | stale build (the rebuild failed) = `koog-1` behaviour | none | not a valid test |
| `koog-3` | Koog with `simpleSpanProcessor`, no serialization plugin | export | 11 spans, but the tool failed (`SerializationException`) |
| `koog-4-verbose` | as `koog-3`, `verbose` | content | same tool error, args visible |
| `koog-5-verbose` | + `kotlin-maven-serialization` | content, fixed | tool result correct, no normalize errors |
| `koog-6-default` | fixed, verbose off | masking | 11 spans, `HIDDEN:non-empty`, normalize errors on llm and agent rows |

## 9. Open questions (for the user or the plan)

1. **Spring AI content.** The design says "`gen_ai.system=spring_ai`, content in events". In 2.0.2 the spans have no events, and the content goes to the log (run `spring-ai-4-content`). Should the capture register a cetra-owned observation handler that writes span events, or accept no content for Spring AI? (The plan decides the content rule; this needs a product decision on what "content" means for cetra's I/O.)
2. **Spring AI wiring.** The spike used plain Java (no Spring Boot). A Boot starter may register the handlers and the tracing bridge itself, and its span shape was not run. Which wiring does the capture use?
3. **LangChain4j listeners.** OpenInference Java's two listener kinds give different spans (§ 5.2), and attaching both duplicates every LLM call. Choose: model listener only (model names, tokens, root traces), AiService listeners only (agent and tool structure, model `unknown`), or both plus a normalizer dedupe rule. Also: LangChain4j's native observation has no agent or tool spans, so it cannot stand alone for the design's "trace the agent" goal.
4. **Java line.** JDK 21 is the lowest LTS that runs every framework here (Embabel and Arconia need 21). Arconia 1.0 (under development) needs 25. Confirm 21 for the capture runner, or move to 25 now.
5. **Build tool.** Maven was chosen; the design does not name one. Confirm, or ask for Gradle (the wrapper and `verification-metadata.xml` are stronger on integrity, § 7).
6. **Koog batch export.** `addSpanExporter` exported nothing before exit (run `koog-1`), and `agent.close()` did not change that. Not investigated: a Koog defect or a flush call we did not make. The capture should use the synchronous processor until someone checks; that needs a decision.
7. **Koog masking.** The default (verbose off) makes the normalizer report `messages:genai:*` errors on every llm row (run `koog-6-default`). Either the capture sets `setVerbose(true)` (content on), or the normalizer learns `HIDDEN:non-empty`. The plan decides; AGENTS.md's privacy rule covers cetra's own logs and errors, not the spans a user's app sends, so it does not decide this.
8. **OpenInference tool name.** `tool.name` on `AiService.tool` is not read into `tool_name` (run `lc4j-oi-2`); the same is true of `gen_ai.tool.name` on Koog and Quarkus tool rows (runs `koog-6-default`, `qlc4j-3`). Is that a normalizer gap to fix in M8?
9. **Quarkus `gen_ai.prompt` and `gen_ai.completion`.** Quarkus writes them when content tracing is on (run `qlc4j-content-4`), and cetra does not read them (`attributes_left`). Same question as 8.
10. **Arconia and Embabel.** Not run. Running Embabel needs its observability starter (on Central) and a Spring Boot app; Arconia needs its Spring Boot or Quarkus modules. Is either in M8 scope, given the time? The brief allowed them only if time allowed.
11. **Embabel's snapshot repository.** Its README points to `repo.embabel.com/snapshots`, which the rules do not allow as a download source. The Central copy of the starter (1.5.4) exists. Confirm that the capture may use Central only.
12. **OpenTelemetry Java agent.** Not tested: `-javaagent` instrumentation (it might capture OpenAI SDK calls without library setup). If the plan wants agent-based capture, that needs a run.

## 10. Downloads

| Item | URL | Bytes | sha256 / sha512 |
|---|---|---|---|
| Temurin JDK 21.0.13+9 | `https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.13%2B9/OpenJDK21U-jdk_x64_linux_hotspot_21.0.13_9.tar.gz` | 207,858,352 | sha256 `d7f5d5cf865a2902b70ff549b97586a7d3f234e016f041ab447ea740c7d66d1c` (file `J/dl/OpenJDK21U-jdk_x64_linux_hotspot_21.0.13_9.tar.gz`; sha256 verified) |
| Apache Maven 3.9.16 | `https://repo1.maven.org/maven2/org/apache/maven/apache-maven/3.9.16/apache-maven-3.9.16-bin.tar.gz` | 9,278,065 | sha512 `831a8591fe20c8243b1dbe7d71e3244f31d1665b0804b2e825e38cbbe5ce0cafb8338851f90780735568773e0a6cd07bbec107cda0b896b008b861075358b6f6` (verified against repo1's `.sha512`) |
| Temurin JDK pointer JSON | `https://api.adoptium.net/v3/assets/feature_releases/{21,17,25}/ga` | small | `J/dl/temurin{21,17,25}-assets.json` |
| Maven Central artifacts (jars, poms, plugins) | repo1.maven.org | 1,877 files in `J/cache/m2`, 353 MB | per-file sha256 in `J/logs/cache-m2.sha256` |
| Jars and poms read for class versions and APIs | repo1.maven.org | 30 files | `J/logs/dl.sha256` |
| Source jars (Spring AI 2.0.2, micrometer-tracing-bridge-otel 1.7.2) | repo1.maven.org | small | `J/dl/jars/*-sources.jar` (in `dl.sha256`) |
| Koog, Quarkus, LangChain4j, Embabel, Arconia, OpenInference docs and READMEs | GitHub (`gh api`, read only) | small | `J/dl/*.md`, `J/dl/*.adoc` |

Evidence paths: `J/runs/…`, `J/logs/…`, `J/projects/…`, `J/runners/…`, `J/tools/…`. `J/` is `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m8/java`.
