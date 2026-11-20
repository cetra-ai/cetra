# V · M2 — SDK `get_prompt`: fetch, cache, compile, link

Implementation plan for milestone M2 of track V. It has 5 phases, **V2.1 to V2.5**. **Haiku** implements each phase with no context beyond this plan, the design doc and the files it names; **Sonnet** reviews the milestone. If this plan and the code disagree, **STOP and report** instead of guessing. Each phase ends with its checks green and one commit.

**Prerequisites** (each must print at least one line, else STOP):

```bash
grep -n '"text"' fixtures/prompts/render.json
grep -n "class ContextValues" sdks/python/src/cetra/_context.py
grep -n "export function withContext" sdks/typescript/src/context.ts
grep -n "get_prompt_resolve" crates/server/openapi.json
```

The last one needs M1 (the `resolve` route) in this branch.

**Read first** (every phase): `AGENTS.md`; `docs/plans/v-prompts/00-design.md` D-V-3, D-V-4, D-V-5, D-V-9, D-V-12, D-V-14, D-V-15; `docs/spec/10-sdk.md` § Design rules, § Recorded data; `fixtures/prompts/README.md`; this file up to § V2.1, then your phase.

## What M2 is, and what it is not

M2 delivers, in both SDKs:
- the plain template renderer, passing every case of `fixtures/prompts/render.json` (text and chat);
- `get_prompt` / `getPrompt` with the in-process cache, stale-while-revalidate, the fallback and `PromptNotAvailable` (D-V-14);
- `Prompt.compile`, which also links the current span;
- the `prompt` context value, stamped as `cetra.prompt.name`/`cetra.prompt.version` on every span started inside (D-V-15);
- `client.prompts` for scripts and CI;
- the API reference headings, a "Prompts" docs page, spec 10 and the changelog.

M2 does not change the server (M1, M3), the web UI (M4) or the playground (M5–M7). Until M3 lands, the server keeps the two attributes in `attributes`; that needs nothing here.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-V-3 | `{{ name }}` only; strict; positions are 1-based lines and columns in code points |
| D-V-5 | `text` or `chat`; chat messages `{role, content}`, placeholders `{type: "placeholder", name}` filled with message lists, copied unchanged |
| D-V-9 | default label `production` |
| D-V-12 | `GET /api/v1/projects/current/prompts/{name}/resolve?label=|version=`, `ETag`, `304` on `If-None-Match` |
| D-V-14 | cache per (name, label or version); versions never expire; labels fresh `cache_ttl_s` (60; 0 = off) then stale-while-revalidate, one refresh per key; LRU 1 000; first fetch 5 s per attempt, 2 retries on network errors, 429 and 5xx, `Retry-After` capped at 5 s; failure → stale, else fallback, else `PromptNotAvailable`; disabled SDK = failed first fetch; the 402 pause does not apply |
| D-V-15 | `context(prompt=…)` stamps every span started inside; `compile()` stamps the current recording span; a fallback writes the name only |

Made precise by this plan (V2.5 records them in `docs/mvp-decisions.md` § Track V as amendments of D-V-14 and D-V-15, and in the design doc):
- **TypeScript context.** TypeScript has no `context(values, fn)`; the prompt goes through the existing `withContext({ prompt }, fn)`.
- **Errors.** Two new public exceptions: `PromptNotAvailable` (no prompt and no fallback) and `PromptCompileError` (a missing variable or placeholder, or an invalid fallback template), a `ValueError` in Python. Both are raised to the caller, like spec 10's programming errors; nothing else in the prompt code raises. Their messages hold the prompt name and a reason (`not found`, `status 503`, `network`, `sdk disabled`, `missing variable <name>`), never template text or values.
- **Values.** `compile` accepts `str`, `int`, `float` and `bool` for variables: `bool` renders `true`/`false`, an integral `float` renders like an integer (`1.0` → `1`), other numbers use the language's shortest form (`repr` in Python, `String` in TypeScript; exponent spellings may differ between languages). A placeholder takes a list of `{role, content}` string mappings. Any other value type is a programming error (`TypeError`).
- **Cache scope.** One cache per process, keyed by `(base_url, name, label or version)`; `init()` state is read at each call; the test reset clears it.
- **`client.prompts`** has `get` (uncached), `list` (names), `create`, `create_version`/`createVersion`, `set_label`/`setLabel`; it uses the client's own timeouts and retries.

## Versions

No new dependency. Python uses `requests` (already a dependency); TypeScript uses the runtime `fetch` through `web.ts`.

## Phase graph

| Phase | Touches | Needs |
|---|---|---|
| V2.1 | `sdks/python/src/cetra/_template.py`, its test | — |
| V2.2 | Python prompts, context, processor, client, runtime reset, `__init__`, API page | V2.1 |
| V2.3 | `sdks/typescript/src/template.ts`, its test | — |
| V2.4 | TypeScript prompts, context, processor, client, runtime reset, `index.ts`, API page | V2.3 |
| V2.5 | docs page, sidebar, spec 10, decisions, changelog, roadmap | V2.2, V2.4 |

The Python pair (V2.1, V2.2) and the TypeScript pair (V2.3, V2.4) touch disjoint files and may run in parallel. Each phase commits only its own files, by path (`git add <paths>`; never `git add -A`).

## Conventions for every phase

- Spec 10's rules hold: never break the app (everything but the two new exceptions is caught and logged at debug with the exception's type name only); logs carry prompt names and HTTP statuses, never template text, values, keys or the config; Python warnings start with `cetra: `.
- **Python**: ≥ 3.11, `mypy --strict` over `src` and `tests`, ruff (ANN, S, BLE…), frozen slots dataclasses for public result types, `logger` from `cetra._log`, `warn_once`. Check: `just check-py`.
- **TypeScript**: ESM, `lib` ES2023, `types: []` (no DOM, no Node typings): use `runtimeGlobals()`, `getFetch()`, `timeoutSignal()` from `src/web.ts`; `import type` for types; biome. No `node:` imports in `src`. Check: `just check-ts`.
- **Machine load**: other sessions build on this machine. While iterating run only the touched tests (`cd sdks/python && uv run pytest tests/test_<file>.py`; `pnpm --dir sdks/typescript exec vitest run src/<file>.test.ts --maxWorkers=8`); the full check once before the commit (`export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` first). Never kill a process you did not start.
- Commit once per phase with the exact subject, no attribution lines.

## Precisions (from the cold review; they override anything below that differs)

1. **Python cache seam.** `class _PromptCache: def __init__(self, *, now: Callable[[], float] = time.monotonic, spawn: Callable[[Callable[[], None]], None] = _spawn_daemon, sleep: Callable[[float], None] = time.sleep)`. Module-level `_cache_instance: _PromptCache | None = None`; `_cache()` creates it lazily; `_reset_cache()` sets it to `None`. `refresh_later(self, key, run: Callable[[], None]) -> None`: `get_prompt` builds the closure `run` (which calls `_fetch` with the entry's etag and updates the entry). `_fetch(config, name, label, version, etag, *, sleep)` receives `sleep=cache.sleep`. Tests install `_prompts._cache_instance = _PromptCache(now=clock, spawn=lambda run: run(), sleep=sleeps.append)`. `_spawn_daemon(run)` starts `threading.Thread(target=run, name="cetra-prompt-refresh", daemon=True)`.
2. **TypeScript cache seam.** `export class PromptCache { constructor(seams?: { now?: () => number; fetch?: RestFetch; sleep?: (ms: number) => Promise<void> }) }` in `prompts.ts`; `export function setPromptCacheForTests(cache: PromptCache | undefined): void`; `getPrompt` uses the installed cache (created lazily with defaults) and takes its `fetch` and `sleep` from it; `resetForTests` (runtime.ts) calls `setPromptCacheForTests(undefined)`. The default `fetch` is `getFetch()` cast to `RestFetch`, exactly as `client.ts`' private `defaultFetch()` does; `FakeRest.fetch` plugs into the seam. `getPrompt` reads the body with `response.text()` then `JSON.parse`.
3. **Ruff.** `class PromptNotAvailable(Exception):  # noqa: N818 (public name, D-V-14)`.
4. **Import cycles.** Python: `_prompts.py` imports `_runtime` at module top; `_runtime.reset()` does `from cetra import _prompts` inside the function and calls `_prompts._reset_cache()` as its first statement (before any early `return`); `_context.py` checks the prompt with a function-local `from cetra._prompts import Prompt`, and types the parameter `prompt: Prompt | None = None` with `Prompt` imported under `TYPE_CHECKING` (and a runtime `isinstance` guard that drops anything else with the debug log). TypeScript: put `Prompt`, `PromptNotAvailable` and the `ChatEntry`/`ChatMessage` types in a leaf module `src/prompt.ts` that imports only `template.ts`, `log.ts` and `@opentelemetry/api`; `context.ts` imports `Prompt` from `./prompt.js`; `prompts.ts` holds `getPrompt`, `GetPromptOptions`, `PromptCache`, `setPromptCacheForTests` and `Prompts`, and imports the runtime. Add `src/prompt.ts` to V2.4's Edit list.
5. **Retry waits.** Do not reuse `retry_delay`/`waitMillis`. Each prompts module has a private wait: attempt 1 → 0.5 s, attempt 2 → 1 s; a `Retry-After` header that is ASCII digits only (Python: stripped, `isascii() and isdigit()`; TypeScript: the exported `NON_NEGATIVE_INTEGER` regex) replaces it, capped at 5 s.
6. **PUT.** Python `_request` and TypeScript `request` are generic over the method; `set_label`/`setLabel` call them with `"PUT"` and no retry (PUT is not in the retryable set). No change to the client's request code.
7. **Bad 200 bodies.** A 200 whose body is not JSON or lacks `name`, `type`, `version` or `template` is `Failed("invalid response")`, not retried; `PromptNotAvailable` then carries `invalid response`. Python's `_fetch` uses `requests.get(..., timeout=5)` (no shared session: the refresh runs on another thread).
8. **Starting the runtime against a recorder.** Python: `exporter, start = memory; start(base_url=http_recorder.url)` (the `memory` fixture merges keyword options over its defaults). TypeScript: `startMemory({ baseUrl: "http://cetra.test" })` plus `setPromptCacheForTests(new PromptCache({ fetch: rest.fetch, sleep: rest.sleep, now }))`.
9. **Chat entries in the renderers.** An entry is a placeholder when it has `type == "placeholder"` and a string `name`; otherwise it must have string `role` and `content`, else `PromptCompileError("invalid", reason="not_a_message")` (also accept `not_a_message` in TypeScript). Name rule: trim with `.strip(" \t")` (TypeScript: remove only leading/trailing spaces and tabs), then `re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name, re.ASCII)` (TypeScript `/^[A-Za-z_][A-Za-z0-9_]*$/` on the trimmed text, which has no newline inside a valid name): the `newline-in-tag` case must give `not_a_name`. Python `_template.py` imports `re`, `typing.Final`, `collections.abc.Mapping, Sequence`.
10. **Reference pages.** Besides the new headings, update in the same phase: the `Client` entry (it gains `prompts`) and the `context`/`withContext`/`ContextOptions` entries (the `prompt` option) in both API pages, and the prose of `reference/python/context.mdx`, `reference/typescript/context.mdx` (the prompt option, one paragraph) and `reference/{python,typescript}/scores-datasets-experiments.mdx` (one paragraph on `client.prompts`). Links from these pages to `/docs/evaluation/prompts/` are added only in V2.5, once the page exists (the link validator runs in `check-docs`).
11. **TypeScript exports** (`index.ts`), exactly: values `getPrompt`, `Prompt`, `PromptCompileError` (re-exported from `./template.js`), `PromptNotAvailable`; types `ChatEntry`, `ChatMessage`, `GetPromptOptions`, `PromptValues`, `type Prompts` (as `type Datasets` is). `RESERVED`, `variables`, `render`, `chatNames`, `renderChat`, `PromptCache` and `setPromptCacheForTests` are not exported from `index.ts`. Python `__all__` gains `Prompt`, `PromptCompileError`, `PromptNotAvailable` and `get_prompt`; `Prompts` is not exported (like `Datasets`).
12. **Version entries and `cache_ttl_s = 0`.** A version entry is served from the cache whatever `cache_ttl_s` is (versions never change); `cache_ttl_s = 0` affects label lookups only. Add this case to test 5.
13. **TypeScript programming errors** reject the promise with `TypeError`; test with `await expect(…).rejects.toThrow(TypeError)`.
14. **Python tests and mypy.** Narrow `compile()`'s `str | list` result with `isinstance` or `assert` in tests.
15. **Docs page markers.** The untested marker is exactly `{/* untested: needs a running cetra server */}` on the line before each code fence; the page `description` is at most 160 characters. The page says that `get_prompt` before `init()`, or with the SDK disabled, behaves like a failed fetch (reason `sdk disabled`).
17. **PUT in the Python recorder.** `sdks/python/tests/conftest.py` joins V2.2's Edit list: add `def do_PUT(self) -> None: self._handle()` to `_RecordingHandler` (next to `do_PATCH`; use the handler's real dispatch method name if it differs).
18. **Quoted annotations.** `_context.py` has no `from __future__ import annotations`: write `prompt: "Prompt | None" = None` (quoted), and quote the `Client` annotation in `Prompts` as `_datasets.py` does.
19. **Missing key.** A runtime whose `config.api_key` is `None` counts as `sdk disabled` (narrow it before building the `Authorization` header, in both SDKs).
20. **No top-level use** of `runtime` in TypeScript `prompts.ts` (and of `prompts` in `runtime.ts`): only inside functions.
21. **One body parser.** Python `_parse_prompt(body: object) -> Prompt | None` (TypeScript `parsePrompt(body: unknown): Prompt | undefined`) returns `None`/`undefined` on an invalid shape; `_fetch` maps that to `Failed("invalid response")`, and `client.prompts` raises `ApiError` with `detail="invalid response"` (status 200, code `invalid_response`) built the way `_client.py`/`client.ts` build other `ApiError`s.
22. **TypeScript tests** include `setLabel` (V2.2 test 13's TypeScript twin).
23. **Warning clock.** The once-a-minute warning uses the cache's injected `now` (seconds in Python, milliseconds in TypeScript), so test 8 advances the fake clock by 61 s.
24. **Prompt name in compile errors.** The renderers (V2.1, V2.3) take no prompt name. `Prompt.compile` catches `PromptCompileError` from them and raises a new one with the same `kind`, name/variable, `reason`, `line`, `column` and `prompt=self.name` (Python: `raise … from None`). A `not_a_message` error has no position: its message is `invalid template: not_a_message`. Placeholder names follow the tag-name rule (trim spaces and tabs, ASCII name, not reserved), else `invalid` with reason `not_a_name` and no position, as Python's `_template.py` already does; V2.4 adds that check to TypeScript's `renderChat`/`chatNames` (`sdks/typescript/src/template.ts` and its test join V2.4's Edit list).
16. **Spec 10** (V2.5) also gains the resolve URL in the `### Names` table and a `### Language differences` line on number formatting (`str` vs `String`, exponent spellings; no shared fixture covers them).

---

## V2.1 — Python template renderer

**Commit subject:** `python sdk: add the plain prompt template renderer`

### Edit

| File | Change |
|---|---|
| `sdks/python/src/cetra/_template.py` | new |
| `sdks/python/tests/test_template.py` | new |

### `_template.py`, exactly

```python
"""Plain ``{{ name }}`` prompt templates (D-V-3, D-V-5): the rules of ``fixtures/prompts``."""

RESERVED: Final = frozenset({
    "true", "false", "none", "True", "False", "None",
    "and", "or", "not", "in", "is", "if", "else",
    "loop", "self", "range", "dict", "namespace", "debug",
})

class PromptCompileError(ValueError):
    """A template is invalid, or a variable or placeholder has no value.

    ``kind`` is ``invalid``, ``missing_variable`` or ``missing_placeholder``. ``name`` is the
    missing name; ``reason``, ``line`` and ``column`` describe an invalid template. The message
    never quotes the template.
    """
    def __init__(self, kind: str, *, name: str | None = None, reason: str | None = None,
                 line: int | None = None, column: int | None = None, prompt: str | None = None) -> None: ...

def variables(source: str) -> list[str]:
    """Variable names in order of first use. Raises PromptCompileError(kind="invalid")."""

def render(source: str, values: Mapping[str, str]) -> str:
    """Substitutes every variable; raises PromptCompileError on an invalid template, then on
    the first missing variable in template order."""

def chat_names(template: Sequence[Mapping[str, str]]) -> tuple[list[str], list[str]]:
    """(variables, placeholder names), each in order of first use across the messages."""

def render_chat(template: Sequence[Mapping[str, str]], values: Mapping[str, str],
                placeholders: Mapping[str, Sequence[Mapping[str, str]]]) -> list[dict[str, str]]:
    """Renders a chat template (rules below)."""
```

Scanning rules are those of `crates/eval/src/plain_template.rs` (read it: §1 of `docs/plans/v-prompts/01-m1-registry.md` V1.1): `{%`/`{#` → `block`; `{{` with no later `}}` → `unclosed`; inner text trimmed of spaces and tabs must match `[A-Za-z_][A-Za-z0-9_]*` (ASCII) → else `not_a_name`; a name in `RESERVED` → `reserved`; anything else is text. Positions: 1-based line and column counted in code points (Python `str` indexing), a `\n` starts a new line. Values are inserted verbatim. A trailing newline is kept.

`render_chat` rules, in order:
1. Scan every message's `content` (in order) for validity; the first invalid one raises `invalid`.
2. Every variable of every message (in order of first use) must be in `values`; the first missing raises `missing_variable`.
3. Every placeholder (in order) must be in `placeholders`; the first missing raises `missing_placeholder`.
4. Output: messages in template order; a message becomes `{"role": role, "content": rendered}`; a placeholder is replaced by copies of the given messages (`dict(m)`), not rendered.

`PromptCompileError`'s message: `f"prompt {prompt}: {detail}"` when `prompt` is set, else `detail`, where `detail` is `f"missing variable {name}"`, `f"missing placeholder {name}"` or `f"invalid template at line {line}, column {column}: {reason}"`.

### Tests (`test_template.py`)

1. `test_shared_text_cases` — parametrized over `json.loads((Path(__file__).parents[3] / "fixtures" / "prompts" / "render.json").read_text())["text"]` (ids = case names): `variables(template)` equals `variables` (or raises `invalid` when it is `null`); `render(template, values)` equals `expected`, or raises with the case's `kind`, `name`, `reason`, `line`, `column`.
2. `test_shared_chat_cases` — the same over `["chat"]` with `render_chat(template, values, placeholders)`, and `chat_names(template) == (variables, placeholder_names)`.
3. `test_error_message_has_no_template_text` — `render("{{ secret.x }}", {})` raises an error whose `str()` does not contain `secret`.

### Done when

`cd sdks/python && uv run pytest tests/test_template.py` passes, then `just check-py`.

---

## V2.2 — Python `get_prompt`, `Prompt`, context, `client.prompts`

**Commit subject:** `python sdk: add get_prompt with caching, prompt links and client.prompts`

### Read first

`sdks/python/src/cetra/__init__.py`, `_runtime.py` (`current`, `Runtime.config`, `reset`), `_config.py` (`Config`), `_client.py` (`Client`, `_request`, `_segment`, `ApiError`), `_datasets.py` (sub-client style), `_retry.py`, `_context.py` (`ContextValues`, `context`, `_enter`, `experiment_scope`), `_processor.py` (`_stamp`), `_scores.py` (`ScoreQueue`: lazy daemon thread, `_register_at_fork`, `_reinit`, `_note_drop`), `_log.py`, `tests/conftest.py` (`http_recorder`, `memory`, `isolated_runtime`), `tests/test_context.py`, `tests/test_client.py`, `tests/test_api_reference.py`, `website/src/content/docs/docs/reference/python/api.mdx`.

### Edit

| File | Change |
|---|---|
| `sdks/python/src/cetra/_prompts.py` | new: `Prompt`, `PromptNotAvailable`, `get_prompt`, the cache, `Prompts` (client sub-client) |
| `sdks/python/src/cetra/_context.py` | `prompt` context value |
| `sdks/python/src/cetra/_processor.py` | stamp it |
| `sdks/python/src/cetra/_client.py` | `prompts` property |
| `sdks/python/src/cetra/_runtime.py` | `reset()` clears the prompt cache |
| `sdks/python/src/cetra/__init__.py` | exports `Prompt`, `PromptCompileError`, `PromptNotAvailable`, `get_prompt` (sorted `__all__`) |
| `sdks/python/tests/test_prompts.py` | new |
| `sdks/python/tests/test_context.py` | prompt stamping tests |
| `website/src/content/docs/docs/reference/python/api.mdx` | four headings |

### Types (in `_prompts.py`)

```python
@dataclass(frozen=True, slots=True)
class Prompt:
    """A prompt version from the registry, or a fallback (D-V-14)."""
    name: str
    type: Literal["text", "chat"]
    template: str | tuple[Mapping[str, str], ...]
    version: int | None            # None for a fallback
    config: Mapping[str, Any]      # MappingProxyType; {} for a fallback
    labels: tuple[str, ...]
    variables: tuple[str, ...]
    placeholders: tuple[str, ...]
    is_fallback: bool

    def compile(self, **values: object) -> str | list[dict[str, str]]: ...

class PromptNotAvailable(Exception):
    """No prompt could be fetched and no fallback was given (D-V-14)."""
    def __init__(self, name: str, reason: str) -> None: ...   # message f"prompt {name} is not available: {reason}"; attributes name, reason

def get_prompt(name: str, *, label: str | None = None, version: int | None = None,
               fallback: str | Sequence[Mapping[str, str]] | None = None,
               cache_ttl_s: float = 60.0) -> Prompt: ...
```

### Behaviour

**`Prompt.compile(**values)`**: split `values` into variables (`str`/`int`/`float`/`bool`, converted as in § Decisions) and placeholders (a `list`/`tuple` of mappings with string `role` and `content`); any other type raises `TypeError(f"prompt value {key}: unsupported type")` (programming error). Render with `_template.render` (text) or `_template.render_chat` (chat), passing `prompt=self.name` into errors. After success, link the current span: `span = trace_api.get_current_span(); if span.is_recording(): span.set_attribute("cetra.prompt.name", self.name)` and, when `self.version is not None`, `cetra.prompt.version`; this linking is inside `try/except Exception` → `logger.debug("cetra: could not link the prompt (%s)", type(error).__name__)`.

**`get_prompt`**:
1. Programming errors (raise `ValueError`): empty `name`; both `label` and `version`; `version < 1`; `cache_ttl_s < 0`.
2. `label = label or "production"` when `version` is None.
3. `runtime = _runtime.current()`; `None` (disabled) → step 7 with reason `sdk disabled`.
4. `key = (config.base_url, name, f"v:{version}" if version else f"l:{label}")`; `cache = _cache()` (module singleton, created lazily).
5. Cache hit: a version entry, or a label entry younger than `cache_ttl_s` (with `cache_ttl_s > 0`) → return it. A stale label entry (and `cache_ttl_s > 0`) → `cache.refresh_later(key, …)` and return the stale prompt.
6. Miss (or `cache_ttl_s == 0`): `fetch(config, name, label, version, etag=None)` with retries (below). Success → store (unless `cache_ttl_s == 0`) and return.
7. Failure: a stale entry exists → return it (warn as below); else `fallback` given → `Prompt` built from it (`type` = `text` for a `str`, else `chat`; `version=None`, `labels=()`, `config={}`, names from `_template.variables`/`chat_names`; an invalid fallback template raises `PromptCompileError` here, a programming error); else raise `PromptNotAvailable(name, reason)` with `reason` `not found` (404), `status <n>`, `network`, or `sdk disabled`.

**Fetch** (`_fetch`, module-private): `GET {base_url}/api/v1/projects/current/prompts/{quote(name, safe='')}/resolve` with `params={"label": label}` or `{"version": version}`, headers `Authorization: Bearer <key>` and `If-None-Match: <etag>` when given, `timeout=5`. Up to 3 attempts: retry on `requests.RequestException`, 429 and ≥ 500, sleeping `Retry-After` seconds when it is a non-negative integer (capped at 5), else 0.5 s then 1 s (through an injectable `sleep`). 200 → parse the body into a `Prompt` (`template` list → tuple of `MappingProxyType`s, `config` → `MappingProxyType`) and keep the `ETag` header. 304 → "not modified". Other statuses → failure with that status. The function returns a small private result (`Fetched(prompt, etag)`, `NotModified`, or `Failed(reason)`).

**Cache** (`_PromptCache`, module-private):
- an `OrderedDict[key, Entry(prompt, etag, fetched_at)]` under a `threading.Lock`, at most 1 000 entries (LRU: move to end on hit, pop the oldest on insert);
- `fetched_at` from an injectable `now` (default `time.monotonic`);
- `refresh_later(key, fetch_args)`: if `key` is in the in-flight set, return; else add it and start one daemon thread (`threading.Thread(target=…, name="cetra-prompt-refresh", daemon=True)`, through an injectable `spawn` so tests run it inline) that fetches with the entry's `etag`: `Fetched` → replace the entry; `NotModified` → `fetched_at = now()`; `Failed` → keep the entry and log `cetra: prompt %s refresh failed (%s)` with the name and reason at warning level at most once a minute for the whole cache (the `ScoreQueue._note_drop` pattern); always remove `key` from the in-flight set in a `finally`; the thread body catches `Exception` (debug log) so it never dies with an exception;
- fork safety: register `os.register_at_fork(after_in_child=…)` with a weakref, as `ScoreQueue` does, recreating the lock and clearing the in-flight set;
- `clear()`; `_runtime.reset()` calls `_prompts._reset_cache()` (sets the singleton to `None`).

**Context** (`_context.py`): `ContextValues` gains `prompt: tuple[str, int | None] | None = None`. `context.__init__` gains `prompt: object = None` (keyword-only, last): a `Prompt` instance becomes `(prompt.name, prompt.version)`; anything else that is not `None` is dropped with `logger.debug("cetra: context prompt ignored: not a Prompt")`. In `_enter`, `prompt` is the inner one when set, else `outer.prompt`. `experiment_scope` keeps `prompt=None`. Avoid an import cycle: import `Prompt` inside the function, or check with `isinstance` against a lazily imported class.

**Processor** (`_stamp`): `if values.prompt is not None: span.set_attribute("cetra.prompt.name", values.prompt[0])` and, when the version is not `None`, `span.set_attribute("cetra.prompt.version", values.prompt[1])`.

**`client.prompts`** (`Prompts`, not exported; `Client.__init__` adds `self._prompts = Prompts(self)` and a `prompts` property), every call through `self._client._request` with route templates for errors:
- `get(name, *, label=None, version=None) -> Prompt` (`GET /prompts/{prompt}/resolve`, uncached);
- `list() -> list[str]` (pages `GET /prompts` with `limit=1000` and `cursor`, returns the names);
- `create(name, *, type, template, config=None, labels=(), commit_message=None, description=None, tags=()) -> Prompt` (`POST /prompts`, never retried);
- `create_version(name, *, template, config=None, labels=(), commit_message=None) -> Prompt` (`POST /prompts/{prompt}/versions`, never retried);
- `set_label(name, label, version) -> None` (`PUT /prompts/{prompt}/labels/{label}`, body `{"version": version}`; if `_request` has no PUT support, add `PUT` to it the way `POST` is handled and say so).
Optional request fields are left out of the body when not given.

### Tests

`tests/test_prompts.py` (using `http_recorder`, the `memory` fixture to start the in-memory runtime pointed at `http_recorder.url` the way `test_examples.py` does, and direct `_PromptCache` tests with fake `now`/`spawn`/`sleep`):
1. `test_get_prompt_fetches_and_caches` — one 200 with an `ETag`; two calls → one request; the request path is `/api/v1/projects/current/prompts/support-reply/resolve?label=production` with the bearer header.
2. `test_version_entries_never_expire` — `version=2`, advance the fake clock by 1 hour → no new request.
3. `test_stale_label_is_served_and_revalidated` — after `ttl`, the call returns the old prompt and the inline refresh sends `If-None-Match`; a 304 keeps the prompt and resets the age; a later 200 replaces it.
4. `test_one_refresh_per_key` — with a `spawn` that only records, two stale calls start one refresh.
5. `test_ttl_zero_always_fetches`.
6. `test_retries_then_succeeds` — 503, 503, 200 → success; sleeps `[0.5, 1.0]`; `Retry-After: 30` is capped to 5.
7. `test_not_found_uses_fallback` / `test_not_found_without_fallback_raises` — `PromptNotAvailable` with `reason == "not found"`; 404 is not retried.
8. `test_failed_refresh_keeps_stale_and_warns_once` — two failed refreshes within a minute → one warning containing the name and `status 500`, not the template.
9. `test_disabled_sdk` — no runtime: fallback returned; without fallback `PromptNotAvailable("…", "sdk disabled")`.
10. `test_compile_links_the_current_span` — inside `cetra.span("x")` (or `observe`), `prompt.compile(product="cetra", question="hi", history=[])` returns the messages; the span has both attributes; a fallback prompt writes the name only.
11. `test_compile_value_types` — `True` → `true`, `1.0` → `1`, `2.5` → `2.5`, an object → `TypeError`; a missing value → `PromptCompileError` with `kind == "missing_variable"`.
12. `test_lru_bound` — 1 001 version keys → the first is evicted.
13. `test_client_prompts` — `Client(api_key=…, base_url=http_recorder.url)`: `create`, `create_version`, `set_label`, `get`, `list` send the right methods, paths and bodies.
14. `test_logs_have_no_template` — capture `caplog` around a failed refresh and a successful fetch: no template text, no key.

`tests/test_context.py`: `test_context_prompt_is_stamped` (a span started inside `cetra.context(prompt=p)` has both attributes; nested context without prompt keeps it; inner prompt wins); `test_context_prompt_rejects_other_types` (a string → no attribute, no exception).

`api.mdx` (Python): headings `### \`Prompt\``, `### \`PromptCompileError\``, `### \`PromptNotAvailable\``, `### \`get_prompt\`` in sorted position, each with a `{/* untested: signature */}` line, a signature block and two or three sentences, in the page's style.

### Done when

`cd sdks/python && uv run pytest tests/test_prompts.py tests/test_context.py tests/test_api_reference.py` passes, then `just check-py`.

---

## V2.3 — TypeScript template renderer

**Commit subject:** `typescript sdk: add the plain prompt template renderer`

### Edit

| File | Change |
|---|---|
| `sdks/typescript/src/template.ts` | new |
| `sdks/typescript/src/template.test.ts` | new |

### API, exactly

```ts
export const RESERVED: ReadonlySet<string>;   // the same 19 names as Python's RESERVED
export type ChatMessage = { readonly role: string; readonly content: string };
export type ChatEntry = ChatMessage | { readonly type: "placeholder"; readonly name: string };

export class PromptCompileError extends Error {
  readonly kind: "invalid" | "missing_variable" | "missing_placeholder";
  readonly variable: string | undefined;   // the missing name
  readonly reason: string | undefined;     // block | unclosed | not_a_name | reserved
  readonly line: number | undefined;
  readonly column: number | undefined;
  constructor(kind: …, details: { variable?: string; reason?: string; line?: number; column?: number; prompt?: string });
}

export function variables(source: string): string[];
export function render(source: string, values: Readonly<Record<string, string>>): string;
export function chatNames(template: readonly ChatEntry[]): { variables: string[]; placeholders: string[] };
export function renderChat(template: readonly ChatEntry[], values: Readonly<Record<string, string>>,
  placeholders: Readonly<Record<string, readonly ChatMessage[]>>): ChatMessage[];
```

The rules and messages are those of V2.1 (`name` in Python is `variable` here: `name` is taken by `Error`). Positions count code points: iterate with `for (const ch of source)` or `Array.from(source)`, never `source.length` indexes for columns. `this.name = "PromptCompileError"`.

### Tests (`template.test.ts`)

The same three tests as V2.1, reading the fixture with `JSON.parse(readFileSync(new URL("../../../fixtures/prompts/render.json", import.meta.url), "utf8"))` (test files may use `node:fs`; check how `conformance.test.ts` imports it and do the same).

### Done when

`pnpm --dir sdks/typescript exec vitest run src/template.test.ts --maxWorkers=8`, then `just check-ts`.

---

## V2.4 — TypeScript `getPrompt`, `Prompt`, context, `client.prompts`

**Commit subject:** `typescript sdk: add getPrompt with caching, prompt links and client.prompts`

### Read first

`sdks/typescript/src/index.ts`, `runtime.ts` (`currentRuntime`, `resetForTests`, `Runtime.config`), `client.ts` (`Client`, `request`, `segment`, `ApiError`, `RestFetch`), `datasets.ts`, `retry.ts`, `context.ts` (`ContextValues`, `coerceValues`, `contextFor`, `withContext`), `processor.ts` (`stampContext`), `scores.ts` (`countDrop`), `log.ts`, `web.ts`, `rest.test-helpers.ts` (`FakeRest`, `inOrder`), `testing.test-helpers.ts` (`startMemory`), `context.test.ts`, `api-reference.test.ts`, `website/src/content/docs/docs/reference/typescript/api.mdx`.

### Edit

| File | Change |
|---|---|
| `sdks/typescript/src/prompts.ts` | new: `Prompt`, `PromptNotAvailable`, `getPrompt`, `GetPromptOptions`, `PromptValues`, the cache, `Prompts` |
| `sdks/typescript/src/prompts.test.ts` | new |
| `sdks/typescript/src/context.ts`, `context.test.ts` | `prompt` option |
| `sdks/typescript/src/processor.ts` | stamp it |
| `sdks/typescript/src/client.ts` | `readonly prompts: Prompts` |
| `sdks/typescript/src/runtime.ts` | `resetForTests` clears the prompt cache |
| `sdks/typescript/src/index.ts` | exports |
| `website/src/content/docs/docs/reference/typescript/api.mdx` | headings |

### API

```ts
export type PromptValues = Readonly<Record<string, string | number | boolean | readonly ChatMessage[]>>;

export interface GetPromptOptions {
  readonly label?: string;
  readonly version?: number;
  readonly fallback?: string | readonly ChatMessage[];
  /** Seconds a label stays fresh; 0 turns the cache off. Default 60. */
  readonly cacheTtlS?: number;
}

export class Prompt {
  readonly name: string;
  readonly type: "text" | "chat";
  readonly template: string | readonly ChatEntry[];
  readonly version: number | undefined;
  readonly config: Readonly<Record<string, unknown>>;
  readonly labels: readonly string[];
  readonly variables: readonly string[];
  readonly placeholders: readonly string[];
  readonly isFallback: boolean;
  compile(values?: PromptValues): string | ChatMessage[];
}

export class PromptNotAvailable extends Error { readonly prompt: string; readonly reason: string; }

export function getPrompt(name: string, options?: GetPromptOptions): Promise<Prompt>;

export class Prompts {   // client.prompts
  get(name: string, options?: { label?: string; version?: number }): Promise<Prompt>;
  list(): Promise<string[]>;
  create(input: { name: string; type: "text" | "chat"; template: string | readonly ChatEntry[]; config?: Record<string, unknown>; labels?: readonly string[]; commitMessage?: string; description?: string; tags?: readonly string[] }): Promise<Prompt>;
  createVersion(name: string, input: { template: string | readonly ChatEntry[]; config?: Record<string, unknown>; labels?: readonly string[]; commitMessage?: string }): Promise<Prompt>;
  setLabel(name: string, label: string, version: number): Promise<void>;
}
```

`createVersion` and `create` send `commit_message` (snake case) on the wire.

### Behaviour

Exactly V2.2's behaviour, with these TypeScript specifics:
- Programming errors throw `TypeError` (as spec 10 says for TypeScript).
- Fetch through `getFetch()` with `signal: timeoutSignal(5000)`; headers as an object; `If-None-Match` when an `etag` is known. Query string built by hand: `?label=` + `encodeURIComponent(label)` or `?version=` + the number; the name segment with the client's exported `segment()`.
- Sleep through `defaultSleep` from `retry.ts` (or an injected seam).
- The cache: a module-level `PromptCache` (a `Map` in insertion order for the LRU) with injectable `now` (default `Date.now`, milliseconds), `fetch` and `sleep` seams; `refreshLater` keeps a `Map<string, Promise<void>>` of in-flight refreshes and starts `void refresh().catch(…)`; tests reach the cache through an exported-for-tests function in a `*.test-helpers.ts` file or a `PromptCache` they construct directly. No timers.
- Warnings at most once a minute through `warn()` (the `countDrop` pattern), naming the prompt and the reason.
- `withContext({ prompt }, fn)`: `ContextOptions` gains `readonly prompt?: Prompt`; `ContextValues` gains `prompt: { name: string; version: number | undefined } | undefined`; `coerceValues` and `contextFor` carry it (inner wins, else outer); anything that is not a `Prompt` instance is dropped with `debug(…)`.
- `stampContext` writes `cetra.prompt.name` and, when defined, `cetra.prompt.version`.
- `compile()` links the active span: `const span = trace.getSpan(context.active()); if (span?.isRecording()) …` inside `try/catch` → `debug`.

### Tests (`prompts.test.ts`, `context.test.ts`)

The same cases as V2.2's tests 1–14 and the two context tests, with `FakeRest`/`inOrder` replies (`{ status: 304 }`, headers `{ etag: '"x"' }`), `startMemory` for the runtime, and an injected fetch for `getPrompt` (through the cache seam). `api-reference.test.ts` must pass: add `### \`…\`` headings for every new export (values and types: `ChatEntry`, `ChatMessage`, `GetPromptOptions`, `Prompt`, `PromptCompileError`, `PromptNotAvailable`, `PromptValues`, `Prompts`, `getPrompt`) in the page's structure.

### Done when

`pnpm --dir sdks/typescript exec vitest run src/prompts.test.ts src/context.test.ts src/api-reference.test.ts --maxWorkers=8`, then `just check-ts`.

---

## V2.5 — Docs page, spec 10, decisions, changelog

**Commit subject:** `docs: document prompts in the SDKs`

### Edit

| File | Change |
|---|---|
| `website/src/content/docs/docs/evaluation/prompts.mdx` | new page |
| `website/astro.config.mjs` | sidebar entry "Prompts" after "Datasets" |
| `docs/spec/10-sdk.md` | attributes, APIs, the raise sentence |
| `docs/mvp-decisions.md`, `docs/plans/v-prompts/00-design.md` | D-V-14, D-V-15 amendments |
| `CHANGELOG.md`, `docs/roadmap.md` | lines |

### Steps

1. **Page** `evaluation/prompts.mdx` (frontmatter `title: Prompts`, a `description` within the length `check-terms.mjs` allows; follow `website/STYLE.md` and the structure of `evaluation/datasets.mdx`): what the registry is (versions, labels, protected labels); fetching with `get_prompt` / `getPrompt` in `Tabs` for Python and TypeScript; `compile`, chat placeholders; linking with `context(prompt=…)` / `withContext({ prompt }, …)`; caching and fallback (60 s, stale-while-revalidate, fallback, `PromptNotAvailable`); the REST fetch (`curl` with `GET …/prompts/{name}/resolve`, `ETag`). Every code block is preceded by the marker `website/scripts/check-samples.mjs` accepts for an untested block, with the reason "needs a running server" (copy the exact marker spelling from an existing page that uses it). No screenshot (M4 adds the UI).
2. **Sidebar**: in `astro.config.mjs`, after `{ label: "Datasets", link: "/docs/evaluation/datasets/" }`, add `{ label: "Prompts", link: "/docs/evaluation/prompts/" }`.
3. **Spec 10**: the span attributes table gains `cetra.prompt.name` (string; context processor, `compile`) and `cetra.prompt.version` (int; omitted for a fallback); the Python and TypeScript API blocks gain `get_prompt`/`getPrompt`, `compile`, `context(prompt=…)`/`withContext({ prompt }, …)` and `client.prompts`; the sentence "These are the only cases where the SDK raises after `init()`" becomes "…, plus `PromptNotAvailable` and `PromptCompileError` from the prompt API (D-V-14)"; a short `### Prompts` subsection lists the cache rules of D-V-14.
4. **Decisions**: append to D-V-14 in `docs/mvp-decisions.md` and the design doc: "TypeScript passes the prompt to `withContext({ prompt }, fn)`. `PromptCompileError` (a missing variable or placeholder, an invalid fallback) and `PromptNotAvailable` are the prompt API's only exceptions. `compile` takes strings, numbers and booleans (`true`/`false`; an integral number renders without a fraction) and message lists for placeholders."
5. **Changelog** `### Added`: `- SDK prompts (D-V-14, D-V-15): get_prompt / getPrompt with a 60 s cache, stale-while-revalidate and a fallback, Prompt.compile, prompt links through context(prompt=…) / withContext({ prompt }), and client.prompts.` **Roadmap**: V row lists M2 as implemented.

### Done when

`just check-docs` (with the Node `PATH` export) passes, then `just check`.
