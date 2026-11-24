# Track IN, M5: gateways — plan

Pages for every gateway in the design's § Gateways that a spike kept, and captured cases for the ones that run locally:
- LiteLLM Proxy (pip);
- Bifrost, Envoy AI Gateway (`aigw` with Envoy), and the Temporal dev server that M3 left setup-only. These three are binaries the user approved on 2026-11-23.

The normalizer learns:
- LiteLLM's cost keys;
- embedding operation names;
- Bifrost's root span kind, flattened tool calls and cost split;
- LiteLLM's tool-call output.

The capture tool learns to run pinned binaries.

Implementer: the GLM 5.3 agent, one agent per phase. Conventions as in M4:
- Node on `PATH`;
- `CARGO_TARGET_DIR=/home/sebastien/travaux/llm-observer/target`;
- `git commit -m … -- <paths>`;
- `touch tools/capture/src/main.rs` before the first capture;
- never `--run-ignored`.

**Read first:**
- `docs/plans/in-integrations/00-design.md`: § Gateways, D-IN-2, D-IN-13, D-IN-16.
- `spikes/m5-gateways.md`: LiteLLM's real run, per-gateway verdicts, proposed rules 1–8.
- `spikes/m5-binaries.md`: Bifrost, aigw and Temporal runs; rules B1–B3; § Capture-tool changes; § Page notes; the download table (URL, size, SHA-256).
- `spikes/m5-files/`: the spike's working templates (below).
- `docs/spec/02-ingestion.md`.
- `tools/capture/src/{mock,run,scenario}.rs` and `tools/capture/py/src/capture_py/harness.py`.

**The spike's templates** are in `docs/plans/in-integrations/spikes/m5-files/`. They are reference code, not importable modules: adapt them to the harness in the scenario directories, and never point anything at `/tmp`.

| File | What it is |
|---|---|
| `litellm/config.yaml` | the proxy's `model_list`, `litellm_settings` and `general_settings` (ten models; the capture keeps `gpt-4o-mini`, `ds` and the embedding model, which the capture names `text-embedding-3-small` where the template says `emb`) |
| `litellm/run.py` | the driver: fake upstream, receiver, the `litellm` command, the requests, the 8 s wait, SIGINT. Do not carry over `PYTHONPATH=hook`, `EXT_LOG` or the `.venv/bin/litellm` path (spike-only DNS blocking and layout). Its `%20` in `OTEL_HEADERS` is fixed to a literal space |
| `bifrost/config.json` | the config with `@APP_DIR@`, `@MOCK_URL@` and `@OTLP_ENDPOINT@` placeholders |
| `bifrost/{pricing,model-parameters}.json` | the vendor's local pricing files (copy both into the scenario directory) |
| `bifrost/run.py` | the driver (ports, app dir, readiness, request, stop) |
| `aigw/aigw.tmpl.yaml` | the Gateway API config with `@LISTEN@` and `@FAKE@` (the mock's port) |
| `aigw/run.py` | the driver (env, readiness, request, stop) |
| `temporal/{run.py,wfmod.py,pyproject.toml}` | the driver, the workflow module and the spike's project pins |
| `common.py` | the spike's fake provider, receiver and `wait_http`/`stop` helpers (used by the three drivers). `Fake`, `Recv`, `serve` and `REQLOG` are spike-only and are not carried over: the runner provides the mock and receiver. Scenarios use the harness helpers of IN5.1 |

The `master_key` in `litellm/config.yaml` and the keys in the drivers are test values, not secrets.

## What it is, and what it is not

When it is done:
- LiteLLM Proxy, Bifrost, Envoy AI Gateway and Temporal have captured cases;
- every gateway the spikes kept has a page;
- the index has a Gateways section;
- `just capture-tools` fetches the pinned binaries, with their checksums.

Not in M5:
- **Kong.** It needs Docker, which this machine lacks. Its page is setup only (the user, 2026-11-24).
- **Azure API Management.** It has no OTLP trace export. It goes to § Not supported.
- **Pydantic AI Gateway.** It exports to Logfire only. The Pydantic AI page gets one sentence; no page of its own.
- **Databricks AI Gateway.** One sentence on the existing Databricks page.
- **Gravitee.** Its spans are plain HTTP spans. It goes to § Not supported.
- **OrcaRouter and Anannas AI.** No page and no host row: each is one line in the design's appendix, as the spike recommends.
- **Cloudflare AI Gateway's OTLP export.** No recorded export exists. The page documents the OpenAI-client route and lists the export's gaps.
- **The Vercel AI Gateway trace drain.** It is SaaS, needs a paid plan, and carries no content. The page documents the OpenAI-client route and mentions the drain, setup only.
- **New hosts for Anannas, OrcaRouter or TrueFoundry.** The spike advises against them.
- **Envoy's request model, rule E1.** The page notes that the model column shows the response model.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-IN-13 | Gateway pricing. LiteLLM Proxy writes the upstream provider, so it needs no `litellm_proxy` entry. |
| D-IN-16 | Gateway-reported cost: `gen_ai.cost.total_cost`, `gen_ai.cost.input_cost` and `gen_ai.cost.output_cost` (LiteLLM, captured), plus Bifrost's `bifrost.cost.input` and `bifrost.cost.output`. IN5.5 amends it. |
| D-IN-25 (new, recorded by IN5.5) | M5's decisions, listed below. |

D-IN-25 records these decisions:
1. **Pinned binaries.** The user approved Temporal CLI 1.10.1, Bifrost 2.5.1, aigw 1.2.1 and Envoy 1.39.1 (2026-11-23 and 2026-11-24). Temporal, aigw and Envoy come from the vendors' GitHub releases; Bifrost comes from `downloads.getmaxim.ai` (the host Bifrost's own `npx` launcher uses; its GitHub releases carry no binaries). All four are pinned by SHA-256 and fetched by `just capture-tools` into `target/capture-tools/`. Envoy is fetched from `envoyproxy/envoy`, never by `aigw` at run time.
2. **Kong** is setup only, because there is no Docker.
3. **Bifrost's server root span** (scope `bifrost`, no `gen_ai.operation.name`) is `chain`.
4. **A `tool_calls` element with `name`/`args` and no `function` object** is a tool call (Bifrost).
5. **LiteLLM's empty assistant output with `gen_ai.completion.0.function_call.*`** becomes a tool-call output.
6. **`gen_ai.operation.name` `embedding` and `aembedding`** map to `embedding`.
7. **Gateway pages warn that an instrumented app behind a traced gateway counts each call twice,** because both honour `traceparent`.
8. **Azure API Management and Gravitee** go to § Not supported; **Pydantic AI Gateway and Databricks AI Gateway** are merged into their providers' pages.
9. **Deviations from the spike's verdicts.**
   - OrcaRouter and Anannas AI: the spike suggested setup-only OpenAI-client pages in step (4) of each entry but recommended appendix lines; the plan follows the recommendation. No page, one appendix line each.
   - Gravitee: the spike's summary table offered an OpenAI-client page. It goes to § Not supported instead: its spans are HTTP spans without LLM attributes, so there is nothing for a page to show. An OpenAI-compatible endpoint behind Gravitee is traced by the app's own instrumentation, as on the OpenAI page.

## Versions

| Item | Version | Source |
|---|---|---|
| `litellm[proxy]` (capture project) | 1.109.1, the version the spike ran (check the 7-day rule with `uv lock`) | PyPI |
| `temporalio`, `temporalio-openai-agents[openinference]` | 1.36.1, 2.0.0 | PyPI |
| Temporal CLI | 1.10.1 | `temporalio/cli` GitHub release |
| Bifrost | 2.5.1 | `https://downloads.getmaxim.ai/bifrost/v2.5.1/linux/amd64/bifrost-http` (GitHub releases of `maximhq/bifrost` carry no binaries) |
| aigw | 1.2.1 | `envoyproxy/ai-gateway` GitHub release (now redirected to `theagentrouter/agent-router`; `curl -L` follows it) |
| Envoy | 1.39.1 | `envoyproxy/envoy` GitHub release |

The exact URLs and SHA-256 are in IN5.1 step 4 (copied from `spikes/m5-binaries.md`). The implementer re-checks each against the spike and reports any difference.

`temporalio-openai-agents` 2.0.0 was uploaded 2026-11-17T21:07Z. If `uv lock` refuses it, the run is before 2026-11-24T21:07Z: wait, or add an `exclude-newer-package` line for that one package, as the project already does for google-adk. Report which you did.

## Phases and dependencies

| Phase | Edits | Depends on |
|---|---|---|
| IN5.1 Capture tool for binaries | `tools/capture/src/{mock,run}.rs`, `tools/capture/py/src/capture_py/harness.py`, `tools/capture/fetch-tools.sh` (new), `justfile`, `AGENTS.md` (§ Required tools) | — |
| IN5.2 Normalizer rules | `crates/otel/src/{kind,cost}.rs`, `crates/otel/src/conventions/genai.rs`, `crates/otel/src/convert/openai.rs`, `docs/spec/02-ingestion.md` | — |
| IN5.3 Captures | `tools/capture/py/{pyproject.toml,uv.lock}`, scenario directories, fixture directories | IN5.1, IN5.2 |
| IN5.4 Pages | gateway pages, one sentence each in `temporal.mdx`, `databricks.mdx`, `pydantic-ai.mdx`, the reference and concept pages, generated skill references | IN5.3 |
| IN5.5 Index, sidebar, records | `index.mdx`, `astro.config.mjs`, `integrations.test.mjs`, `00-design.md`, `docs/mvp-decisions.md`, `CHANGELOG.md`, `docs/roadmap.md` | IN5.4 |

IN5.1 and IN5.2 touch disjoint files. They run one after the other in this worktree, in either order.

---

## IN5.1 — Capture tool for binaries

**Goal:** a scenario can start a pinned binary and record it, as `spikes/m5-binaries.md` § Capture-tool changes describes.

**Edit:**
- `tools/capture/src/mock.rs`;
- `tools/capture/src/run.rs` (`lib.rs` line 21 re-exports `program_env`; the re-export does not change, so `lib.rs` is not edited);
- `tools/capture/py/src/capture_py/harness.py`;
- `tools/capture/fetch-tools.sh` (new);
- `justfile`;
- `AGENTS.md` (§ Required tools).

**Steps.**

1. **`mock.rs`: `GET .../models`.**
   - In `Mock::respond` (`mock.rs` ~119–130), before the `classify` call, answer a request whose method is `GET` and whose path ends in `/models` with status 200 and the body `{"object":"list","data":[]}` (`content-type: application/json`).
   - It takes no turn and pushes no error, as `HEAD /api/hello` does (`hello`, `mock.rs` ~114). It is not a router route: the prefix varies (Bifrost calls `GET /v1/models`).
   - Any other `GET`, and a `POST` to `/models`, still goes through `classify` and the "no route" error.
   - Unit tests, next to the `HEAD /api/hello` test (`mock.rs` ~933): `get_models_is_answered_without_a_turn` (status 200, the body, the mock's error list stays empty, a scripted turn is still available afterwards) and `post_models_is_still_an_unknown_route`.

2. **`run.rs`: the program's process group.** No `unsafe`, no `libc`, no `nix`.
   - In `spawn_program` (`run.rs` ~360), split the builder chain: `let mut command = Command::new(bin);` then `command.args(args).current_dir(dir).kill_on_drop(true).env_clear().envs(env).stdout(…).stderr(…);`, then `#[cfg(unix)] command.process_group(0);`, then `command.spawn().map_err(…)` as today. `process_group` is an inherent method of tokio's `Command` on Unix (tokio 1.54.1 is locked). `kill_on_drop(true)` stays: it kills only the child, not the processes the child started.
   - Add, `#[cfg(unix)]`:
     ```rust
     /// Sends SIGTERM to every process of the group whose leader is `pid`. Errors are ignored:
     /// the group may be gone already.
     fn kill_process_group(pid: u32) {
         let _ = std::process::Command::new("kill")
             .args(["-TERM", "--", &format!("-{pid}")])
             .stdout(std::process::Stdio::null())
             .stderr(std::process::Stdio::null())
             .status();
     }

     /// Kills the program's whole group when dropped, so a gateway the scenario started under
     /// `uv run` does not outlive the runner (`kill_on_drop` kills only the child).
     struct ProcessGroupGuard {
         pid: u32,
     }

     impl Drop for ProcessGroupGuard {
         fn drop(&mut self) {
             kill_process_group(self.pid);
         }
     }
     ```
   - In `run_case` (`run.rs` ~176), right after `spawn_program` (~191), build the guard from `child.id()` (`Some` until the child is reaped; skip the guard when it is `None`) and keep it alive until the function returns. The guard also cleans up leftovers on the success path (a gateway the scenario failed to stop). Errors from `kill` are ignored, so a group that is already gone is fine.
   - In `wait_program` (`run.rs` ~424), in the timeout arm, call `kill_process_group(pid)` (with `child.id()`, read before the kill) before `child.kill().await` and `child.wait().await`.

3. **`run.rs`: `program_env` gets the tools directory.**
   - New signature: `pub fn program_env(parent: &HashMap<String, String>, scenario: &Scenario, repo_root: &Path, mock_port: u16, receiver_port: u16, meta_path: &str) -> BTreeMap<String, String>`. It is `pub` and re-exported in `lib.rs` (line 21); its only caller is `run_case` (`run.rs` ~191), which already has `repo_root`. Update that call.
   - Insert `CETRA_CAPTURE_TOOLS` = `<repo_root>/target/capture-tools` (a `String` from `repo_root.join("target/capture-tools")`; `repo_root` is UTF-8 as `meta_path` is: use `utf8_path`-style handling, and keep the function infallible by `to_string_lossy`). Insert it with the other fixed `CETRA_CAPTURE_*` variables, before `env.extend(scenario.env.clone())`.
   - Update the doc comment ("`CETRA_CAPTURE_TOOLS` names the pinned binaries' directory") and the test `program_env_drops_parent_secrets_and_applies_scenario_env` (`run.rs` ~1124–1140): pass `Path::new("/repo")` and assert `CETRA_CAPTURE_TOOLS` is `/repo/target/capture-tools`.
   - There is no "inject a timeout" alternative.
   - Raise `PROGRAM_TIMEOUT` (`run.rs` line 27) from 120 s to 180 s: the gateways start in 1 to 8 s and the LiteLLM proxy needs up to 60 s, plus the 8 s wait.

4. **`run.rs`: tests** (`#[cfg(unix)]`, in the existing `tests` module):
   - `kill_process_group_stops_grandchildren`: spawn `sh -c "sleep 60 & echo $!; wait"` with `tokio::process::Command`, `.process_group(0)`, piped stdout, and read the first line, the grandchild's pid. Call `kill_process_group(child.id())`. Assert `child.wait()` returns within 5 s (use `tokio::time::timeout`). Then assert the grandchild is gone, polling for up to 5 s: `/proc/<pid>` does not exist, or its `stat` file shows state `Z`. The `/proc` check is Linux only: put it under `#[cfg(target_os = "linux")]`.
   - `process_group_guard_kills_the_group_on_drop`: the same setup, drop a `ProcessGroupGuard { pid }`, then the same two assertions.

5. **`harness.py`:**
   - Factor the existing `finish` into three functions (read the current `finish`, `harness.py` ~123–134):
     - `importlib_versions(names: list[str]) -> dict[str, str]`: `{name: importlib.metadata.version(name) for name in names if name}`.
     - `_write_meta(libraries: dict[str, str]) -> None`: reads `CETRA_CAPTURE_META` (a missing variable raises `KeyError` naming it) and writes `{"libraries": libraries}`.
     - `finish(provider)`: flush, shut down, then `_write_meta(importlib_versions(os.environ["CETRA_CAPTURE_LIBRARIES"].split(",")))`. Its behaviour does not change.
   - Add `finish_tool(provider: TracerProvider | None, versions: dict[str, str]) -> None`: with a provider, `force_flush()` then `shutdown()`; with `None`, no flush; then `_write_meta(versions)`. It writes exactly `versions`. Every entry of the scenario's `libraries` must be a key of `versions` (the runner requires it, `run.rs` `read_libraries` ~487).
   - Add the gateway helpers (the spike's `common.py` equivalents; add `import socket, subprocess, signal, time, urllib.request, urllib.error` and `from collections.abc import Callable` as needed):
     - `free_port() -> int`: bind a socket to `127.0.0.1:0`, read the port, close it, return it.
     - `wait_http(url: str, timeout: float, ok: Callable[[int], bool] = lambda status: status == 200) -> None`: poll `GET url` every 0.25 s; an HTTP error status counts as a status (`urllib.error.HTTPError.code`); connection errors are retried; return when `ok(status)` is true; raise `TimeoutError` naming the URL after `timeout` seconds.
     - `stop_gateway(proc: subprocess.Popen[bytes], wait: float) -> None`: `proc.send_signal(signal.SIGINT)`, `proc.wait(timeout=wait)`, and on `subprocess.TimeoutExpired` `proc.kill()` then `proc.wait()`.
   - **Tests.** `tools/capture/py` has no test directory, so these helpers have no unit tests; `ruff` and `mypy` check them, and the IN5.3 captures exercise them.
   - Add `tool_binary(*parts: str) -> str`: `os.path.join(os.environ["CETRA_CAPTURE_TOOLS"], *parts)`. When the file does not exist it raises `FileNotFoundError` with a message that names the path and says `run `just capture-tools` (Linux x86_64 only)`. A scenario that needs a binary calls it first, so a host without the binaries fails with that message.

6. **`tools/capture/fetch-tools.sh`** (bash, `set -euo pipefail`; run from the repo root, resolving it from the script's own path):
   - **Layout.** `target/capture-tools/` holds exactly these files. The three non-archives are downloaded and renamed; Temporal is extracted from its tarball.

     | Final path | Download | SHA-256 (of the download) |
     |---|---|---|
     | `temporal/1.10.1/temporal` | `https://github.com/temporalio/cli/releases/download/v1.10.1/temporal_cli_1.10.1_linux_amd64.tar.gz` (38,357,804 bytes); the member `temporal` at the tarball root (the spike's extraction held `LICENSE` and `temporal`; run `tar tzf` and report if the layout differs) | `4c1500e2ce2d4de136e5fe8852d9b7f82eb62f19b6b383b3e0c040519a9d20b1` |
     | `bifrost/2.5.1/bifrost-http` | `https://downloads.getmaxim.ai/bifrost/v2.5.1/linux/amd64/bifrost-http` (152,623,104 bytes) | `2b629808bdd54dca5abb38f0e62f6250e7fbd8c336c45d568d83d683974c0734` |
     | `aigw/1.2.1/aigw` | `https://github.com/envoyproxy/ai-gateway/releases/download/v1.2.1/aigw-linux-amd64` (296,261,464 bytes; GitHub redirects, so `curl -L`) | `b97af14fc3f116ccbdce7cdb7ae98da999f41623285bbf253095f09d214523d7` |
     | `envoy/1.39.1/envoy` | `https://github.com/envoyproxy/envoy/releases/download/v1.39.1/envoy-1.39.1-linux-x86_64` (105,039,832 bytes) | `002c6e1c69ed0fa0ea381887247cadadfaec9481375fa8d8d2b1731eeabf40b8` |

     The sizes are for the report only; the script does not check them.
   - **Complete** means the final binary exists and is executable. If it does, skip that tool.
   - **Otherwise:** `mkdir -p` the final directory; download with `curl -fsSL` to a temporary file in that same directory (a `trap` removes it on any exit); check it with `echo "<sha>  <file>" | sha256sum -c -` and fail on a mismatch; for Temporal, `tar -xzf <tarball> -C <temporary directory in the same directory> temporal`; `chmod +x`; `mv` the binary to its final name last, so a partial run never leaves a file that looks complete.
   - **Platform:** Linux x86_64 only (`uname -s`, `uname -m`). Elsewhere it prints that the binaries are not available on this platform and exits 0.
   - Scenarios use exactly these paths, through `CETRA_CAPTURE_TOOLS`.

7. **`justfile`:**
   - New recipe `capture-tools` running `bash tools/capture/fetch-tools.sh`, with a comment line above it (the justfile's style).
   - `capture` (`+cases: capture-py-sync capture-ts-sync`), `capture-all` and `check-capture` each gain `capture-tools` as a dependency. The binary scenarios need Linux x86_64: elsewhere `capture-tools` exits 0, and `check-capture` and `capture-all` then fail on those cases with the message naming `just capture-tools`. `check-capture` is not in CI.

8. **`AGENTS.md` § Required tools:** add "for captures, `just capture-tools` downloads Temporal CLI, Bifrost, aigw and Envoy into `target/capture-tools` (pinned, checksummed)".

**Done when:**

```bash
cargo nextest run -p cetra-capture
cargo clippy -p cetra-capture --all-targets -- -D warnings
bash -n tools/capture/fetch-tools.sh
just capture-tools
cd tools/capture/py && uv run ruff format --check && uv run ruff check && uv run mypy
```

`just capture-tools` succeeds twice in a row (the second run downloads nothing). Report the four binaries' paths and sizes.

**Commit:** `Let capture scenarios run pinned gateway binaries`

## IN5.2 — Normalizer rules

**Goal:** the rules of D-IN-16 and D-IN-25 items 3–6, each with unit tests. Spec 02 is edited in the same commit.

**Edit:**
- `crates/otel/src/cost.rs`;
- `crates/otel/src/kind.rs`;
- `crates/otel/src/conventions/genai.rs`;
- `crates/otel/src/convert/openai.rs`;
- `docs/spec/02-ingestion.md`.

`conventions/mod.rs` is not edited: the new keys need no registry entry (step 5).

**Steps.**

1. **`cost.rs`, reported cost** (`cost.rs` ~22–29):
   - `TOTAL_KEYS` gains `gen_ai.cost.total_cost`.
   - `INPUT_KEYS` gains `gen_ai.cost.input_cost` and `bifrost.cost.input`.
   - `OUTPUT_KEYS` gains `gen_ai.cost.output_cost` and `bifrost.cost.output`.
   - Each key goes last in its list.
   - **Spec 02 § Cost computation, step 1** (line 418): add the keys to the three sentences ("`gen_ai.cost.total_cost` fill `cost_total`", "`gen_ai.cost.input_cost` and `bifrost.cost.input` fill `cost_input`", the same for output). Add: LiteLLM's `input_cost` and `output_cost` are the raw token costs, before its discount and margin, so the parts need not add up to the total; the total is the reported total (D-IN-16).

2. **`kind.rs`, `gen_ai.operation.name` row** (`genai_operation`, `kind.rs` ~108): `"embedding" | "aembedding"` join `"embeddings"` → `SpanKind::Embedding`.
   - **Spec 02 § Span kind, the `gen_ai.operation.name` row** (line 85): `embeddings`, `embedding`, `aembedding` → `embedding`.

3. **`kind.rs`, Bifrost** (`scope_gated`, `kind.rs` ~77; it runs before the model fallback, so the row decides first):
   - In `scope_gated`, before the Mirascope prefix test: when `scope_name == "bifrost"`, the span has no `gen_ai.operation.name` (`!present(bag, "gen_ai.operation.name")`), and it has `gen_ai.request.model` or `gen_ai.response.model` (`present`, peeked, not consumed), return `Some(SpanKind::Chain)`. Otherwise it decides nothing, so the existing rules still apply (Bifrost's 24 `plugin.*` spans have no model and stay `span`; its `chat` span has `gen_ai.operation.name = chat` and is decided earlier as `llm`).
   - The scope name is the instrumentation scope's `name`: the spike printed `bifrost@v2.5.1`, name `bifrost` and version `v2.5.1`. The golden fixture of IN5.3 proves it: if the root span is not `chain` there, stop and report the scope name.
   - Update the doc comment above `scope_gated`.
   - The spec's row order (Bifrost after Mirascope) and the code's order (the Bifrost test before the Mirascope prefix test) differ on purpose and do not matter: the scopes are disjoint.
   - **Spec 02 § Span kind:** a new row after the `mirascope.type` row (line 96) and before `model fallback` (line 97): "Bifrost (scope `bifrost`) | On a span whose instrumentation scope name is `bifrost` and that has no `gen_ai.operation.name`: `gen_ai.request.model` or `gen_ai.response.model` present → `chain` (Bifrost's server root span repeats its `chat` child's model and content, D-IN-25). Any other span, and any other scope, does not decide the kind".

4. **The OpenAI chat converter** (`convert/openai.rs`; spec 02 § Input / output, the GenAI row (line 108) and the "Message conversion" paragraph (line 120) name it):
   - **`tool_call`** (`openai.rs` ~254): today a `tool_calls` element without a `function.name` returns `Part::Unknown(Json::Object(map))` (the `let Some(name) = … else` at ~259–264). Before that, add a branch: when the element has no `function` key and has a string `name`, it is a flattened call (Bifrost). Build it as `Part::ToolCall { id, name, arguments, extra }`:
     - `id: Option<String>` from a string `id` (removed from the map only when it is a string, as the existing code does);
     - `name: String` from `name`;
     - `arguments: Option<Json>` from `args`, run through `parse_tool_arguments` (`payload.rs` ~147, already imported at `openai.rs` line 9), exactly as `function.arguments` is; `None` when there is no `args`;
     - `extra: Map<String, Json>`: the element's other keys, after removing `id` (when string), `type`, `name` and `args`.
   - **The existing path is unchanged.** The test at `openai.rs` ~461 (`a_part_that_cannot_be_modelled_stays_on_the_message`: `{"id": "call_1", "function": {"arguments": "{}"}}`, a `function` object without `name`) must still pass: the new branch requires no `function` key.
   - **`message`** (`openai.rs` ~47): `content: ""` (an empty string) next to a non-empty `tool_calls` array adds no empty text part. In the `else if let Some(content) = extra.shift_remove("content")` branch, skip an empty `Json::String` when `extra.get("tool_calls")` is a non-empty array. An empty string without tool calls keeps producing the empty text part it produces today.
   - **Spec 02:** in the GenAI row of § Input / output (line 108) and the "Message conversion" paragraph (line 120): a `tool_calls` element with `name` and `args` and no `function` object is a tool call (Bifrost, D-IN-25); `content: ""` beside tool calls adds no text part.
   - No existing `expected.json` may change. If one does, stop and report.

5. **LiteLLM tool-call output** (`conventions/genai.rs`, `read`, ~56). LiteLLM writes `gen_ai.output.messages` as `[{"role":"assistant","parts":[],"finish_reason":"tool_calls"}]` and the call only in `gen_ai.completion.0.function_call.name` and `.arguments` (strings).
   - **Why the keys are readable here.** `detect.rs` also classifies these spans as OpenLLMetry (`legacy_numbered`, `detect.rs` ~114), so the OpenLLMetry reader runs and its `IndexedPrefix("gen_ai.completion.")` pattern (`conventions/openllmetry.rs` ~26) owns these keys. But readers all run first (`payload.rs` ~233–260); `drop_lower_payloads` (`payload.rs` ~276) only runs afterwards, and marks lower-ranked conventions' keys `Payload`. GenAI outranks OpenLLMetry (`rank`, `payload.rs` ~399), so the GenAI reader's output wins and the keys are still in the bag when `genai::read` runs. Read them with `ctx.bag.get` there.
   - **Where.** In `read`, right after the second `read_messages` call (`OUTPUT_MESSAGES`, ~67–74) and before `io.system_instructions` is set: call a new private function `litellm_tool_call(ctx, &mut io)`.
   - **Condition.** `io.output` is `Some(Payload::Messages(messages))` with exactly one message, whose `role` is `Role::Assistant` and whose `parts` are empty, and `ctx.bag.get("gen_ai.completion.0.function_call.name")` is a non-empty string (`as_str`).
   - **Action.** Push one part on that message: `Part::ToolCall { id: None, name, arguments, extra: Map::new() }`, where `arguments` is `Some(parse_tool_arguments(Json::String(text)))` from `gen_ai.completion.0.function_call.arguments` when it is a string, else `None`. The message keeps its `finish_reason`. Then mark both keys `ctx.bag.take_payload(..)` (a missing arguments key is ignored by `take_payload`), so they never reach `attributes`, whatever the detected conventions are.
   - **No `PAYLOAD_KEYS` entry.** `PAYLOAD_KEYS` lists the keys a lower convention loses when a higher one wins. These keys are already covered by OpenLLMetry's `IndexedPrefix`; adding them to GenAI's list would only drop them when `cetra` wins, which no span does. The function marks them itself.
   - Import `Part` from `cetra_core::message` (the file imports `Message, Role` at line 7) and `parse_tool_arguments` from `crate::payload`.
   - **Spec 02:** in the GenAI row of § Input / output (line 108), output column: when the output message has no parts and the span has `gen_ai.completion.0.function_call.name`, that key and `.arguments` become one tool call (LiteLLM, D-IN-25).

Read `spikes/m5-gateways.md` § LiteLLM Proxy (the `litellm_request` dump) for the exact key spelling. If a key or value shape differs from this step, stop and report.

**Tests:**
- `cost.rs`: each new cost key alone (`gen_ai.cost.total_cost`, `gen_ai.cost.input_cost`, `gen_ai.cost.output_cost`, `bifrost.cost.input`, `bifrost.cost.output`); the margin case: `gen_ai.cost.total_cost` differs from the price table's figure, and the reported total wins with `cost_source = reported`; `cost_input + cost_output` need not equal `cost_total`.
- `kind.rs`: `embedding` and `aembedding` give `embedding` (extend `gen_ai_operation_name_uses_the_semconv_table`, `kind.rs` ~378); the Bifrost root span in scope `bifrost` with `gen_ai.request.model` and no operation is `chain`, and the keys are not consumed; the same attributes in another scope are `llm` (model fallback); in scope `bifrost` with only `gen_ai.response.model` it is `chain`; in scope `bifrost` without a model it is `span`; a Bifrost `chat` span (`gen_ai.operation.name = chat`) stays `llm`.
- `openai.rs`: the flattened tool call (`id`, `name`, `args` as a JSON string) gives `ToolCall` with `arguments` parsed and no `extra`; the same with an extra key keeps it in `extra`; `args` absent gives `arguments: None`; `content: ""` beside `tool_calls` adds no text part; `content: ""` without tool calls still adds the empty text part; the existing `a_part_that_cannot_be_modelled_stays_on_the_message` still passes.
- `genai.rs`, a span carrying both conventions: attributes `gen_ai.output.messages` (the LiteLLM JSON above), `gen_ai.completion.0.function_call.name` and `.arguments`, read through a new test helper `read_span_with_bag(attrs) -> (Io, AttrBag<'_>)` (as `openllmetry.rs` ~183 does; import `crate::bag::{AttrBag, Use}` in the test module): the output is one assistant message with one `ToolCall` and `finish_reason` `tool_calls`; the two keys are in `Use::Payload` afterwards (`bag.state`); and a case with a non-empty `parts` is left alone, with the keys untouched.

**Done when:** `cargo nextest run -p cetra-otel`, `cargo clippy -p cetra-otel --all-targets -- -D warnings` and `cargo fmt --check` pass, with no `expected.json` changed.

**Commit:** `Read gateway cost keys, embedding operations and Bifrost spans`

## IN5.3 — Captures

**Goal:** captured cases for LiteLLM Proxy, Bifrost, aigw and Temporal.

**Edit:**
- `tools/capture/py/pyproject.toml`, `tools/capture/py/uv.lock`;
- `tools/capture/py/scenarios/genai/{litellm-proxy-chat,litellm-proxy-embedding,bifrost-chat}/`;
- `tools/capture/py/scenarios/openinference/{envoy-ai-gateway-chat,temporal-openai-agents}/`;
- `fixtures/otlp/genai/{litellm-proxy-chat,litellm-proxy-embedding,bifrost-chat}/`;
- `fixtures/otlp/openinference/{envoy-ai-gateway-chat,temporal-openai-agents}/`.

**Steps.**

1. **Capture project:** add `litellm[proxy]==1.109.1`, `temporalio==1.36.1` and `temporalio-openai-agents[openinference]==2.0.0` to `pyproject.toml` (the spike's `temporal/pyproject.toml` shows the pins and the temporary `exclude-newer-package` line), then `uv lock`.
   - The spike's lock added 4 packages and moved no existing pin. Report every changed existing pin. If one moves, run the whole `run --all --check`, and stop if an existing case drifts.
2. **Scenario files.** Each case directory has `scenario.json` and `main.py` (and the helper modules below). Each `scenario.json` has `description`, `spec_refs` (at least one; use the five of `openinference/openai-agents-tool/scenario.json`), `notes`, `instrumentation`, `libraries` (must contain `instrumentation`, and every entry must be a key of the versions the program passes to `finish_tool`, because the runner requires each in the meta file), the span-shaping fields below, and `turns`. The group is the directory name.

   | Case | Group | `instrumentation` | `libraries` | Versions passed to `finish_tool` |
   |---|---|---|---|---|
   | `litellm-proxy-chat` | `genai` | `litellm` | `litellm`, `openai` | `importlib_versions(["litellm", "openai"])` |
   | `litellm-proxy-embedding` | `genai` | `litellm` | `litellm`, `openai` | the same |
   | `bifrost-chat` | `genai` | `bifrost` | `bifrost`, `openai` | `{"bifrost": "2.5.1", **importlib_versions(["openai"])}` |
   | `envoy-ai-gateway-chat` | `openinference` | `aigw` | `aigw`, `envoy`, `openai` | `{"aigw": "1.2.1", "envoy": "1.39.1", **importlib_versions(["openai"])}` |
   | `temporal-openai-agents` | `openinference` | `openinference-instrumentation-openai-agents` | `openai`, `openai-agents`, `openinference-instrumentation-openai-agents`, `opentelemetry-sdk`, `temporalio`, `temporalio-openai-agents`, `temporal-cli` | `{**importlib_versions(<the six pip names>), "temporal-cli": "1.10.1"}` |

   **Files and checks.** The template drivers are not `ruff`/`mypy` clean. The Done-when `ruff format --check`, `ruff check` and `mypy` apply to the new scenario files, so write typed code with no wildcard imports. The project's rule set includes `S` (bandit) and `ANN`: a gateway launch may carry `# noqa: S603 - a pinned binary`; use an absolute path (`tool_binary(...)`, or the `litellm` next to `sys.executable`) so `S607` does not fire. Each scenario directory has `main.py` and one module named `<group>_<case>.py` with underscores (for example `genai_litellm_proxy_chat.py`; `main.py` imports `main` from it, as `openinference/vertex-ai-chat` does), because mypy maps every `main.py` to one module and every other module name must be unique across scenarios. `temporal-openai-agents` has a third module for the workflow, `openinference_temporal_openai_agents_workflow.py` (the workflow must be importable by name; it cannot live in `__main__`). Do not name any module `workflows.py`.

   Common to every program:
   - the mock is at `CETRA_CAPTURE_MOCK_URL` (`harness.mock_url`); OTLP goes to `CETRA_CAPTURE_OTLP_ENDPOINT` (`harness.receiver_base_url` strips `/v1/traces`), with `Authorization: Bearer capture-mock-key`;
   - ports for everything configurable come from `harness.free_port()`; readiness and stopping use `harness.wait_http` and `harness.stop_gateway`;
   - a binary is located with `harness.tool_binary(...)` (IN5.1 step 5), called first;
   - the gateway process gets an explicit `env=` (the runner clears the environment, and the child needs only what the scenario lists);
   - the gateway is stopped with `harness.stop_gateway` (SIGINT) in `try/finally` before `finish_tool` (every gateway flushes on SIGINT);
   - the client is the `openai` package, with `base_url` pointing at the gateway.

   **LiteLLM Proxy** (both cases; start from `spikes/m5-files/litellm/{config.yaml,run.py}`):
   - **Config.** `general_settings.master_key` is set, as in the template, and requests send `Authorization: Bearer <master>` (the proxy refuses to start without one; the template's value is the one the spike ran, and a weaker one may be refused). `litellm_settings: {callbacks: ["otel"], turn_off_message_logging: false}`. Chat case models: `gpt-4o-mini` (`openai/gpt-4o-mini`) and the alias `ds` (`deepseek/deepseek-chat`), both with `api_base: os.environ/FAKE_BASE`. Embedding case model: `model_name: text-embedding-3-small` (the template calls it `emb`; rename it), `litellm_params.model: openai/text-embedding-3-small`. The model name matters: the embedding span reports no cost, so cetra must price it from the built-in table, and `crates/otel/tests/golden.rs` `captured_llm_spans_are_priced_by_the_builtin_snapshot` (~316–380) fails for any captured `llm` or `embedding` span with tokens and a model that has no price. If the embedding span is unpriced (its provider is empty), stop and report. The chat alias is `ds`, not `or-claude`: the deepseek span is the aliased-model case (reported cost 3.01e-05 in the spike), and it needs no price row.
   - **Launch.** `[os.path.join(os.path.dirname(sys.executable), "litellm"), "--config", <cfg>, "--port", <p>, "--host", "127.0.0.1"]`, the config written into a temporary directory by the program. Readiness: `wait_http("http://127.0.0.1:<p>/health/liveliness", 60)`. If start-up exceeds 60 s, stop and report.
   - **Gateway environment** (explicit `env=`): `PATH`, `HOME`, `FAKE_BASE=<mock>/v1`, `LITELLM_LOCAL_MODEL_COST_MAP=True` (without it the proxy fetches the model cost map from `raw.githubusercontent.com` at start), `DISABLE_SCHEMA_UPDATE=true`, `OTEL_EXPORTER=otlp_http`, `OTEL_ENDPOINT=<receiver base>`, `OTEL_HEADERS=Authorization=Bearer capture-mock-key` with a **literal space** (LiteLLM sends the value as is, so `%20` would arrive as `Bearer%20capture-mock-key`), `OTEL_SERVICE_NAME=litellm-proxy`.
   - **Requests** carry `Authorization: Bearer <master key>` (`OpenAI(api_key=<master key>)`). Chat case, in this order, each with `user="user-1"`: (1) `gpt-4o-mini`, no tools; (2) `ds`, no tools; (3) `gpt-4o-mini` with one `get_weather` tool. Embedding case: one `embeddings.create(model="text-embedding-3-small", input="hello")`.
   - **Turns** (`openai.chat`, in request order): a text turn (31 in, 12 out, model `gpt-4o-mini-2024-07-18`), a text turn for `ds` (model `deepseek-chat`), a tool-call turn (`get_weather`, `{"city": "Paris"}`, finish `tool_calls`, 82 in, 17 out). Embedding case: one `openai.embeddings` turn, model `text-embedding-3-small`, `"dims": 2`, usage input 5.
   - **After the last request the program sleeps 8 s, then sends SIGINT** (the spike's order: the batch exporter's delay is why).
   - **Fields.** `span_names: ["litellm_request"]` (drops the roughly ten plumbing spans per request); `volatile_attributes: ["litellm.call_id", "hidden_params", "metadata.requester_metadata", "gen_ai.response.id"]` (the `gen_ai.response.id` is a uuid on embeddings only; list it in both cases); `timing_attributes: ["litellm.preprocessing.duration_ms"]` (the spike lists it on the `Received Proxy Server Request` span, which `span_names` drops, so keep the entry only if the key survives on a kept span; otherwise leave the list empty).
   - **Ports.** After scrubbing, no gateway or mock port may remain anywhere in `expected.json` or `input.json` (`hidden_params` carries the upstream `api_base`; `metadata.requester_metadata` carries the proxy's host and port). If one does, stop and report which attribute holds it.

   **Bifrost** (`bifrost-chat`; start from `spikes/m5-files/bifrost/`):
   - Copy `pricing.json` and `model-parameters.json` into the scenario directory (committed, so the reported cost is stable). The program writes `config.json` from the template into a temporary app directory, replacing the three placeholders: `@APP_DIR@` the app directory, `@MOCK_URL@` `CETRA_CAPTURE_MOCK_URL`, `@OTLP_ENDPOINT@` the receiver base (the template appends `/v1/traces`). The three air-gap keys stay: `file://` pricing URLs, `mcp_library_sync_interval: 0` and `client.enforce_auth_on_inference: false` (without the first two, Bifrost phones `getbifrost.ai`, and exits at start when that fails).
   - Launch `[tool_binary("bifrost", "2.5.1", "bifrost-http"), "-app-dir", app, "-port", p, "-host", "127.0.0.1", "-log-style", "pretty"]` with `env={"PATH", "HOME", "CETRA_OTLP_AUTH": "Bearer capture-mock-key", "UPSTREAM_KEY": "sk-capture"}`. Readiness: poll `GET /health` until 200 (up to 30 s).
   - **Two requests** with `OpenAI(base_url=f"http://127.0.0.1:{p}/v1").chat.completions.create(model="openai/gpt-4o-mini", ...)`: (1) the question with one `get_weather` tool, which gets the tool-call turn; (2) the same conversation plus the assistant's tool call and a `tool` message, which gets the text turn. Turns: `openai.chat` tool-call (82/17, finish `tool_calls`), then `openai.chat` text (131/12).
   - The mock answers Bifrost's two `GET /v1/models` at start (IN5.1 step 1); without that step the capture fails with "no route".
   - **Expect two `llm` and two `chain`.** `span_names: ["/v1/chat/completions", "chat gpt-4o-mini"]` keeps the roots and the `chat` spans and drops the 24 `plugin.*` spans per request. `volatile_attributes: ["bifrost.request.id", "bifrost.selected_key.id"]`; `timing_attributes: ["bifrost.overhead.duration_ms", "bifrost.upstream.duration_ms", "bifrost.worker.handoff_ms"]`.
   - **Unverified:** the spike ran only the first request. The second request's history is probably flattened the same way (`name`/`args`) in `gen_ai.input.messages`, but that was not observed. Stop at the first mismatch with rule B2 (a different shape, or a history part that stays malformed) and report; do not widen the rule.

   **Envoy AI Gateway** (`envoy-ai-gateway-chat`; start from `spikes/m5-files/aigw/`):
   - A temporary directory under `/tmp` with a short name (the `AIGW_RUNTIME_DIR` holds a Unix socket; a path longer than about 100 characters fails with `bind: invalid argument`) holds `AIGW_CONFIG_HOME`, `AIGW_DATA_HOME`, `AIGW_STATE_HOME` and `AIGW_RUNTIME_DIR`. Symlink (or copy) `tool_binary("envoy", "1.39.1", "envoy")` to `$AIGW_DATA_HOME/envoy-versions/1.39.1/bin/envoy`, so `aigw` never downloads Envoy.
   - Render `aigw.tmpl.yaml` with `@LISTEN@` = a free port and `@FAKE@` = the mock's port; launch `[tool_binary("aigw", "1.2.1", "aigw"), "run", <cfg>, "--admin-port", <free port>]`.
   - `env`: `PATH`, `HOME` (writable: Envoy Gateway writes under `$HOME/.config`), the four `AIGW_*` directories, `ENVOY_VERSION=1.39.1`, `OPENAI_API_KEY=sk-capture`, `OTEL_SERVICE_NAME=aigw`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=<CETRA_CAPTURE_OTLP_ENDPOINT>`, `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`, `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer capture-mock-key`, `OTEL_EXPORTER_OTLP_COMPRESSION=none`, `OTEL_TRACES_EXPORTER=otlp`, `OTEL_METRICS_EXPORTER=none`, `OTEL_LOGS_EXPORTER=none`.
   - Readiness: poll `GET /v1/models` on the listener until 200 (about 2 s; aigw answers it itself). One chat request, model `gpt-4o-mini`; one `openai.chat` text turn (31/12). Then SIGINT (aigw flushes and exits at once; the SDK's default batch delay of 2.4 s would otherwise lose the span).
   - aigw binds fixed ports (1061, 8081, 9856, 18000, 18002, 19000, 19001, 19003): a foreign process on one breaks only this case. Cases run in sequence. Nothing varies between runs, so `volatile_attributes` and `timing_attributes` are empty.

   **Temporal** (`temporal-openai-agents`; start from `spikes/m5-files/temporal/{run.py,wfmod.py}`):
   - The workflow module (`openinference_temporal_openai_agents_workflow.py`) holds the workflow and the `get_weather` activity (defined in `__main__` the worker fails "Failed validating workflow"). It is `wfmod.py` of the spike.
   - `main.py`: `create_tracer_provider(resource=Resource.create({"service.name": "capture-app"}))` from `temporalio.contrib.opentelemetry`; `add_span_processor(BatchSpanProcessor(OTLPSpanExporter()))` (the exporter reads the runner's `OTEL_EXPORTER_OTLP_*`); `trace.set_tracer_provider`; `OpenAIAgentsPlugin(model_provider=OpenAIProvider(openai_client=AsyncOpenAI(base_url=<mock>/v1, api_key=..., max_retries=0), use_responses=True), model_params=ModelActivityParameters(start_to_close_timeout=timedelta(seconds=30)), use_otel_instrumentation=True)` and `OpenTelemetryPlugin(add_temporal_spans=True)` on the client; `WorkflowEnvironment.start_local(dev_server_existing_path=tool_binary("temporal", "1.10.1", "temporal"), dev_server_extra_args=["--headless", "--log-level", "warn"], ui=False)`; one workflow with the fixed id `wf-capture`.
   - No `instrument=` and no `OpenAIAgentsInstrumentor`: the plugin instruments OpenAI Agents itself, and a second instrumentor would double the spans.
   - After the workflow: `await env.shutdown()`, then `finish_tool(tp, ...)` (the provider flushes and shuts down inside it).
   - Turns: `openai.responses`, as `openai-agents-tool`: a tool call (`get_weather`, `{"city": "Paris"}`, 82/17), then text (131/12), both model `gpt-4o-mini-2024-07-18`.
   - **All 22 spans are kept** (14 from the plugin, 8 from `OpenTelemetryPlugin`): no `span_names`. `volatile_attributes: ["temporalRunID"]`. `timing_attributes` stays empty unless a run shows a measured value.

3. **Capture, bless and check** (the golden test loads every captured case, so blessing happens in this same commit):
   - `just capture genai/litellm-proxy-chat genai/litellm-proxy-embedding genai/bifrost-chat openinference/envoy-ai-gateway-chat openinference/temporal-openai-agents`.
   - `just fixtures-bless`, then `just fixtures-fmt`, then `just fixtures-check` (`justfile` ~525–534). Review the diff of each `expected.json` before committing.
   - **No port may remain.** For all five cases, after blessing, run `grep -nE '127\.0\.0\.1:[0-9]+|localhost:[0-9]+' fixtures/otlp/<group>/<case>/{input,expected}.json` and `grep -nE -A3 'server\.port|\.port\"' …`. The first must print nothing. The second may print only well-known ports (such as `443`, which the scrubber leaves); a value equal to the gateway's, the mock's or the receiver's port from the run means the port was not scrubbed: stop and report the attribute.
   - Check each `expected.json`:
     - LiteLLM chat: provider is the upstream's (`openai` for the first and third request, `deepseek` for `ds`); cost is `reported` with the `gen_ai.cost.*` values, including the aliased model (`gen_ai.response.model` is `ds`); the tool turn's output is a tool call; no `gen_ai.completion.0.function_call.*` key stays in `attributes`.
     - LiteLLM embedding: the span is `embedding` and priced.
     - Bifrost: exactly two `llm` and two `chain`; reported cost with `cost_input` and `cost_output`; the tool call is a `tool_call` part, in the first response and in the second request's history.
     - aigw: one `llm`, provider from `llm.system`, cost computed.
     - Temporal: the OpenAI Agents spans as in `openinference/openai-agents-tool`, the plugin's `temporal:*` spans as `chain`, and Temporal's own `StartWorkflow`, `RunWorkflow`, `StartActivity` and `RunActivity` spans as `span`.
   - Run `cargo run -q -p cetra-capture -- run --all --check` once more: if any of the five cases still differs from its committed fixture, stop and report (the case's page then becomes setup only, as in IN3.2).

   Stop and report every deviation.

**Done when** (the `ruff` and `mypy` run covers the new scenario files):

```bash
cargo nextest run -p cetra-otel --test golden --test fixtures
cargo run -q -p cetra-capture -- run --all --check
cd tools/capture/py && uv run ruff format --check && uv run ruff check && uv run mypy
```

All pass.

**Commit:** `Capture LiteLLM Proxy, Bifrost, Envoy AI Gateway and Temporal`

## IN5.4 — Pages

**Goal:** the gateway pages, written from the spikes' per-gateway sections and § Page notes.

**Edit:**
- `website/src/content/docs/docs/integrations/{litellm-proxy,bifrost,envoy-ai-gateway,kong,portkey,truefoundry,vercel-ai-gateway,helicone,cloudflare-ai-gateway}.mdx` (new);
- `website/src/content/docs/docs/integrations/{litellm,temporal,databricks,pydantic-ai}.mdx`;
- `website/src/content/docs/docs/reference/attributes.md`;
- `website/src/content/docs/docs/tracing/cost-and-tokens.mdx`;
- `website/src/content/docs/docs/concepts/conventions.mdx`;
- `skills/cetra/references/*.md` that the regeneration rewrites.

**Steps.** Pages: `litellm-proxy`, `bifrost`, `envoy-ai-gateway` (captured); `kong`, `portkey`, `truefoundry`, `vercel-ai-gateway`, `helicone`, `cloudflare-ai-gateway` (setup only). No OrcaRouter or Anannas page.
- OpenAI-client pages (Portkey, Vercel AI Gateway, Helicone, Cloudflare) use the M2 OpenAI-compatible template:
  - provider shown: the gateway's name where the host is mapped, else `openai`;
  - pricing: the publisher's price through D-IN-13 where mapped, else none;
  - a short "Its own OTLP export" section where one exists (TrueFoundry, Vercel drain, Cloudflare), marked setup only, with what is known to be missing.
- Every gateway page's Limits carries the double-count warning (D-IN-25 item 7).
- **Writing rules.** `otel` appears only inside code spans (`check-terms.mjs` bans the bare word, ~line 12); write "OpenTelemetry" or "OTel" in prose.
- **`litellm-proxy.mdx` and `litellm.mdx` cross-link:** `litellm.mdx` (the Python library, OpenInference instrumentation) gains one sentence "Running the LiteLLM Proxy instead? See [LiteLLM Proxy](/docs/integrations/litellm-proxy/)." and `litellm-proxy.mdx` links back in its intro.
- `litellm-proxy.mdx` states: the config block (`callbacks: ["otel"]`), the environment (with the literal-space `OTEL_HEADERS` and why), `LITELLM_LOCAL_MODEL_COST_MAP=True` for an offline host, the response model column showing the proxy's alias, the reported cost (its margin and discount are included), embeddings unpriced unless the model is in the table, the plumbing spans and the health-probe traces.
- `bifrost.mdx` states: the `otel` plugin block, the three air-gap keys, `enforce_auth_on_inference`, that Bifrost exits at start when it cannot fetch its pricing datasheet, the root span as a chain, the reported cost.
- `envoy-ai-gateway.mdx` states: the standard `OTEL_EXPORTER_OTLP_*` variables, the 2.4 s batch delay, that aigw downloads Envoy from Tetrate's archive unless it is pre-placed, the model column showing the response model (rule E1 not applied), computed cost.
- `temporal.mdx` becomes captured: a `<Backing>` box naming `openinference/temporal-openai-agents`, What you get from the fixture, and the setup as built (`temporalio-openai-agents` 2.0.0, the `OpenTelemetryPlugin` for Temporal's own spans). Remove "setup only".
- One sentence each in `databricks.mdx` (the AI Gateway logs to Unity Catalog, not OTLP) and `pydantic-ai.mdx` (Pydantic AI Gateway exports to Logfire only).
- **`reference/attributes.md`** (~line 75, the GenAI table): add `gen_ai.cost.total_cost`, `gen_ai.cost.input_cost`, `gen_ai.cost.output_cost` and `bifrost.cost.input`, `bifrost.cost.output` to the "Reported cost" row (or a new row beside it); the `gen_ai.operation.name` row (line 61) lists `embedding` and `aembedding` as embedding operations.
- **`tracing/cost-and-tokens.mdx`:** the "Reported" bullet (~lines 23–27) names gateways as a source ("or a gateway such as LiteLLM Proxy or Bifrost"), and the keys sentence (~line 85) lists the new keys.
- **`concepts/conventions.mdx`:** as M4 did for VoltAgent (the table row at line 16), the table gains no row (Bifrost writes the GenAI convention). Add one sentence under the table: "Bifrost writes the GenAI convention under the instrumentation scope `bifrost`; cetra reads its server root span as a chain, and reads its tool calls and cost keys (`bifrost.cost.*`)." Read the VoltAgent row and precedence list first; the precedence list does not change.
- Run `cd website && CETRA_DOCS_WRITE=1 node scripts/skill-references.mjs` (or `just docs-gen`) and commit what it rewrites.

**Done when:**

```bash
cd website && node scripts/check-terms.mjs && node scripts/check-samples.mjs && node scripts/check-backing.mjs && node scripts/skill-references.mjs
```

All pass. `node scripts/check-integrations.mjs` checks the index against the pages: it may fail until IN5.5 lists the new pages, so it runs there.

**Commit:** `Add gateway pages`

## IN5.5 — Index, sidebar, records

**Goal:** as IN4.10.

**Edit:**
- `website/src/content/docs/docs/integrations/index.mdx`;
- `website/astro.config.mjs`;
- `website/src/marketing/data/integrations.test.mjs`;
- `docs/plans/in-integrations/00-design.md`;
- `docs/mvp-decisions.md`;
- `CHANGELOG.md`;
- `docs/roadmap.md`.

**Steps.**
1. **Index and sidebar:**
   - `index.mdx`: add a **Gateways** section after Providers, with rows, tiers and cards for tested rows. OpenRouter stays under Providers. Temporal's row becomes `captured`, with a card.
   - `astro.config.mjs`: a sidebar group **Gateways** right after the Providers group (the group at ~line 187–190; `label: "Gateways"`, `collapsed: true`, one item per page, sorted by label, each label equal to its page's title).
   - `website/src/marketing/data/integrations.test.mjs`: in `parses the real page` (~lines 11–17), insert `"Gateways"` after `"Providers"` in the expected groups array.
2. **Design** (`00-design.md`):
   - add D-IN-25 (this plan's text, item 9 included) to the decisions table;
   - amend D-IN-16's cell (line 314): the keys are LiteLLM's `gen_ai.cost.*` and Bifrost's `bifrost.cost.input` and `bifrost.cost.output`, as captured;
   - amend D-IN-22's cell (line 320): Temporal is captured in M5, with a pinned dev-server binary (D-IN-25), so "setup only until a pinned dev-server binary exists" goes;
   - § Gateways rows as built (line 187): the Temporal row (line 212 sits in § Agent frameworks) changes "setup only until a pinned dev-server binary exists" to "captured (M5)"; Vercel AI Gateway's row notes its trace drain; Kong stays setup only; Azure API Management and Gravitee leave the "spike decides" row;
   - § Not supported gains Azure API Management (no OTLP trace export, Application Insights only) and Gravitee (HTTP spans without LLM attributes; an OpenAI-compatible endpoint behind it is traced by the app's own instrumentation, as on the OpenAI page);
   - the appendix (§ Appendix, from line 385) gains one line each for OrcaRouter and Anannas AI: OpenAI-compatible routers with no OTLP export, no page, no host row.
   - **In `docs/mvp-decisions.md`,** the same: D-IN-16's row (line 1033) amended with the Bifrost keys, D-IN-22's row (line 1039) amended, D-IN-25 added after D-IN-24 (line 1041). D109 (line 164) lists no keys, so it is not edited.
3. **Changelog and roadmap:**
   - CHANGELOG `Added`: gateway pages, the LiteLLM Proxy, Bifrost, Envoy AI Gateway and Temporal captures, `just capture-tools`.
   - CHANGELOG `Changed`: LiteLLM's and Bifrost's reported cost read; Bifrost's root span is a chain; embedding operations.
   - Roadmap (`docs/roadmap.md`, the IN row): the M5 plan link, status "M1–M5 implemented" (drop the M3 note that Temporal's capture waits for a pinned dev-server binary), decisions `D-IN-1`–`D-IN-25`.

**Done when:**

```bash
cd website && node --test scripts/ src/marketing/ && node scripts/check-terms.mjs && node scripts/check-samples.mjs && node scripts/check-backing.mjs && node scripts/check-integrations.mjs && node scripts/skill-references.mjs
just check-docs
```

All pass (`src/marketing/` includes `integrations.test.mjs`). Re-run `just check-docs` once if Astro segfaults (exit 139).

**Commit:** `List the gateway pages and record D-IN-25`

## After the last phase

The main session:
- runs `just check` (touch the crates that embed `CARGO_MANIFEST_DIR` first);
- runs a Sonnet review;
- has a GLM agent fix the findings;
- pushes to PR #242.

## Amendment A — IN5.2b: OpenLLMetry completions without a role

IN5.2 found that LiteLLM's tool turn (`gen_ai.completion.0.function_call.{name,arguments}`, no `gen_ai.completion.0.role`) makes the OpenLLMetry reader record `cetra.normalize_error = messages:openllmetry:output`, although GenAI wins the output. The fix is in the OpenLLMetry reader, not in error recording.

**Goal:** an indexed `gen_ai.completion.N` message without `role` is an assistant message, and its `function_call` object is a tool call.

**Edit:** `crates/otel/src/conventions/openllmetry.rs` (and the converter it calls for indexed messages, if the change belongs there: name it in the report), `docs/spec/02-ingestion.md` (the OpenLLMetry row of § Input/output).

**Steps.**
1. In the OpenLLMetry output path only (`gen_ai.completion.`), a rebuilt message with no `role` gets `role = "assistant"`. Prompts (`gen_ai.prompt.`) keep today's rule (no role → raw JSON and an error; the test `a_prompt_without_a_role_is_raw_json_and_an_error_code` stays).
2. A completion message's `function_call` object `{name, arguments}` becomes one `Part::ToolCall { id: None, name, arguments: parse_tool_arguments(arguments), extra: Map::new() }`, after any text part. If `name` is missing or not a string, the message is a conversion failure as today.
3. Spec 02: one sentence in the OpenLLMetry row.

**Tests:** completion without role → assistant; completion with `function_call` → tool call; the LiteLLM span (both conventions, GenAI empty assistant output + `function_call` keys) has no `cetra.normalize_error`; prompt without role still errors.

**Done when:** `cargo nextest run -p cetra-otel`, clippy, fmt pass; no `expected.json` changed (if one changes, stop and report).

**Commit:** `Read OpenLLMetry completions without a role`

## Amendment B — IN5.3: the LiteLLM Proxy environment and attribute order

The IN5.3 run found two problems.
1. `litellm[proxy]==1.109.1` cannot join `tools/capture/py`. It needs `mcp>=2.3,<3` (semantic-kernel 1.47.0 needs `mcp<2`) and `rich<14` (together 3.1.1 needs `rich>=14.2`). With both frozen by overrides, the proxy dies on import (`mcp.shared.exceptions.MCPError` is an mcp 2 name).
2. Bifrost (Go) writes span attributes in a random order on every run, so `run --check` reports a difference on identical captures.

**Decisions** (recorded in D-IN-25 as item 10):
- **LiteLLM Proxy is a pinned tool, like the gateway binaries.**
  - It gets its own uv project, `tools/capture/litellm-proxy/`, made of a `pyproject.toml` and a `uv.lock`, with no code:
    - `name = "cetra-capture-litellm-proxy"`, `version = "0.0.0"`, `requires-python` as `tools/capture/py`;
    - `dependencies = ["litellm[proxy]==1.109.1"]`;
    - `[tool.uv] package = false`, `exclude-newer = "7 days"` with the same comment as `tools/capture/py`.
  - `fetch-tools.sh` installs it after the binaries: `UV_PROJECT_ENVIRONMENT=<repo>/target/capture-tools/litellm-proxy/1.109.1 uv sync --frozen --project tools/capture/litellm-proxy`. Skip it when `<that dir>/bin/litellm` exists and the lock has not changed: compare a copy of `uv.lock` stored next to the environment.
  - The LiteLLM scenarios launch `tool_binary("litellm-proxy", "1.109.1", "bin", "litellm")`. They take the proxy version for `finish_tool` from that constant.
  - `tools/capture/py` gets no LiteLLM change. The scenario's OpenAI client stays in the capture project.
  - Dependabot does not watch `tools/capture/` today; this changes nothing there.
- **`--check` sorts attributes.** `json_differences` in `tools/capture/src/run.rs` already sorts resource attributes before comparing. It now also sorts every span's, event's and link's `attributes` by key, on both sides. This is a check-only change: no fixture is rewritten, and `determinism.rs` is not touched.
  - Test: `check_ignores_span_attribute_order`. Two documents that differ only in span attribute order compare equal, and a changed value is still reported.
- **Accepted from the run:**
  - `metadata.requester_ip_address` is volatile in both LiteLLM scenarios.
  - Temporal flushes and shuts down its provider, then calls `finish_tool(None, …)`, because temporalio's provider is not an SDK `TracerProvider`.

**IN5.3 Edit list gains:**
- `tools/capture/litellm-proxy/{pyproject.toml,uv.lock}`;
- `tools/capture/fetch-tools.sh`;
- `tools/capture/src/run.rs`, for `json_differences` and its test only;
- the `AGENTS.md` capture-tools line, which gains "and the LiteLLM Proxy environment".
