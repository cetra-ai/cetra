# W3 — Get started, and tested samples in docs pages

**Commit subject:** `Add the quick start and test the docs' code samples`

**Goal:** a new user can run cetra and see a first trace by following one page, in Python, TypeScript or plain OpenTelemetry. Every Python and TypeScript block in the site is tested or marked untested (D-W-6): the SDK READMEs' test harnesses are generalised to read the site's pages, and `check-samples.mjs` fails an unmarked block.

## Read first

- README § Decisions D-W-6, D-W-14; § Page list › Start; § Source of truth and drift › The checks; § Style guide.
- `website/STYLE.md`.
- `sdks/python/tests/test_readme.py`, `sdks/python/tests/conftest.py` (fixtures `memory`, `http_recorder`, `TEST_API_KEY`), `sdks/python/README.md` § Install, § Quick start, § Configuration.
- `sdks/typescript/src/readme.test.ts`, `sdks/typescript/src/readme-examples.test-helpers.ts`, `sdks/typescript/README.md` § Install, § Quick start, § Configuration, `sdks/typescript/tsconfig.test.json`.
- `docs/self-hosting/README.md`, `install.md`, `configuration.md` (the first-start paragraphs and `CETRA_INIT_*`).
- `docs/spec/02-ingestion.md` § the OTLP endpoints and authentication; `docs/spec/09-api.md` § OTLP.

**Check first:** `sdks/python/tests/test_readme.py` has `MARKER = "<!-- test -->"`; `sdks/typescript/src/readme.test.ts` has the regex `` /^<!-- readme:([a-z0-9-]+) -->$/ ``. If either differs, stop and report.

## Files

| File | Change |
|---|---|
| `sdks/python/tests/test_readme.py` → `sdks/python/tests/test_examples.py` (`git mv`) | README and site pages; `test:otel` blocks |
| `sdks/typescript/src/readme.test.ts` → `examples.test.ts` (`git mv`) | README and site pages |
| `sdks/typescript/src/readme-examples.test-helpers.ts` → `examples.test-helpers.ts` (`git mv`) | marker `// example:<name>`; the quick-start examples of the site |
| `sdks/typescript/README.md` | markers `<!-- example:<name> -->` |
| `sdks/typescript/tsconfig.test.json` | the renamed helper file, if it is listed by name |
| `website/scripts/check-samples.mjs`, `check-samples.test.mjs` (new) | the sample-marker check |
| `website/package.json` | `check:content` runs it |
| `website/src/content/docs/docs/get-started/quickstart.mdx` (new) | Quick start |
| `website/src/content/docs/docs/get-started/run-cetra.mdx` (new) | Run cetra |
| `website/src/content/docs/docs/get-started/opentelemetry.mdx` (new) | Send OpenTelemetry |
| `website/astro.config.mjs` | the Get started group |
| `website/src/content/docs/docs/index.mdx` | a quick-start card |
| `website/STYLE.md` | the markers, no longer "added in W3" |

## Steps

### 1. Markers

| Marker (line right before the fence) | Fence | Tested by |
|---|---|---|
| `{/* test */}` | ```` ```python ```` | `test_examples.py`, through the in-memory SDK pipeline, as the README blocks are today |
| `{/* test:otel */}` | ```` ```python ```` | `test_examples.py`, with `OTEL_EXPORTER_OTLP_ENDPOINT` set to the `http_recorder` URL; passes when the recorder got a `POST /v1/traces` |
| `{/* example:<name> */}` | ```` ```ts ```` | `examples.test.ts`: byte-identical to the body of the function marked `// example:<name>` in `examples.test-helpers.ts`, which runs |
| `{/* untested: <reason> */}` | `python`, `ts`, `tsx`, `js` | nothing; `check-samples.mjs` only checks the reason is not empty |

In the SDK READMEs (plain Markdown) the same markers are HTML comments: `<!-- test -->`, `<!-- test:otel -->`, `<!-- example:<name> -->`, `<!-- untested: <reason> -->`. A fence inside a `<TabItem>` is written at column 0, with its marker on the line before.

### 2. Python: `test_examples.py`

`git mv sdks/python/tests/test_readme.py sdks/python/tests/test_examples.py`, then:

- Sources: `README` (as now) plus every `*.md` and `*.mdx` under `Path(__file__).resolve().parents[3] / "website" / "src" / "content" / "docs"` (sorted, so ids are stable). If that directory does not exist (a packaged sdist test run), test the README only.
- Markers: a block counts when the line before its fence, stripped, is one of `<!-- test -->`, `{/* test */}` (kind `sdk`) or `<!-- test:otel -->`, `{/* test:otel */}` (kind `otel`).
- Slug: `<file stem>-<nearest heading slug>`, with the stem `readme` for the README, and the existing `-2`, `-3` suffixes for repeats. Update `test_required_examples_are_marked` to the new README slugs (`readme-quick-start`, `readme-manual-spans-and-llm-span`, `readme-context-and-baggage`, `readme-scores`) and add `quickstart-python` (the quick start's Python tab, heading "Python") and `opentelemetry-python` (the OTel page's Python section).
- Kind `sdk`: exactly today's `test_readme_block` body.
- Kind `otel`: a new test function `test_otel_block(slug, code, http_recorder, monkeypatch)`: `monkeypatch.setenv("OTEL_EXPORTER_OTLP_ENDPOINT", http_recorder.url)`, `monkeypatch.setenv("CETRA_API_KEY", "sk-cetra-test")`, run the block with `exec` in a fresh namespace (no `cetra` substitution), then assert `any(method == "POST" and path == "/v1/traces" for method, path, _, _ in http_recorder.requests)`. The block must call `provider.shutdown()` (or `force_flush()`) itself so the export happens before the assertion; the page's example does that anyway, since a short script must flush.
- Module docstring: say it tests the README and the docs site (D-W-6), and name both marker forms.

Run `just check-py`.

### 3. TypeScript: `examples.test.ts`

`git mv` both files; then:

- In `examples.test-helpers.ts`, every `// readme:<name>` becomes `// example:<name>`; the docstring says the bodies are mirrored in the README and in the site pages.
- In `examples.test.ts`, `markedBlocks` accepts `<!-- example:<name> -->` and `{/* example:<name> */}`, and `markedBodies` matches `// example:<name>`. Blocks are collected from the README and from every `.md`/`.mdx` under `new URL("../../../website/src/content/docs/", import.meta.url)` (skip the site when the directory does not exist). The same `<name>` may appear in several files (the quick start repeats the README's); each occurrence must equal the body. The set of names across all files must equal the set of marked functions, as today.
- In `sdks/typescript/README.md`, every `<!-- readme:<name> -->` becomes `<!-- example:<name> -->`.
- If `tsconfig.test.json` or `vitest` config names `readme-examples.test-helpers.ts` or `readme.test.ts`, rename there too (`rg -n "readme-examples|readme.test" sdks/typescript`).

The site's TypeScript quick start reuses `quick-start` and `with-context` byte for byte. If a site example needs code the README does not have, add a new marked function and a new entry in `EXAMPLES_BY_NAME` with its assertions.

Run `just check-ts`.

### 4. `check-samples.mjs`

`website/scripts/check-samples.mjs` exports `checkFile(path, text)` and a `main` (same shape as `check-terms.mjs`). For each opening fence whose info string's first word is `python`, `py`, `ts`, `typescript`, `tsx`, `js` or `javascript`, the nearest non-blank line before it must be one of the four markers of step 1 in MDX form (`{/* … */}`); for `untested`, the reason after the colon must be non-empty. For `test`/`test:otel` the fence must be `python`, for `example:` it must be `ts`. Findings: `path:line: <language> block without a test marker (D-W-6)`, and so on.

`check-samples.test.mjs` covers: marked blocks pass; an unmarked `python` block fails; `untested:` with no reason fails; `test` before a `ts` fence fails; `bash` and `toml` fences need nothing; a fence inside a fence is not looked at.

`website/package.json`: `"check:content": "node scripts/check-terms.mjs && node scripts/check-samples.mjs"`.

### 5. The pages

All three pages are `.mdx`, use `import { Tabs, TabItem, Steps, Aside } from "@astrojs/starlight/components";`, and follow STYLE.md.

**`get-started/quickstart.mdx`** (title "Quick start"). Required, in this order:

1. One sentence: what the reader will have at the end (cetra running locally and a first trace in the UI).
2. `<Steps>`:
   1. **Run cetra.** `<Tabs syncKey="run">` with "Docker" (the `docker run` of `install.md` § Docker, with `-e CETRA_INIT_ADMIN_EMAIL=… -e CETRA_INIT_ADMIN_PASSWORD=… -e CETRA_INIT_ORG=… -e CETRA_INIT_PROJECT=… -e CETRA_INIT_SECRET_KEY=sk-cetra-<32 letters or digits>` so there is no setup page and the key is known; tell the reader to pick their own 32 characters), "Binary" (`install.sh`, then `cetra serve` with the same variables) and "From source" (`git clone`, `just build-release`, `./target/release/cetra serve`; an `<Aside>` says this is the only way before the first release).
   2. **Open the UI** at `http://localhost:8080` and sign in with the admin email and password.
   3. **Send a trace.** `<Tabs syncKey="lang">` with "Python", "TypeScript", "OpenTelemetry":
      - Python: `pip install cetra-sdk`; `export CETRA_API_KEY=sk-cetra-…`; the README quick-start code under `{/* test */}` (heading "Python" just above the tabs is not possible inside tabs, so the slug comes from the nearest heading; name the step's heading "Send a trace" and expect the slug `quickstart-send-a-trace`; adjust step 2's required slug list to it).
      - TypeScript: `npm install @cetra-ai/sdk` (also `pnpm add`); the `quick-start` block under `{/* example:quick-start */}`.
      - OpenTelemetry: one sentence and a link to `/docs/get-started/opentelemetry/`.
   4. **See it.** Where the trace appears (Traces page of the project), what to click.
3. "Nothing showing up?": a short checklist (the key starts with `sk-cetra-` and belongs to the project; the base URL and port; the process exited before flushing: call `flush()`/`shutdown()`; `CETRA_ENABLED`; the server log line for a refused request). W13's troubleshooting page will be linked from here when it exists.
4. "Next steps": links to `/docs/get-started/run-cetra/`, `/docs/get-started/opentelemetry/` (later phases add sessions, integrations, evaluation as they land).

Fix step 2's required slugs once the page exists: run `uv run pytest -k quickstart --collect-only -q` and use the slugs it prints.

**`get-started/run-cetra.mdx`** (title "Run cetra"): the methods table of `docs/self-hosting/README.md` (each row links to the file's GitHub URL until W4 moves it; W4 changes the links), ports, first start (setup link in the log, or `CETRA_INIT_*`), where data lives (`CETRA_DATA_DIR`), one process per data directory.

**`get-started/opentelemetry.mdx`** (title "Send OpenTelemetry"): when to use plain OTel (an existing setup, another language); endpoints table (OTLP/HTTP `http://<host>:8080/v1/traces` and `:4318/v1/traces`; OTLP/gRPC `<host>:4317`); authentication header `Authorization: Bearer sk-cetra-…`; protobuf and JSON, gzip; then sections:
- "Python": `opentelemetry-sdk` + `opentelemetry-exporter-otlp-proto-http`, a tracer provider with a `BatchSpanProcessor(OTLPSpanExporter(headers={"Authorization": f"Bearer {os.environ['CETRA_API_KEY']}"}))`, one span with `gen_ai.*` attributes, `provider.shutdown()`. The exporter reads `OTEL_EXPORTER_OTLP_ENDPOINT` (the example must not hard-code the endpoint, so the test can point it at the recorder; tell the reader to `export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:8080`). Marker `{/* test:otel */}`.
- "Node.js": the same with `@opentelemetry/sdk-trace-node` and `@opentelemetry/exporter-trace-otlp-proto`, marked `{/* untested: the TypeScript SDK does not depend on the OTel exporter packages */}`.
- "Go", "Java": short exporter setups, `bash`/`go`/`java` fences (not checked).
- "OpenTelemetry Collector": a `yaml` `otlphttp` exporter with the header, for apps that already send to a collector.
- "What cetra reads": two sentences and, until W7 writes Concepts › Semantic conventions, no link.

### 6. Sidebar and overview

`astro.config.mjs`, the Docs topic `items` becomes:

```js
items: [
  { label: "Overview", link: "/docs/" },
  {
    label: "Get started",
    items: [
      { label: "Quick start", link: "/docs/get-started/quickstart/" },
      { label: "Run cetra", link: "/docs/get-started/run-cetra/" },
      { label: "Send OpenTelemetry", link: "/docs/get-started/opentelemetry/" },
    ],
  },
],
```

`index.mdx`: add a `<LinkCard>` "Quick start" to `/docs/get-started/quickstart/` first in the grid.

## Tests

- `just check-py`: `test_examples.py` runs the README blocks and the two site blocks; `test_required_examples_are_marked` passes with the new slugs.
- `just check-ts`: `examples.test.ts` passes; changing one character of the site's TypeScript quick start makes it fail (try it, revert).
- `just check-docs`: `check-samples.test.mjs` passes; the three pages pass `check-samples.mjs` and `check-terms.mjs`; links validate.

## Done when

- `just check` passes.
- Every `python`/`ts` block in `website/` and in both READMEs has a marker.
- The quick start, followed by hand against a source build (`just build-release`), gives a trace in the UI (do it once and say so in the report).
