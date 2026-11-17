# W11 — SDK references, moved out of the READMEs

**Commit subject:** `Move the SDK guides into the site and test their API coverage`

**Goal:** the Reference topic holds the Python and TypeScript SDK references (six and seven pages, README § Page list › Reference topic). Their content comes from the READMEs, which shrink to install, quick start and a link (D-W-5). Each language gets an API page with one heading per exported name, and a test that fails when an export has no heading or a heading names no export (D-W-8). Every tested README block moves with its section and stays tested.

## Read first

- README § Decisions D-W-5, D-W-6, D-W-8; § Page list › Reference topic (the `reference/python/*` and `reference/typescript/*` rows); § Source of truth and drift › What moves.
- `sdks/python/README.md` and `sdks/typescript/README.md`, all of them.
- `sdks/python/src/cetra/__init__.py` (`__all__`), `sdks/typescript/src/index.ts`, `sdks/typescript/package.json` (`exports`), the subpath entry files (`openai.ts`, `anthropic.ts`, `langchain.ts`, `browser.ts`).
- `sdks/python/tests/test_examples.py`, `sdks/typescript/src/examples.test.ts` (W3).
- `docs/spec/10-sdk.md` (pointers to the READMEs, if any: `rg -n "README" docs/spec/10-sdk.md`).

**Check first:** `rg -c "<!-- test -->" sdks/python/README.md` prints 6 and `rg -c "<!-- example:" sdks/typescript/README.md` prints 5. If not, recount which blocks move and say so in the report.

## Files

| File | Change |
|---|---|
| `website/src/content/docs/docs/reference/python/{index,tracing,context,scores-datasets-experiments,opentelemetry,api}.mdx` (new) | Python reference |
| `website/src/content/docs/docs/reference/typescript/{index,tracing,context,scores-datasets-experiments,opentelemetry,runtimes,api}.mdx` (new) | TypeScript reference |
| `sdks/python/README.md`, `sdks/typescript/README.md` | shrunk |
| `sdks/python/tests/test_examples.py` | the required slugs follow the moved blocks |
| `sdks/python/tests/test_api_reference.py` (new) | Python coverage |
| `sdks/typescript/src/api-reference.test.ts` (new) | TypeScript coverage |
| `docs/spec/10-sdk.md` | pointers to the site |
| `website/astro.config.mjs`, `reference/index.mdx`, pages that link SDK sections | sidebar and links |
| `CHANGELOG.md` | `Changed` line |

## Steps

### 1. Move the sections

Map each README section to its page (README § Page list "Sources" column gives the mapping). Move the text, keep it correct, adapt it:
- headings become the page's `##` sections;
- intra-README anchors (`[Serverless](#serverless)`) become absolute site links;
- `<!-- test -->` becomes `{/* test */}` and `<!-- example:<name> -->` becomes `{/* example:<name> */}`, on the line right before the fence;
- tables stay tables; the configuration tables go on the `index.mdx` pages;
- nothing is dropped: every README paragraph lands on exactly one page. Keep a checklist while moving and include it in the report.

The README after the move:

```markdown
# Python SDK            (or "# cetra for TypeScript", the current title)

<the current one-paragraph intro>

## Install
<the current Install section, unchanged>

## Quick start
<the current Quick start, its tested block unchanged>

## Documentation

The full guide and reference are at https://cetra.sh/docs/reference/python/ (configuration,
tracing, context, scores, datasets and experiments, OpenTelemetry, the API).
```

(TypeScript: `https://cetra.sh/docs/reference/typescript/`.) The README's quick-start block keeps its marker, so it stays tested.

`test_examples.py`'s `test_required_examples_are_marked`: the required slugs become `readme-quick-start` plus the site slugs of the moved manual-spans, context and scores blocks (find them with `uv run pytest -k test_readme_block --collect-only -q`, now `test_sdk_block` if W3 renamed it).

### 2. The API pages

**`reference/python/api.mdx`** (title "API (Python)"): one intro sentence; one `### \`<name>\`` heading per name of `cetra.__all__`, in `__all__` order, each with the signature in a `python` fence marked `{/* untested: signature */}`, one paragraph, and a link to the guide section that explains it. Classes list their public methods as `####` headings under them, not checked.

**`reference/typescript/api.mdx`** (title "API (TypeScript)"): a `##` section per entry point (`@cetra-ai/sdk`, `@cetra-ai/sdk/openai`, `/anthropic`, `/langchain`, `/browser`), then one `### \`<name>\`` per export of that entry point (values and types), with the signature in a `ts` fence marked `{/* untested: signature */}`.

### 3. The coverage tests

`sdks/python/tests/test_api_reference.py`:

```python
"""The site's Python API page names every public name and nothing else (D-W-8)."""

import re
from pathlib import Path

import pytest

import cetra

PAGE = (
    Path(__file__).resolve().parents[3]
    / "website/src/content/docs/docs/reference/python/api.mdx"
)


@pytest.mark.skipif(not PAGE.exists(), reason="the docs site is not in this tree")
def test_the_api_page_covers_all() -> None:
    headings = re.findall(r"^### `([A-Za-z_][A-Za-z0-9_]*)`$", PAGE.read_text("utf-8"), re.M)
    assert len(headings) == len(set(headings)), "a name is documented twice"
    assert set(headings) == set(cetra.__all__)
```

`sdks/typescript/src/api-reference.test.ts`: uses the TypeScript compiler API (`typescript` is already a dev dependency) to list each entry point's exports, values and types:

```ts
import { readFileSync } from "node:fs";
import ts from "typescript";
import { describe, expect, it } from "vitest";

const PAGE = new URL(
  "../../../website/src/content/docs/docs/reference/typescript/api.mdx",
  import.meta.url,
);
// Entry point (as the page's `##` heading) → source file, from package.json `exports`.
const ENTRIES: Record<string, string> = {
  "@cetra-ai/sdk": "./index.ts",
  "@cetra-ai/sdk/openai": "./openai.ts",
  "@cetra-ai/sdk/anthropic": "./anthropic.ts",
  "@cetra-ai/sdk/langchain": "./langchain.ts",
  "@cetra-ai/sdk/browser": "./browser.ts",
};

function exportsOf(file: string): string[] {
  const path = new URL(file, import.meta.url).pathname;
  const program = ts.createProgram([path], { strict: true, module: ts.ModuleKind.NodeNext, moduleResolution: ts.ModuleResolutionKind.NodeNext });
  const checker = program.getTypeChecker();
  const source = program.getSourceFile(path);
  if (source === undefined) throw new Error(`no source ${file}`);
  const symbol = checker.getSymbolAtLocation(source);
  if (symbol === undefined) return [];
  return checker.getExportsOfModule(symbol).map((s) => s.getName()).sort();
}

function documented(): Map<string, string[]> { /* split the page on `## \`<entry>\`` headings, collect `### \`<name>\`` under each */ }

describe("API reference page", () => {
  it("documents every export of every entry point, and nothing else", () => {
    const page = documented();
    expect([...page.keys()].sort()).toEqual(Object.keys(ENTRIES).sort());
    for (const [entry, file] of Object.entries(ENTRIES)) {
      expect(page.get(entry)?.slice().sort(), entry).toEqual(exportsOf(file));
    }
  });
});
```

Write `documented()` in full (the comment says what it does). Check `ENTRIES` against `package.json` `exports` before writing; if they differ, `exports` wins. Skip the test (`it.skipIf`) when the page does not exist. If the compiler API needs the project's `tsconfig.json` options to resolve imports, read them with `ts.readConfigFile` + `ts.parseJsonConfigFileContent` instead of the inline options.

### 4. Specs and links

`docs/spec/10-sdk.md`: where it points readers to the READMEs for user documentation, point to `https://cetra.sh/docs/reference/python/` and `…/typescript/`. Pages written in W7–W10 that link "the README" link the site pages instead (`rg -n "README" website/src/content`).

### 5. Sidebar

Reference topic items, before "REST API":

```js
{
  label: "Python SDK",
  items: [
    { label: "Install and configure", link: "/docs/reference/python/" },
    { label: "Tracing", link: "/docs/reference/python/tracing/" },
    { label: "Context", link: "/docs/reference/python/context/" },
    { label: "Scores, datasets and experiments", link: "/docs/reference/python/scores-datasets-experiments/" },
    { label: "OpenTelemetry", link: "/docs/reference/python/opentelemetry/" },
    { label: "API", link: "/docs/reference/python/api/" },
  ],
},
{
  label: "TypeScript SDK",
  items: [
    { label: "Install and configure", link: "/docs/reference/typescript/" },
    { label: "Tracing", link: "/docs/reference/typescript/tracing/" },
    { label: "Context", link: "/docs/reference/typescript/context/" },
    { label: "Scores, datasets and experiments", link: "/docs/reference/typescript/scores-datasets-experiments/" },
    { label: "OpenTelemetry", link: "/docs/reference/typescript/opentelemetry/" },
    { label: "Runtimes", link: "/docs/reference/typescript/runtimes/" },
    { label: "API", link: "/docs/reference/typescript/api/" },
  ],
},
```

`reference/index.mdx`: two cards.

### 6. CHANGELOG

`Changed`: "The SDK guides moved from the package READMEs to cetra.sh/docs/reference/python/ and …/typescript/; the READMEs keep install and the quick start."

## Tests

- `just check-py`: `test_examples.py` runs every moved block; `test_api_reference.py` passes; renaming a heading makes it fail (try, revert).
- `just check-ts`: `examples.test.ts` and `api-reference.test.ts` pass; the TypeScript package check (`check:package`) still passes with the short README.
- `just check-docs`.

## Done when

- `just check` passes; the README checklist of step 1 shows every paragraph placed; both READMEs are under 80 lines.
