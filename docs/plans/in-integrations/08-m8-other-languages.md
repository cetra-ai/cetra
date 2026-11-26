# IN M8 — Other languages (Java, .NET; setup-only Go, Ruby, Rust)

*Cold review (Sonnet) round 1 applied, with spike 3 (`m8-followup.md` § 9): the OpenInference Spring AI wiring on Boot, the index schema, the Boot and Quarkus configuration, libraries per case, the .NET OpenAI and Koog OTel versions, base URLs, the event-body rule's exact shape, the runner's scratch directory, the build hash exclusions.*

This plan gives cetra's capture tool two new languages, Java and .NET, and uses them to capture Spring AI (two routes), LangChain4j, Quarkus LangChain4j, Koog, Microsoft.Extensions.AI, Semantic Kernel .NET and Microsoft Agent Framework .NET against the mock provider. It adds the four normalizer rules those captures need, a page for each, setup-only pages for Genkit Go, openai-go, ruby_llm, rig and Arconia, and § Not supported rows for the integrations with no OpenTelemetry route (D-IN-30).

**Prerequisites:**
- M7 part 1 done. The records phase IN8.7 runs after M7 part 2's IN7.15 (both edit the index, sidebar, CHANGELOG and decision files; IN7.15 records D-IN-29 in `mvp-decisions.md` before IN8.7 adds D-IN-30).
- D-IN-30, the M8 rows and the § Not supported rows are committed in `00-design.md` (with this plan).
- The plan's branch is `claude/in-integrations`, worktree `.claude/worktrees/integrations`.

**Read first:**
- `AGENTS.md`.
- `docs/plans/in-integrations/00-design.md`: § Other languages (M8), § Not supported, D-IN-9, D-IN-26 items 1, 2, 9 and 11, D-IN-28 item 8, **D-IN-30**.
- The spikes, all of them: `spikes/m8-java.md`, `spikes/m8-dotnet.md`, `spikes/m8-followup.md`, `spikes/m8-setup-only.md`.
- The spikes' working programs, committed under `spikes/m8-files/` (fake keys `sk-fake-m8`; read, never copy those keys):
  - `java/{spring-ai,langchain4j,quarkus-lc4j,koog}/` (poms and sources);
  - `followup/boot-ai/` (Spring Boot 4.1.2 with the Spring AI starter; `props/route-b.properties` is the working export config, `route-a` the one that exports nothing), `followup/oi-spring/` (OpenInference's Spring AI handler), `followup/go-openai/` (openai-go v3 with OpenInference Go);
  - `dotnet/{extai,sk,maf}/` (csproj, `Program.cs`, `packages.lock.json`).
- The code: `tools/capture/src/{run.rs,write.rs,scenario.rs,mock.rs}`, `tools/capture/fetch-tools.sh`, the `capture*` recipes of `justfile`, `tools/capture/ts/harness.ts` (`writeMeta`, the meta contract), `crates/otel/src/conventions/{genai.rs,scoped.rs}`, `crates/otel/src/context.rs` (`FRAMEWORK_NAMES`, `exact_alias`).

## What it is, and what it is not

**When it is done:**
- `cetra-capture` runs Java and .NET scenarios (`tools/capture/java/`, `tools/capture/dotnet/`), offline in the loopback namespace, from programs that `just capture-tools` built.
- Captured cases (D-IN-30 item 1; groups as in D-IN-23 item 10):
  - `genai/spring-ai-boot-tool` (Boot, Micrometer route: native `gen_ai.*`, so group `genai` per D-IN-23 item 10) and `openinference/spring-ai-tool` (the OpenInference handler; its wiring is `spikes/m8-followup.md` § 9's result);
  - `openinference/langchain4j-tool` (model listener only);
  - `genai/quarkus-langchain4j-tool`, `genai/koog-agent-tool`;
  - `genai/extensions-ai-dotnet-tool`, `genai/semantic-kernel-dotnet-tool`, `genai/agent-framework-dotnet-tool`.
- The normalizer rules of D-IN-30 item 4, with unit tests.
- Pages: `spring-ai`, `langchain4j`, `quarkus-langchain4j`, `koog`, `extensions-ai` (captured); a ".NET" section on the existing `semantic-kernel` and `agent-framework` pages; setup-only `genkit-go`, `openai-go`, `ruby-llm`, `rig`, `arconia`.
- Index, sidebar, § Not supported rows, D-IN-30, CHANGELOG, roadmap.

**Not in this plan:**
- Go, Ruby or Rust runners: those pages are setup only (D-IN-9, D-IN-30 item 5).
- LangChain4j's own Micrometer route, the OpenTelemetry Java agent, Gradle (D-IN-30).
- Spring AI's content log records (cetra drops logs; D-IN-30 item 2).
- Semantic Kernel's singular `gen_ai.response.finish_reason` and URL-shaped `server.address`: not read; the page's Limits say so.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-IN-9 | Java and .NET get capture runners and captured cases; Go, Ruby and Rust are setup only |
| D-IN-23 item 10 | Fixture groups: native GenAI frameworks in `genai`, OpenInference in `openinference` |
| D-IN-26 items 1, 2, 9, 11 | Tool environments in `target/capture-tools/`; the scoped content table; captures offline in a loopback namespace |
| D-IN-28 item 8 | The version rule: pins win; later pins are the newest release ≥ 7 days old |
| D-IN-30 | This milestone's scope, routes and rules |

The user's answers (2026-11-24), recorded in D-IN-30: all the recommendations after the competitor comparison (Spring AI: Boot, two routes, no content on the Micrometer route; LangChain4j: model listener; Arconia setup only; Embabel and the four Go/Rust/C++ items not supported).

Details this plan fixes, within D-IN-30:

1. **Scenario layout.**
   - Java: one Maven reactor, `tools/capture/java/pom.xml` (packaging `pom`, `groupId` `cetra.capture`, `artifactId` `capture-reactor`, `version` `0.0.0`), with the module `harness` and one module per scenario at `tools/capture/java/scenarios/<group>/<case>/` (module path `scenarios/<group>/<case>`). Each scenario module has a `scenario.json`, as the other languages' scenarios do.
   - Every scenario module's parent is the reactor pom. The reactor pom's `dependencyManagement` lists `cetra.capture:harness:0.0.0`, and each scenario module depends on it. Each module's `<parent>` has `<relativePath>../../../pom.xml</relativePath>`. A Spring Boot module does not use `spring-boot-starter-parent`: it imports `org.springframework.boot:spring-boot-dependencies:4.1.2` and `org.springframework.ai:spring-ai-bom:2.0.2` (both `<type>pom</type><scope>import</scope>`) in its own `dependencyManagement`. The reactor sets `maven.compiler.parameters` `true` (Spring's `@Tool` methods and Quarkus need `-parameters`).
   - Resource filtering is on only for `capture-versions.properties`: each scenario module declares two `<resource>` entries for `src/main/resources`, one filtered that includes only that file and one unfiltered that excludes it.
   - .NET: `tools/capture/dotnet/Harness/Harness.csproj` (a class library) and one console project per scenario at `tools/capture/dotnet/scenarios/<group>/<case>/`, each with `scenario.json`, `Capture.csproj` (the project file is always named `Capture.csproj`, assembly name `Capture`), `Program.cs` and a committed `packages.lock.json` (`RestorePackagesWithLockFile=true`).
2. **Build output the runner runs:**
   - Java: each scenario module's build writes `target/capture.args`, a Java argument file. For a plain module its content is `-cp <target/classes>:<the module's runtime classpath> capture.Main`; for the Quarkus module it is `-jar target/quarkus-app/quarkus-run.jar`. The runner runs `<jdk>/bin/java @target/capture.args` in the scenario directory.
   - .NET: `dotnet build` with `-o bin/capture`, then the runner runs `<dotnet>/dotnet bin/capture/Capture.dll` in the scenario directory.
3. **Toolchain locations** (under `target/capture-tools/`): `jdk/21.0.13+9/` (`bin/java`), `maven/3.9.16/` (`bin/mvn`), `m2/` (the Maven repository), `dotnet/10.0.404/` (`dotnet`), `nuget/` (the NuGet packages folder).
4. **Builds run in `just capture-tools`**, online, after the fetches: `mvn -B -q -Dmaven.repo.local=<m2> package -DskipTests` on the reactor; for each .NET project `dotnet restore --locked-mode -p:NuGetAudit=false` then `dotnet build --no-restore -p:NuGetAudit=false -p:UseSharedCompilation=false -nodeReuse:false -o bin/capture`, then `dotnet build-server shutdown` (spike `m8-dotnet.md` § 6.5: a build server left running keeps the parent waiting). Each build is skipped when its inputs (poms and sources, or csproj, lock and sources; never anything under `target/`, `bin/` or `obj/`) are unchanged since the last build: a hash of the input files stored next to the output.
5. **Library versions (the meta contract).** Java: each scenario module filters `src/main/resources/capture-versions.properties` (Maven resource filtering) with one `<library name>=${<property>}` line per library, from the same properties its pom pins; the harness's `Capture.writeMeta()` writes `CETRA_CAPTURE_META` from it for the names in `CETRA_CAPTURE_LIBRARIES` and fails on a missing name. .NET: the harness's `Capture.WriteMeta()` reads `packages.lock.json` in the working directory, `dependencies["net10.0"][<name>].resolved`, for each listed name, and fails on a missing one. Both write `{"libraries": {...}}` after the tracer provider is flushed and closed.
6. **Runner environment.** For `java` and `dotnet` programs, `program_env` also sets `JAVA_HOME` (Java) or `DOTNET_ROOT`, `DOTNET_CLI_TELEMETRY_OPTOUT=1`, `DOTNET_NOLOGO=1`, `DOTNET_CLI_HOME=<a directory the runner creates for the case>` (.NET), from the runner itself, not from `scenario.env`, whose allowed prefixes do not change.
7. **Spring Boot programs** read the receiver and mock URLs from `CETRA_CAPTURE_OTLP_ENDPOINT` and `CETRA_CAPTURE_MOCK_URL` and set them as Spring properties in code (`SpringApplication.setDefaultProperties`) before `run`: `management.opentelemetry.tracing.export.otlp.endpoint` (the full traces URL), `management.tracing.sampling.probability=1.0`, `management.otlp.metrics.export.enabled=false` (`spikes/m8-followup.md` § 9.2), `spring.ai.openai.base-url` = `Capture.mockBaseUrl() + "/v1"` (as every spike file sets it), `spring.ai.openai.api-key` = `Capture.mockKey()`. `WebApplicationType.NONE`. After the call: `SpringApplication.exit(ctx)` (which flushes the exporter, as `BootRun.java` does), then `Capture.writeMeta()`, then `System.exit(0)`.
8. **Base URLs.** `CETRA_CAPTURE_MOCK_URL` has no path. Each program appends what its spike program appended: `/v1` for LangChain4j, Quarkus and the three .NET programs (`baseUrl + "/v1"` in the spike files); Spring AI's base-url property with `/v1` (detail 7); Koog the bare URL (`OpenAIClientSettings(baseUrl = base)`). Follow the spike file for each; do not normalize them.
9. **OTel Java autoconfigure** (LangChain4j, and any plain-Java program using `AutoConfiguredOpenTelemetrySdk`): its metrics and logs exporters default to OTLP, so those scenarios set `"env": {"OTEL_METRICS_EXPORTER": "none", "OTEL_LOGS_EXPORTER": "none"}` in `scenario.json`.
10. **Random span names.** Koog (`create_agent <session-uuid>`, `invoke_agent <uuid>`) and Agent Framework .NET (`invoke_agent <agent>(<32-hex>)`) put random ids in span names, and `volatile_attributes` masks only attribute values. IN8.1 adds a scenario field `volatile_name_prefixes: [string]`: a span whose name starts with a listed prefix has the rest of its name replaced by `<volatile>` in the written fixture and before the `--check` comparison, in the same place `volatile_attributes` is applied (`tools/capture/src/determinism.rs`, `revolatile`). D-IN-30 item 7 records it.
11. **Harness endpoints.** Both harnesses give `otlpTracesEndpoint()` (the runner's full `…/v1/traces` URL) and `otlpBaseEndpoint()` (the same with the trailing `/v1/traces` removed; fail if it does not end so). Quarkus and .NET use the base endpoint, the form their spikes verified.
12. **Pins** (§ Versions). They win over later releases (D-IN-28 item 8).

## Rules (D-IN-30 item 4)

| Rule | Where | Behaviour |
|---|---|---|
| Event body | `conventions/genai.rs` `generic_legacy_messages` only (not the Strands readers) | An input or output event (`gen_ai.system.message`, `.user.message`, `.assistant.message`, `.tool.message`, `gen_ai.choice`) with an attribute `gen_ai.event.content` whose string value is a JSON object: that object is the message. For `gen_ai.choice`, its `message` object is the message and its `finish_reason` becomes the message's finish reason, lowercased as the role is (Semantic Kernel writes `Stop` and `ToolCalls`), as `genai_messages` (`genai.rs` ~225–258) treats a choice. The role is lowercased (Semantic Kernel writes `Assistant`). The event's other attributes (such as `gen_ai.system`) are not merged into that message. Without `gen_ai.event.content`, the event is read exactly as today. A value that is not a JSON object is not Semantic Kernel's: the event is read exactly as today too, as though the attribute were absent (no normalize error). |
| Masked content | the GenAI message readers (`gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.system_instructions`) | A string value that starts with `HIDDEN:` is content not captured: the key is consumed, the reader returns at once without consulting later sources (in `read_messages`, `genai.rs` ~145–157, a `None` would fall through to the details, framework and legacy readers: it must not), no input or output is set, and no `messages:genai:*` normalize error is added. Koog also masks `gen_ai.tool.call.arguments`, `gen_ai.tool.call.result` and `gen_ai.tool.definitions`; those stay as they are (the tool's previews read `HIDDEN:non-empty`), which the Koog page states. No capture exercises this rule (the capture uses verbose mode); the unit tests are its only coverage. |
| Framework name | `context.rs` `FRAMEWORK_NAMES` | gains `koog` (sorted). |
| Provider alias | `context.rs` `exact_alias` | `open_ai` → `openai`. |
| Quarkus content | `conventions/scoped.rs` | scope exactly `io.quarkus.opentelemetry`: `gen_ai.prompt` fills input and `gen_ai.completion` fills output, only where the convention readers left them empty, with `take_value` as Langflow's keys. |

## Versions

The implementer re-checks each publish time and reports the version used. These pins win (D-IN-28 item 8). The spike files list the transitive pins each build needed.

| What | Version | Source and checksum | Where |
|---|---|---|---|
| Temurin JDK | 21.0.13+9 | `https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.13%2B9/OpenJDK21U-jdk_x64_linux_hotspot_21.0.13_9.tar.gz`, sha256 `d7f5d5cf865a2902b70ff549b97586a7d3f234e016f041ab447ea740c7d66d1c` | `fetch-tools.sh` |
| Apache Maven | 3.9.16 | `https://repo1.maven.org/maven2/org/apache/maven/apache-maven/3.9.16/apache-maven-3.9.16-bin.tar.gz`, sha512 `831a8591fe20c8243b1dbe7d71e3244f31d1665b0804b2e825e38cbbe5ce0cafb8338851f90780735568773e0a6cd07bbec107cda0b896b008b861075358b6f6` | `fetch-tools.sh` |
| .NET SDK | 10.0.404 | `https://builds.dotnet.microsoft.com/dotnet/Sdk/10.0.404/dotnet-sdk-10.0.404-linux-x64.tar.gz`, sha256 `c0610757a818fde4b960d27a0ac421f273068a05dc5ec7abafc01dbf84e9cb4d` | `fetch-tools.sh` |
| Spring Boot | 4.1.2 | Maven Central | `genai/spring-ai-boot-tool`, `openinference/spring-ai-tool` |
| Spring AI (BOM, OpenAI starter) | 2.0.2 | Maven Central | same; the BOM must be imported (`m8-followup.md`: without it Maven resolves `spring-ai-model` 1.0.1) |
| OpenInference Java, Spring AI | 0.1.10 | Maven Central | `openinference/spring-ai-tool` |
| LangChain4j, LangChain4j OpenAI | 1.23.0 | Maven Central | `openinference/langchain4j-tool` |
| OpenInference Java, LangChain4j | 0.1.9 | Maven Central | same |
| Quarkus platform, Maven plugin | 3.40.1.2 | Maven Central | `genai/quarkus-langchain4j-tool` |
| Quarkus LangChain4j OpenAI | 1.15.1 | Maven Central | same |
| Koog (`koog-agents-jvm`, `agents-features-opentelemetry-jvm`) | 1.3.0 | Maven Central | `genai/koog-agent-tool` |
| Kotlin Maven plugin, serialization plugin | 2.3.21 | Maven Central | same |
| OpenTelemetry Java (BOM, SDK, OTLP exporter, autoconfigure) | 1.68.0 (alpha BOM 1.68.0-alpha) | Maven Central | the LangChain4j module, as its spike pom |
| OpenTelemetry Java BOM | 1.61.0 | Maven Central | the Koog module, as its spike pom (Koog's feature is built against it) |
| Microsoft.Extensions.AI, .OpenAI | 11.0.0 | nuget.org | `genai/extensions-ai-dotnet-tool` |
| OpenAI (.NET) | 2.15.0 | nuget.org | `extensions-ai-dotnet-tool` only, as a direct reference. Semantic Kernel and Agent Framework take it transitively (2.10.0 and 2.14.0 in the spike locks): add no direct reference |
| OpenTelemetry, .Exporter.OpenTelemetryProtocol (.NET) | 1.21.0 | nuget.org | the three .NET scenarios |
| Microsoft.SemanticKernel | 1.82.0 | nuget.org | `genai/semantic-kernel-dotnet-tool` |
| Microsoft.Agents.AI, .OpenAI | 1.28.0 | nuget.org | `genai/agent-framework-dotnet-tool` |

## Phases and dependencies

| Phase | Edits | Depends on |
|---|---|---|
| IN8.0 Toolchains | `tools/capture/fetch-tools.sh` | M7 part 2's IN7.9 (same script) |
| IN8.1 Runner languages | `tools/capture/src/{write.rs,run.rs,scenario.rs,determinism.rs,error.rs}`, doc comments in `justfile` (`capture-tools`) and `AGENTS.md` (the `tools/capture` lines), new `tools/capture/java/{pom.xml,harness/}`, new `tools/capture/dotnet/Harness/`, new `tools/capture/build-jvm-dotnet.sh`, `tools/capture/fetch-tools.sh` (the last line calls the build script), `.gitignore` if `bin/capture` and `target/` under `tools/capture/{java,dotnet}` are not already ignored | IN8.0 |
| IN8.2 Normalizer rules | `crates/otel/src/conventions/{genai.rs,scoped.rs}`, `crates/otel/src/context.rs`, `docs/spec/02-ingestion.md` | — |
| IN8.3 Java captures | new `tools/capture/java/scenarios/...` (five modules), `tools/capture/java/pom.xml` (module list), new fixtures (no new group) | IN8.1, IN8.2 |
| IN8.4 .NET captures | new `tools/capture/dotnet/scenarios/...` (three projects), new fixtures | IN8.1, IN8.2; after IN8.3 for machine load only |
| IN8.5 Captured pages | new `website/src/content/docs/docs/integrations/{spring-ai,langchain4j,quarkus-langchain4j,koog,extensions-ai}.mdx`, `semantic-kernel.mdx` and `agent-framework.mdx` (a .NET section) | IN8.3, IN8.4 |
| IN8.6 Setup-only pages | new `website/src/content/docs/docs/integrations/{genkit-go,openai-go,ruby-llm,rig,arconia}.mdx` | — |
| IN8.7 Index and records | `index.mdx`, `astro.config.mjs`, `docs/plans/in-integrations/00-design.md`, `docs/mvp-decisions.md`, `CHANGELOG.md`, `docs/roadmap.md`, `skills/cetra/references/*` (generated) | IN8.5, IN8.6; M7 part 2's IN7.15 |

**Parallel runs:**
- IN8.2 and IN8.6 can run beside everything, including M7 part 2's phases, except that IN8.2 and M7 part 2's IN7.10 both edit `crates/otel` (different files; compile one after the other).
- IN8.0 after IN7.9 (both edit `fetch-tools.sh`); IN8.1 after IN8.0.
- IN8.3 and IN8.4 add no group, so they do not conflict with M7 part 2's IN7.13 on `GROUPS`; IN8.1 edits `tools/capture/src/run.rs` and `write.rs`, and IN7.13 edits `mock.rs` and `scenario.rs`: no overlap. Still, one capture run at a time (machine load).
- Machine load: one heavy build at a time (Rust, Maven or dotnet).

---

## IN8.0 — Toolchains

**Goal:** `just capture-tools` fetches the pinned JDK, Maven and .NET SDK.

**Edit:** `tools/capture/fetch-tools.sh`.

**Steps.**
1. The existing `fetch` takes one binary out of an archive. These three are whole trees, so add a sibling helper, `fetch_tree <final-dir> <url> <checksum-kind> <checksum> <strip-components>`, in `fetch`'s shape: skip when `<final-dir>/.complete` exists; download to a temp file in the parent; check it with `sha256sum -c` or `sha512sum -c` by `<checksum-kind>` (`sha256` or `sha512`); extract into a temp directory with `tar -xzf --strip-components=<n>`; move it to `<final-dir>` and touch `<final-dir>/.complete` last.
2. Three calls with § Versions' URLs and checksums: `"$tools_dir/jdk/21.0.13+9"` (sha256, strip 1), `"$tools_dir/maven/3.9.16"` (sha512, strip 1), `"$tools_dir/dotnet/10.0.404"` (sha256, strip 0: the SDK tarball has no top folder). Read each archive's top level with `tar -tzf … | head` before choosing the strip count; if it differs from this plan, stop and ask.
3. After the fetch, run `<jdk>/bin/java -version`, `<maven>/bin/mvn -v` (with `JAVA_HOME` set to the JDK) and `<dotnet>/dotnet --version` (with `DOTNET_CLI_TELEMETRY_OPTOUT=1`, `DOTNET_NOLOGO=1`, `DOTNET_CLI_HOME` in a temp dir): expected `21.0.13`, `3.9.16`, `10.0.404`.
4. Update the script's header comment to list every tool it fetches or installs (it lists only some today).

**Done when:** `just capture-tools` succeeds twice (the second run skips the three), and the three version outputs match.

**Commit:** `Pin the JDK, Maven and the .NET SDK for captures`

## IN8.1 — Runner languages

**Goal:** `cetra-capture` discovers, and runs, Java and .NET scenarios built by `just capture-tools`.

**Edit:** as the phase table says.

**Steps.**
1. **`write.rs`:** `Language` gains `Java` (serialized `java`, doc "`tools/capture/java`") and `Dotnet` (serialized `dotnet`, doc "`tools/capture/dotnet`"). Check the fixture loader (`crates/otel/tests/support/mod.rs` and anything that reads `case.json`'s `source.language`) accepts the new strings; if one enumerates the languages, stop and ask (it is outside this phase's Edit list).
2. **`run.rs` `discover`:** scan `tools/capture/java/scenarios` with `Language::Java` and `tools/capture/dotnet/scenarios` with `Language::Dotnet`, as the `ts` root is scanned (`scan` already finds `<group>/<case>/scenario.json`). Update its doc comment.
3. **`Program::for_language`:** `Java` → argv `[<repo>/target/capture-tools/jdk/21.0.13+9/bin/java, @target/capture.args]`; `Dotnet` → `[<repo>/target/capture-tools/dotnet/10.0.404/dotnet, bin/capture/Capture.dll]`. Both run in the scenario directory (`spawn_program` already sets `current_dir`). Put the two tool paths in constants next to `PROGRAM_TIMEOUT`, with the versions in the names' doc comments. In `run_case`, when the language is Java or .NET, check that `program.argv[0]` exists before spawning; if not, return a new `CaptureError::ToolMissing { path }` whose message is "<path> is missing: run just capture-tools" (add it to `error.rs` with a test of its message).
4. **`program_env`:** gains two parameters after `meta_path`: `language: Language` and `scratch: &Path`. `run_case` creates `scratch` as a directory with a drop guard, modelled on `MetaFile` (`unique_temp("cetra-capture-scratch", case)`), for every language. For `Java`: `JAVA_HOME` = the JDK directory. For `Dotnet`: `DOTNET_ROOT` = the SDK directory, `DOTNET_CLI_TELEMETRY_OPTOUT=1`, `DOTNET_NOLOGO=1`, `DOTNET_CLI_HOME` = `scratch`. Python and TypeScript are unchanged. These are set before `scenario.env` is applied. Update the doc comment ("Nothing else from `parent` is present" stays true) and the two existing test calls of `program_env`.
5. **Java harness** (`tools/capture/java/harness/`, a module of the reactor `tools/capture/java/pom.xml`):
   - the reactor pom: `groupId` `cetra.capture`, `packaging` `pom`, Java 21 (`maven.compiler.release` 21), `project.build.sourceEncoding` UTF-8, pinned `maven-compiler-plugin` 3.16.0, `maven-dependency-plugin` 3.11.0, `maven-resources-plugin` (the version Maven 3.9.16 resolves by default; pin it and report it), modules: `harness` (IN8.3 adds the scenarios);
   - `cetra.capture.Capture` (package `cetra.capture`, no dependencies beyond the JDK): `static String mockBaseUrl()` (`CETRA_CAPTURE_MOCK_URL`), `static String otlpTracesEndpoint()` (`CETRA_CAPTURE_OTLP_ENDPOINT`), `static String mockKey()` (`OPENAI_API_KEY`), each failing with an `IllegalStateException` naming the variable when it is unset; `static void writeMeta()` per detail 5 (it reads `capture-versions.properties` from the classpath and writes `{"libraries":{…}}` with the names sorted; JSON written by hand with escaping of `"` and `\`, since the harness has no JSON dependency).
   - **Args file.** `target/capture.args` is not written by Maven. The reactor pom binds `maven-dependency-plugin:build-classpath` for every module (a `<pluginManagement>` entry plus a `<plugins>` execution in the reactor pom, id `capture-classpath`, phase `package`, `outputFile` `${project.build.directory}/classpath.txt`), and `build-jvm-dotnet.sh` writes `capture.args` from that file (step 7).
   - `writeMeta()` reads `capture-versions.properties` through `Thread.currentThread().getContextClassLoader()` (Quarkus's fast-jar uses its own class loader).
6. **.NET harness** (`tools/capture/dotnet/Harness/Harness.csproj`, `net10.0`, `Nullable` enabled, no package references, root namespace `Cetra.Capture`): `public static class Capture` in namespace `Cetra.Capture` with `MockBaseUrl`, `OtlpTracesEndpoint`, `MockKey` (as Java, throwing `InvalidOperationException`) and `WriteMeta()` per detail 5, with `System.Text.Json`. Scenario projects reference it with `<ProjectReference Include="../../../Harness/Harness.csproj" />`.
7. **`tools/capture/build-jvm-dotnet.sh`** (bash, `set -euo pipefail`, run from `fetch-tools.sh`'s last line):
   - Java: run whenever `tools/capture/java/pom.xml` exists (so the harness and Maven's plugins are fetched into `<m2>` even before a scenario exists). Hash every `pom.xml`, `*.java`, `*.kt` and `*.properties` under `tools/capture/java`, excluding every `target/` directory (sorted paths, `sha256sum`), compare with `target/capture-tools/java-build.sha256`; skip only when it matches and every scenario module has `target/capture.args`. Otherwise delete every `target/capture.args`, run `JAVA_HOME=<jdk> <mvn> -B -q -Dmaven.repo.local=<m2> package -DskipTests -f tools/capture/java/pom.xml`. Then for each scenario module: when `target/quarkus-app/quarkus-run.jar` exists write `-jar target/quarkus-app/quarkus-run.jar` into `target/capture.args`; else write `-cp target/classes:<content of target/classpath.txt> capture.Main`. Store the hash last.
   - .NET: for each `tools/capture/dotnet/scenarios/*/*/Capture.csproj`, hash the project directory's `*.cs`, `*.csproj`, `packages.lock.json` plus the Harness sources, excluding `bin/` and `obj/`; when changed (`<project>/bin/capture/.inputs.sha256`), run detail 4's commands with `DOTNET_ROOT`, `DOTNET_CLI_TELEMETRY_OPTOUT=1`, `DOTNET_NOLOGO=1`, `DOTNET_CLI_HOME` (a temp dir), `NUGET_PACKAGES=<nuget>`, then store the hash.
   - Both: print `capture-tools: built <n> Java scenarios` / `… .NET scenarios` or `… up to date`.
7a. **`volatile_name_prefixes`** (detail 10): an optional `Vec<String>` field in `scenario.rs`'s `Scenario` (default empty; each prefix non-empty, else `ScenarioInvalid`), applied in `determinism.rs` where `volatile_attributes` is applied, to span names only. Tests: `volatile_name_prefix_masks_the_rest_of_the_name`, `a_name_without_the_prefix_is_unchanged`, `an_empty_prefix_is_rejected`.
7b. **Doc comments:** `run.rs`'s `Program` docs (~lines 36–43), `scenario.rs`'s module doc, the `justfile` `capture-tools` comment and `AGENTS.md`'s `tools/capture` lines name Java and .NET.
8. No new lint recipe: the Java and .NET compilers are the check, run by `capture-tools`, which `check-capture` already depends on.
9. **`.gitignore`:** `tools/capture/java/**/target/` and `tools/capture/dotnet/**/bin/`, `**/obj/` if not already ignored.

**Tests** (`run.rs`, next to the existing discovery and env tests; extend `write_scenario` with the two roots):
- `discover_reads_java_and_dotnet_scenarios`.
- `java_program_runs_the_args_file` and `dotnet_program_runs_the_built_dll`: the argv.
- `program_env_sets_the_java_and_dotnet_variables`: the four .NET keys and `JAVA_HOME`; Python and TypeScript get none of them.
- `write_case_serializes_java_and_dotnet` (in `write.rs`, as the Python and TypeScript cases).

**Done when:**
- `cargo nextest run -p cetra-capture`, clippy (`-D warnings`) and `cargo fmt -p cetra-capture --check` pass.
- `just capture-tools` succeeds and builds the harness online (no scenario yet; the .NET loop finds no project), then a second `just capture-tools` reports both up to date, and `mvn -o -q -f tools/capture/java/pom.xml -Dmaven.repo.local=<m2> package` with `JAVA_HOME` set succeeds offline.
- `dotnet build tools/capture/dotnet/Harness/Harness.csproj` succeeds with detail 4's flags.

**Commit:** `Run Java and .NET capture scenarios`

## IN8.2 — Normalizer rules

**Goal:** the five rules of § Rules.

**Edit:** as the phase table says.

**Steps.**
1. **Event body** (`conventions/genai.rs`, `generic_legacy_messages` only): for each input or output event, before building the message from the event's attributes (`event_message`), look for `gen_ai.event.content`:
   - a string holding a JSON object: parse it; for `gen_ai.choice`, take its `message` object and set that object's `finish_reason` to the top-level `finish_reason` when that is a string; drop every key whose value is `null`; lowercase `role` when it is a string. That object is the event's array element. The event's other attributes (`gen_ai.system`) are not merged in. The array then goes to `openai_messages` as today.
   - any other string: push `Json::String(<the attribute value>)` as the event's array element. `openai_messages` then fails with `NotAMessage`, and the existing `Payload::Json(<the array of event elements>), true` branch stores the array as the field's payload with the normalize error.
   - no `gen_ai.event.content`: unchanged (`event_message`).
   Strands readers are not touched.
2. **Masked content:** in `span_messages` (for `gen_ai.input.messages` and `gen_ai.output.messages`) and `instruction()` (for `gen_ai.system_instructions`), a string value starting with `HIDDEN:` is consumed and yields "no content, stop": change `span_messages`'s return to `Option<Option<(Payload, bool)>>` (outer `None` = key absent, fall through as today; `Some(None)` = masked, return no payload and do not consult later sources) and update its callers. Put the prefix in one constant with a doc comment naming Koog. (Koog writes its system prompt under the unprefixed `system_instructions`, which cetra does not read; the `gen_ai.system_instructions` case is covered by a unit test only.)
3. **`FRAMEWORK_NAMES`:** add `koog`, keeping the list sorted (the test `framework_names_are_sorted` checks it).
4. **`exact_alias`:** add `"open_ai" => "openai"`.
5. **`scoped.rs`:** a branch for scope `io.quarkus.opentelemetry`: input from `gen_ai.prompt`, output from `gen_ai.completion`, each only when `filled` lacks it, with `take_value`. Update the module doc's list.
6. **Spec 02** (`docs/spec/02-ingestion.md`): § Mapping tables / Input / output (the event-body rule, the masked-content rule, and the Quarkus row with the other scoped-content rows, ~line 124); § Normalization rules item 12 (the framework name and the alias, ~lines 287–289).

**Tests** (unit, next to each rule):
- `event_content_attribute_is_the_event_body`: the exact raw event attributes recorded in `spikes/m8-followup.md` § 9.4, with `<RUN>` replaced by `/capture` (Semantic Kernel's `gen_ai.user.message`, `gen_ai.assistant.message` with a tool call, `gen_ai.tool.message`, `gen_ai.choice`, each with `gen_ai.event.content` and `gen_ai.system`), as one span: input is the user, assistant and tool messages in order, output the choice's message with its finish reason, roles lowercase, no normalize error.
- `event_without_content_attribute_is_read_as_before` (an existing test may already cover it; name it in the report).
- `event_content_that_is_not_an_object_is_a_normalize_error`: `gen_ai.event.content` = `not json` on a `gen_ai.user.message` event gives `cetra.normalize_error` containing `messages:genai:input`.
- `hidden_messages_are_not_captured_and_not_errors`: `gen_ai.input.messages` = `HIDDEN:non-empty` (and a legacy `gen_ai.user.message` event on the same span, which must not be read) gives no input, no `cetra.normalize_error`.
- `hidden_system_instructions_are_not_captured`.
- `koog_is_a_framework_not_a_provider`: `gen_ai.provider.name=koog` on a tool span gives no provider.
- `open_ai_is_openai`.
- `quarkus_prompt_and_completion_fill_input_and_output` and `quarkus_keys_on_another_scope_are_untouched`.

**Done when:** `cargo nextest run -p cetra-otel`, clippy and `cargo fmt -p cetra-otel --check` pass; no `expected.json` changes (stop if one does).

**Commit:** `Read Semantic Kernel event bodies, Koog's masked content and Quarkus content`

## IN8.3 — Java captures

**Goal:** five Java cases captured against the mock.

**Edit:** as the phase table says.

**Steps.**
1. **Groups:** none new (`genai` and `openinference` exist).
2. **Modules.** Each scenario module's parent is the reactor pom, `artifactId` `<group>-<case>`, main class `capture.Main` (Kotlin: `capture.MainKt` is not allowed; set `@file:JvmName("Main")` in package `capture`), a dependency on `cetra.capture:harness`, `capture-versions.properties` per detail 5, and its own pins from § Versions in `<properties>`. Each library line of `capture-versions.properties` is `<library>=${<property>}`, where `<property>` is a pom property that pins it (for example `spring-ai-openai=${spring-ai.version}`); define a property for every listed library, even one that is managed by an imported BOM (`<spring-boot.version>4.1.2</spring-boot.version>`, `<quarkus.version>3.40.1</quarkus.version>` for `quarkus-core`, `<quarkus-langchain4j.version>1.15.1</quarkus-langchain4j.version>`). Port the matching spike program from `spikes/m8-files/` and change only:
   - the base URL, key and OTLP endpoint come from `cetra.capture.Capture` (no `sk-fake-m8`, no `OTEL_EXPORTER_OTLP_*` in code: the runner sets `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_PROTOCOL`, `OTEL_SERVICE_NAME`, the sampler and no headers; programs that use OTel autoconfigure read them);
   - the conversation is the mock's turns below, not the spike's weather tool;
   - the base URL convention of detail 8;
   - no `m8` names: tracer, scope, source and service names become `capture` (tracers, activity sources) or are left to the runner's `OTEL_SERVICE_NAME` (remove `setServiceInfo("m8-koog", …)`, `.AddService(…)` and similar); delete every `M7_OUT`/`base.url` read;
   - the tool is declared with the wire name `get_weather` (Spring: `@Tool(name = "get_weather")`, since the spike method is `getWeather`);
   - only the call sequence of step 3: delete any extra call the spike made before the tool round (the .NET spike programs make a first no-tool call);
   - the LangChain4j module keeps only the `oi-model` mode: drop `langchain4j-observation`, the Micrometer bridge and the other modes;
   - after the call, close or flush the tracer provider, then `Capture.writeMeta()`, then exit 0 (Boot and Quarkus: as their bullets below say).
3. **Turns.** Every Java case: one tool `get_weather` with one string parameter `city`; two turns, model `gpt-4o-mini`, API `openai.chat`: turn 1 a tool call `{"id":"call_capture_1","name":"get_weather","arguments":{"city":"Lyon"}}`, finish `tool_calls`, usage 82/17; turn 2 text `It is sunny in Lyon.`, finish `stop`, usage 131/12. The tool returns `Sunny in Lyon, 21 C`. The prompt is `What is the weather in Lyon? Use the tool.` The tool must exist in the program with that name and schema. Semantic Kernel .NET puts every function in a plugin and names it `<Plugin>-<Function>` on the wire, so its case scripts the call as `WeatherPlugin-get_weather` (its spans still say `get_weather`); for any other framework that names the tool differently on the wire, stop and ask.
4. **Cases.**
   - **`genai/spring-ai-boot-tool`**: Spring Boot 4.1.2 (capture once first and read the raw attributes of Boot's HTTP client `POST` spans: if the leak check refuses them or a random port breaks `--check`, set `span_names` to the other span names the capture shows, exactly as written — Spring writes `tool _calling ` with a leading and a trailing space — and say so in `notes`), `spring-ai-starter-model-openai` 2.0.2, `spring-boot-starter-opentelemetry` and `spring-boot-starter-actuator` (as `spikes/m8-files/followup/boot-ai/pom.xml`), `WebApplicationType.NONE`, properties per detail 7, a `ChatClient` call with the tool as a `ToolCallback`. `instrumentation` `spring-ai-openai`; `libraries` `["spring-ai-openai", "spring-boot"]`.
   - **`openinference/spring-ai-tool`**: the same Boot app as `genai/spring-ai-boot-tool`, plus OpenInference's Spring AI handler registered as a bean, plus `spring.autoconfigure.exclude=org.springframework.boot.micrometer.tracing.autoconfigure.MicrometerTracingAutoConfiguration` (so Micrometer's own tracing handler records nothing; `spikes/m8-followup.md` § 9.1), and the `ChatClient` call made inside an application span made current (as `BootRun3`'s `-Dm8.app=true` variant in § 9.1, committed at `spikes/m8-files/followup/boot3/`). `management.tracing.enabled=false` does nothing and `management.tracing.export.enabled=false` stops every span: use neither. `instrumentation` `openinference-instrumentation-springAI`; `libraries` `["openinference-instrumentation-springAI", "spring-ai-openai", "spring-boot"]`.
   - **`openinference/langchain4j-tool`**: `AiServices` with the tool, the OpenInference model listener only (`instrumentor.createModelListener()` on the chat model; no AiService listeners), the OTel SDK from `AutoConfiguredOpenTelemetrySdk` (detail 9's env). From `spikes/m8-files/java/langchain4j/` mode `oi-model`. `instrumentation` `openinference-instrumentation-langchain4j`; `libraries` `["openinference-instrumentation-langchain4j", "langchain4j", "langchain4j-open-ai"]`.
   - **`genai/quarkus-langchain4j-tool`**: from `spikes/m8-files/java/quarkus-lc4j/`, with `@ActivateRequestContext` on `run()`. The spike pom has no property for `quarkus-core` or `quarkus-langchain4j-core`; add them (step 2).
     - `capture.Main` (`@QuarkusMain`) calls, before `Quarkus.run`: `System.setProperty("quarkus.langchain4j.openai.base-url", Capture.mockBaseUrl() + "/v1")`, `System.setProperty("quarkus.otel.exporter.otlp.endpoint", Capture.otlpBaseEndpoint())` (the spike's key, with a base URL: `m8-java.md` § 5.3), `System.setProperty("quarkus.otel.service.name", "capture-app")`. If no span arrives, stop and ask.
     - `application.properties`: `quarkus.langchain4j.openai.api-key=${OPENAI_API_KEY}`, `quarkus.langchain4j.openai.chat-model.model-name=gpt-4o-mini`, `quarkus.otel.exporter.otlp.traces.protocol=http/protobuf`, and `quarkus.langchain4j.tracing.include-prompt`, `include-completion`, `include-tool-arguments`, `include-tool-result` all `true`. Copy the exact key names from the spike file and `spikes/m8-java.md` § 5.3; if one differs from these, use the spike's and say so.
     - `run()` makes the call, then `Capture.writeMeta()`, and returns 0; Quarkus flushes its SDK at shutdown.
     - `instrumentation` `quarkus-langchain4j-core`; `libraries` `["quarkus-langchain4j-core", "quarkus-langchain4j-openai", "quarkus-core"]`.
   - **`genai/koog-agent-tool`**: from `spikes/m8-files/java/koog/`, with `addSpanProcessor { simpleSpanProcessor(...) }` (not `addSpanExporter`), the exporter's endpoint from `Capture.otlpTracesEndpoint()` and no headers, `setVerbose(true)`, the kotlinx-serialization compiler plugin. `instrumentation` `agents-features-opentelemetry-jvm`; `libraries` `["agents-features-opentelemetry-jvm", "koog-agents-jvm"]`. Koog puts random UUIDs in span names: set `volatile_name_prefixes` to `["create_agent ", "invoke_agent "]` (detail 10), plus `volatile_attributes` for any id attribute `--check` shows varying, each with a reason in `notes`.
   - Each `scenario.json`: `description`, `spec_refs` (`["02 § Mapping tables / Input / output"]`, as existing fixtures write it), `instrumentation`, `libraries`, the turns, and `volatile_attributes` / `timing_attributes` / `span_names` / `unordered_spans` only as `--check` shows they are needed, each with a reason in `notes`.
5. **Capture, check and bless:** `just capture <case>`, `just _capture-namespace <case> --check`, `just fixtures-bless`, `fixtures-fmt`, `fixtures-check`.
   - **Expected:**
     - **`genai/spring-ai-boot-tool`:** llm `chat gpt-4o-mini` ×2 (82/17, 131/12), tool `execute_tool get_weather`, Spring AI's framework spans as `span` rows, Boot's HTTP client spans as `span` rows; no input or output on the llm spans.
     - **`openinference/spring-ai-tool`:** the application span with llm `generate` ×2 under it, with input, output and tokens; no `chat` Micrometer spans (no duplicates).
     - **`openinference/langchain4j-tool`:** llm ×2 with model `gpt-4o-mini`, cost computed; no AiService or tool spans (the model listener has none).
     - **`genai/quarkus-langchain4j-tool`:** llm `completion gpt-4o-mini` ×2 with provider `openai` (the alias), input and output from the scoped rule, tool `langchain4j.tools.get_weather`.
     - **`genai/koog-agent-tool`:** agent ×2 (`create_agent`, `invoke_agent`), llm ×2 with input and output, tool with provider unset (the framework name), no normalize errors.
     - All llm spans: provider `openai`, tokens as the turns, cost `computed`, except where a case above says otherwise.
   - Stop if a case differs in kinds, tokens or provider.

**Done when:** the five cases pass `--check`; `cargo nextest run -p cetra-otel -p cetra-capture` pass.

**Commit:** `Capture Spring AI, LangChain4j, Quarkus LangChain4j and Koog`

## IN8.4 — .NET captures

**Goal:** three .NET cases captured against the mock.

**Edit:** as the phase table says.

**Steps.**
1. Port each spike program from `spikes/m8-files/dotnet/{extai,sk,maf}/` into `tools/capture/dotnet/scenarios/genai/{extensions-ai-dotnet-tool,semantic-kernel-dotnet-tool,agent-framework-dotnet-tool}/` as `Capture.csproj` (`net10.0`, the § Versions pins, `RestorePackagesWithLockFile`, the Harness reference, and the spike csproj's `ImplicitUsings`, `Nullable` and `NoWarn` (MEAI001, OPENAI001, SKEXP*) properties, copied from it) and `Program.cs`, with the same changes as IN8.3 step 2: URLs and key from `Capture` (base URL `+ "/v1"`, detail 8), the exporter configured as the spike verified it: `Environment.SetEnvironmentVariable("OTEL_EXPORTER_OTLP_ENDPOINT", Capture.OtlpBaseEndpoint)` before the tracer provider is built, then `AddOtlpExporter()` (OpenTelemetry .NET 1.21.0 ignores the runner's `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`; the base variable works: `spikes/m8-followup.md` § 9.3); if no span arrives, stop and ask; no `.AddService(...)` (the runner's `OTEL_SERVICE_NAME` names the service), the turns and tool of IN8.3 step 3, and after the call `TracerProvider.Dispose()` then `Capture.WriteMeta()`.
   - Content on: `EnableSensitiveData = true` (Extensions.AI, Agent Framework); both AppContext switches (Semantic Kernel), set before the kernel is built.
2. Generate each `packages.lock.json` with an online `dotnet restore` (detail 4's environment, `NUGET_PACKAGES=<nuget>`), then confirm `dotnet restore --locked-mode` passes. Commit the lock files.
3. `instrumentation` and `libraries`: Extensions.AI `Microsoft.Extensions.AI` with `["Microsoft.Extensions.AI", "Microsoft.Extensions.AI.OpenAI", "OpenAI"]` (`OpenAI` is read from the lock file, direct or transitive); Semantic Kernel `Microsoft.SemanticKernel` with `["Microsoft.SemanticKernel", "OpenAI"]`; Agent Framework `Microsoft.Agents.AI` with `["Microsoft.Agents.AI", "Microsoft.Agents.AI.OpenAI", "OpenAI"]`.
4. Capture, check and bless as IN8.3 step 5.
   - **Expected:**
     - **Extensions.AI:** `orchestrate_tools` root (not llm; its summed usage is not counted), llm `chat gpt-4o-mini` ×2 with input and output, tool `execute_tool get_weather`.
     - **Semantic Kernel:** llm ×2 with input and output from the event bodies (IN8.2), tool `execute_tool …` spans; no parent links (SK's own behaviour, `m8-dotnet.md` § 5).
     - **Agent Framework:** agent `invoke_agent …` root (usage not counted; its name carries a random agent id: set `volatile_name_prefixes` to `["invoke_agent "]`, detail 10), llm ×2 with input and output, tool.
     - All: provider `openai`, tokens as the turns, cost `computed`.
   - Stop if a case differs in kinds, tokens or provider.

**Done when:** the three cases pass `--check`; `cargo nextest run -p cetra-otel -p cetra-capture` pass.

**Commit:** `Capture Microsoft.Extensions.AI, Semantic Kernel .NET and Agent Framework .NET`

## IN8.5 — Captured pages

**Goal:** pages for the captured integrations, in the shape of an existing captured framework page (`semantic-kernel.mdx`): `<Backing fixtures={[...]} />`, setup, What you get (from `expected.json`), Limits.

| Page | Setup (from the scenario program, as users write it) | Limits |
|---|---|---|
| `spring-ai` (both Spring AI cases) | Boot properties of detail 7 with cetra's endpoint and `Authorization: Bearer <key>` header (route 1, Micrometer); route 2: the OpenInference handler bean plus the `MicrometerTracingAutoConfiguration` exclusion, and an application span around model calls | never enable both routes (each call twice); sampling must be 1.0 (Boot keeps 10% by default); the `management.otlp.tracing.*` keys export nothing with Boot's OpenTelemetry starter; route 1 has no content (Spring AI 2.x logs it, and cetra drops logs); OpenInference's Spring AI handler hides content only through code (`TraceConfig`); route 2's exclusion turns off Spring's own tracing for the whole app (HTTP requests included), so without an application span each model call is its own trace |
| `langchain4j` | OpenInference's model listener on the chat model | the AiService listeners are not used: their model spans say `unknown` and both kinds together record each call twice; no agent or tool spans |
| `quarkus-langchain4j` | `quarkus-opentelemetry` OTLP settings and the four `include-*` switches | content is off by default; the HTTP client spans are kept as plain spans |
| `koog` | the OpenTelemetry feature with a synchronous span processor and `setVerbose(true)` | without verbose, content is masked (`HIDDEN:`) and not captured; the batch exporter sent nothing before exit in our runs |
| `extensions-ai` | `UseOpenTelemetry()` with `EnableSensitiveData` | content is off by default |
| `semantic-kernel` (.NET section) | the two AppContext switches; the OTLP exporter on its activity sources | without the switches, no model-call spans; spans have no parent links; finish reasons and the URL-shaped `server.address` are not read |
| `agent-framework` (.NET section) | `UseOpenTelemetry()` on the agent; content by `EnableSensitiveData` or `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true` | content is off by default |

Fences in Java, Kotlin, C#, XML, properties, Go, Ruby and Rust need no marker: `check-samples.mjs` covers only Python, TypeScript and JavaScript fences (`SAMPLE_LANGUAGES`, line 8). The endpoint and key placeholders are those of `website/src/content/docs/docs/get-started/opentelemetry.mdx` (`sk-cetra-…` keys only). On `semantic-kernel.mdx` and `agent-framework.mdx`, which use `<Tabs syncKey="lang">`, the .NET content is a `<TabItem label=".NET">` beside Python in each tab group; add `genai/semantic-kernel-dotnet-tool` / `genai/agent-framework-dotnet-tool` to each page's `<Backing fixtures>`; change the frontmatter description and intro that say "from Python" to name both; the Limits list becomes two sub-lists headed "Python" and ".NET" (Semantic Kernel's log-record bullet stays under Python).

**Done when:** `cd website && node scripts/check-terms.mjs && node scripts/check-samples.mjs && node scripts/check-backing.mjs` pass; `check-integrations.mjs` passes or fails only for pages not yet in the index.

**Commit:** `Add Java and .NET framework pages`

## IN8.6 — Setup-only pages

**Goal:** setup-only pages in the shape of `cleanlab.mdx`, from `spikes/m8-setup-only.md` and `m8-followup.md` § 3. Every code sample is copied from the spike's source-read setup code or the spike's working program (`spikes/m8-files/followup/go-openai/main.go`), with cetra's endpoint and key placeholders; never a call the spikes do not show.

| Page | Content |
|---|---|
| `genkit-go` | Genkit Go v1.14.0's `otelgenai` plugin (versions as `spikes/m8-setup-only.md` § Go lists them), opt-in with `tracing.SetInstrumentation`; OTLP/HTTP exporter (`otlptracehttp` v1.46.0) with the Bearer header; content off by default and its switch; only the span-attribute content mode (cetra drops log records) |
| `openai-go` | openai-go v3.100.0 with OpenInference Go's middleware v0.1.2 and Go OTel 1.47.0, as `spikes/m8-files/followup/go-openai/go.mod` pins them (`m8-followup.md` § 3); content on by default, `OPENINFERENCE_HIDE_INPUTS` / `OPENINFERENCE_HIDE_OUTPUTS`; spans are roots unless the app has a span open |
| `ruby-llm` | ruby_llm 2.2.2's built-in `RubyLLM::OpenTelemetry` (`enable(content:)`), with `opentelemetry-sdk` 1.13.1 and `opentelemetry-exporter-otlp` 0.37.0; content off by default; do not also enable thoughtbot's `opentelemetry-instrumentation-ruby_llm` (each call twice) |
| `rig` | rig-core 0.46.0's GenAI `tracing` spans, exported with `tracing-opentelemetry` 0.34.0 and `opentelemetry-otlp` 0.33.1; content per request with `record_content_telemetry(true)` |
| `arconia` | from `spikes/m8-followup.md` § 9.5 only: `io.arconia:arconia-opentelemetry-spring-boot-starter` and `io.arconia:arconia-openinference-ai-semantic-conventions` 0.32.0; `arconia.otel.exporter.otlp.endpoint`, `.headers`, `.protocol`; content on by default, `OPENINFERENCE_HIDE_*`. Limits: not run; Arconia's OpenInference conventions replace Spring AI's attributes on the same spans (read from its jar, not run); do not combine it with OpenInference's own Spring AI handler |

Each page's Limits: "not captured: written from the library's source and documentation".

**Done when:** as IN8.5.

**Commit:** `Add Genkit Go, openai-go, ruby_llm, rig and Arconia pages`

## IN8.7 — Index and records

**Goal:** list the new pages and record D-IN-30.

**Steps.**
1. `index.mdx`, within its strict row schema (`website/src/marketing/data/integrations.mjs` ~26–30: two language columns, Python and TypeScript, then How, then the tier; captured rows need a `<LinkCard>` in the section's CardGrid, setup-only rows must have none):
   - the five captured pages (`spring-ai`, `langchain4j`, `quarkus-langchain4j`, `koog`, `extensions-ai`) go in the existing "Frameworks" section, with `— | —` in the two language columns, the language named in the How cell (for example "Java, Spring Boot with Micrometer or OpenInference"), the tier `captured`, and a `<LinkCard>` each;
   - the five setup-only pages (`genkit-go`, `openai-go`, `ruby-llm`, `rig`, `arconia`) all go in "Frameworks", after the captured Java and .NET rows, in that order; `— | —`, the language in How, tier `setup only`, no card;
   - the Semantic Kernel and Agent Framework rows are unchanged;
   - the paragraph after the tables that defines the tiers (Captured, Hand-written, SDK test, Setup only; ~line 242) gains: "A dash in both language columns means the page is for another language, named in the How column (Java, .NET, Go, Ruby, Rust)."
   Sidebar entries in the same order, under Frameworks.
2. `00-design.md`: the M8 milestone line, "done".
3. `mvp-decisions.md`: D-IN-30 in the Track IN section, copied from `00-design.md`'s D-IN-30 row (after D-IN-29, which M7 part 2's IN7.15 adds).
4. CHANGELOG `Added`: Java and .NET captures and pages, the setup-only pages; `Changed`: the normalizer rules of D-IN-30 item 4.
5. Roadmap: "M8 implemented" and D-IN-30.
6. `just docs-gen`.

**Done when:** `just check-docs` passes.

**Commit:** `List the Java, .NET and setup-only language pages and record D-IN-30`

## After the last phase

`just check`, a Sonnet code review, fixes, then a push to PR #242.
