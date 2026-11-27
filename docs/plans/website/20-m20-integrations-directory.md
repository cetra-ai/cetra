# M20: integrations directory

Status: planned 2026-11-27. Three phases, D1–D3, done in order, one commit each.

**Summary.** `/integrations/` today is seven full-height bands of large cards (82 integrations), with no way to find one and badges that read as internal jargon. M20 makes it one searchable directory: a hero with the setup code (Python, TypeScript, any OTLP exporter), then a search box, group chips, language chips and small tiles. Each tile shows a lettered square, the name, and language chips (`python`, `typescript`, `java`, `kotlin`, `.net`). When a language is picked, the integrations that need no language (coding agents, gateways, n8n, Promptfoo) stay visible in a "Works with any language" strip. The data still comes from the docs' integrations page; the JVM and .NET languages are read from its "How" column, which already starts with the language for those rows.

**Prerequisites**
- The branch `claude/integrations-directory`, cut from `origin/main` (b0019f2), in the worktree `.claude/worktrees/integrations-directory`.

**Read first**
- `docs/website/00-design.md` §3 ("Single sources"), §5 (the Integrations row), §6.7.
- `docs/plans/website/04-m4-integrations.md` (how the list is parsed; why there are no logos).
- `website/src/marketing/data/integrations.mjs` and `integrations.test.mjs`, whole files.
- `website/src/pages/integrations.astro`, whole file.
- `website/src/content/docs/docs/integrations/index.mdx`: the tables' columns, and rows 133–137 (Spring AI, LangChain4j, Quarkus LangChain4j, Koog, Microsoft.Extensions.AI).
- `website/src/marketing/components/Tabs.astro`, `Section.astro`, `SectionHeading.astro`, `Button.astro`; `website/src/marketing/lib/tabs.ts`; `website/src/marketing/styles/base.css` (`.tablist`, `.visually-hidden`).
- `website/scripts/check-marketing.mjs` `checkPage` (every link and button needs a name, every `aria-controls`/`aria-labelledby` id must exist in the HTML, one h1, no skipped heading level, page weight cap 350 kB).

## What it is, and what it is not

When M20 is done:
- `/integrations/` has a hero with the title on the left and a code box with three tabs on the right (stacked on narrow screens).
- Below it, one band holds: a search box, group chips (`All 82`, `Providers 14`, …), language chips (`Any language`, `Python`, `TypeScript`, `JVM`, `.NET`), a live result count and a grid of small tiles. Every tile links to its docs page.
- Search, group and language filters combine. Without JavaScript the controls stay hidden and every tile shows.
- The teal "Anything that speaks OpenTelemetry" band stays.

Out of scope:
- Logos (M4: trademarks; the tiles keep a lettered square).
- Filter state in the URL.
- Changes to the homepage's integrations strip (it reads only `title` and `href`).
- Changes to the docs integrations page itself.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| Design §3 | The integrations list is generated from the docs' matrix, never typed twice |
| M4 | No third-party logos |
| User, 2026-11-27 | Option A + B: setup code in the hero, then a searchable directory of small tiles |
| User, 2026-11-27 | Language chips on each tile, and a language filter |
| User, 2026-11-27 | JVM and .NET integrations get `java`, `kotlin`, `.net` chips; under a language filter, language-free integrations stay visible in a "Works with any language" strip |
| User, 2026-11-27 | The "Recorded traces / SDK test" badges leave the tiles; the tile's tooltip keeps them |

## Versions

No new dependency.

## Phases

```
D1 (data: languages) ──► D2 (filter logic) ──► D3 (page)
```

D2 does not import D1's code but uses its `Language` values; D3 needs both. Run them in order.

| Phase | Files |
|---|---|
| D1 | `website/src/marketing/data/integrations.mjs`, `website/src/marketing/data/integrations.test.mjs`, `website/src/pages/integrations.astro` |
| D2 | new `website/src/marketing/lib/integrations-filter.mjs`, new `website/src/marketing/lib/integrations-filter.test.mjs` |
| D3 | `website/src/pages/integrations.astro`, `docs/website/00-design.md`, `docs/plans/website/README.md` |

**Checks while iterating** (machine load rule): only the touched tests and Biome. No `astro build`, no `just check-docs` in a phase; the coordinator runs `just check-docs` once at the end.

Node is not on the default PATH: run `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` first.

---

## D1: languages in the integrations data

**Goal:** each integration carries `languages`, read from the Python and TypeScript columns and from the "How" column's leading `Java, `, `Kotlin, ` or `.NET, `; the `python` and `typescript` booleans are gone.

**Edit:**
- `website/src/marketing/data/integrations.mjs`
- `website/src/marketing/data/integrations.test.mjs`
- `website/src/pages/integrations.astro` (only the two badge lines, step 5)

**Steps**

1. In `integrations.mjs`, add above the `Integration` typedef:

```js
/** @typedef {"python" | "typescript" | "java" | "kotlin" | "dotnet"} Language */

/** The chip text of each language, in display order. */
export const LANGUAGE_CHIPS = {
  python: "python",
  typescript: "typescript",
  java: "java",
  kotlin: "kotlin",
  dotnet: ".net",
};

/** The "How" cell prefix that names a language the table has no column for. */
const HOW_LANGUAGES = [
  ["Java, ", "java"],
  ["Kotlin, ", "kotlin"],
  [".NET, ", "dotnet"],
];
```

2. In the `Integration` typedef, replace the two lines

```js
 * @property {boolean} python   ✓ in the Python column
 * @property {boolean} typescript ✓ in the TypeScript column
```

with

```js
 * @property {Language[]} languages ✓ columns and the "How" cell's leading language, in `LANGUAGE_CHIPS` order; empty for a tool used from any language (a coding agent, a gateway)
```

3. Add an exported function at the end of the file, and its tests (below):

```js
/**
 * The tile's lettered square: the first ASCII letter or digit of the title, upper-cased.
 * @param {string} title
 */
export function monogram(title) {
  const match = /[A-Za-z0-9]/.exec(title);
  return match ? match[0].toUpperCase() : "?";
}
```

   Then add a function (not exported) below `parseTested`:

```js
/**
 * @param {boolean} python
 * @param {boolean} typescript
 * @param {string} how the "How" cell
 * @returns {Language[]}
 */
function languagesOf(python, typescript, how) {
  /** @type {Language[]} */
  const languages = [];
  if (python) languages.push("python");
  if (typescript) languages.push("typescript");
  for (const [prefix, language] of HOW_LANGUAGES) {
    if (how.startsWith(prefix)) languages.push(language);
  }
  return languages;
}
```

4. In `parseIntegrations`:
   - Change the row destructuring to `const [, , href, python, typescript, how, tested] = match;`.
   - Change the `rows` map's value type comment to `{languages: Language[], tests: Test[]}`.
   - Store `languages: languagesOf(python === "✓", typescript === "✓", how)` instead of the `python` and `typescript` fields.
   - Nothing else in the function changes; the returned object spreads `row`, so it carries `languages`.

5. In `website/src/pages/integrations.astro`, replace

```astro
{item.python && <span class="badge">Python</span>}
{item.typescript && <span class="badge">TypeScript</span>}
```

with

```astro
{item.languages.includes("python") && <span class="badge">Python</span>}
{item.languages.includes("typescript") && <span class="badge">TypeScript</span>}
```

   (D3 rewrites the page; this keeps D1's commit building.)

6. In `integrations.test.mjs`:
   - The `row` helper keeps its shape; add a third parameter `how = "how"` and use it in the How cell.
   - In "parses the real page", replace the `openai.python` / `openai.typescript` / `bedrock.typescript` assertions with `languages` assertions (below).
   - Replace any other `.python` / `.typescript` use in the file the same way. The test "CRLF line endings parse" (about line 108) compares against an object literal with `python: true, typescript: false`: replace those two fields with `languages: ["python"]`. The test "a hand-written test cell is parsed" (about line 75) asserts `copilot.python === false` and `copilot.typescript === false`: replace both with `assert.deepEqual(copilot.languages, [])`.

**Tests** (in `integrations.test.mjs`):
- `the real page's languages`: OpenAI → `["python", "typescript"]`; Amazon Bedrock → `["python"]`; Vercel AI SDK → `["typescript"]`; Spring AI → `["java"]`; Koog → `["kotlin"]`; Microsoft.Extensions.AI → `["dotnet"]`; Claude Code → `[]`; Bifrost → `[]`.
- `every real integration's languages are known and unique`: for each item, every value is a key of `LANGUAGE_CHIPS` and there are no duplicates.
- `a How cell's leading language is read`: `parseIntegrations` on one row `| [X](/docs/integrations/x/) | — | — | Java, a starter | captured |` plus its card gives `languages: ["java"]`.
- `a language named later in the How cell is not read`: How cell `uses Java, sometimes` with both columns `—` gives `[]`.
- `the columns come before the How language`: How `.NET, x` with Python `✓` gives `["python", "dotnet"]`.
- `no item has the old booleans`: `"python" in item` and `"typescript" in item` are false for every real item.
- `monogram`: `"OpenAI"` → `"O"`; `".NET thing"` → `"N"`; `"9router"` → `"9"`; `"…"` → `"?"`; `"élan"` → `"L"` (accented letters are skipped).

**Done when:**
- `cd website && node --test src/marketing/data/` passes.
- `cd website && ../node_modules/.bin/biome check --write --diagnostic-level=error src/marketing/data/ src/pages/integrations.astro` reports no error.
- From the repo root, `rg -n "\bpython:|\btypescript:|\.python\b|\.typescript\b" website/src/marketing website/src/pages` finds nothing that reads or builds an integration's old booleans; the two `LANGUAGE_CHIPS` lines (`python: "python"`, `typescript: "typescript"`) are expected hits.

**Commit:** `Read integration languages from the docs table, JVM and .NET included`

---

## D2: the directory's filter logic

**Goal:** one pure function decides which tiles show for a group, a language and a search text, tested without a browser.

**Edit:**
- new `website/src/marketing/lib/integrations-filter.mjs`
- new `website/src/marketing/lib/integrations-filter.test.mjs`

**Steps**

1. `integrations-filter.mjs` (plain JS with JSDoc, no imports; it runs both in `node --test` and in the page's bundled script):

```js
/** @typedef {"python" | "typescript" | "java" | "kotlin" | "dotnet"} Language */

/**
 * @typedef {object} FilterItem
 * @property {string} group
 * @property {string} title
 * @property {string} description
 * @property {Language[]} languages empty: used from any language
 */

/** The language chips, in order. `matches` empty: no language filter. */
export const LANGUAGE_FILTERS = [
  { id: "any", label: "Any language", matches: [] },
  { id: "python", label: "Python", matches: ["python"] },
  { id: "typescript", label: "TypeScript", matches: ["typescript"] },
  { id: "jvm", label: "JVM", matches: ["java", "kotlin"] },
  { id: "dotnet", label: ".NET", matches: ["dotnet"] },
];

/**
 * Which items show.
 * @param {FilterItem[]} items
 * @param {{ group: string | null, language: string, query: string }} filter
 *   group null: every group; language: a `LANGUAGE_FILTERS` id; query: the search box's text
 * @returns {{ matched: number[], anyLanguage: number[] }} indexes into `items`, ascending.
 *   `matched`: the main grid. `anyLanguage`: the "Works with any language" strip, always empty
 *   when `language` is "any".
 */
export function filterIntegrations(items, filter) { … }
```

2. `filterIntegrations` body, in this order:
   1. Find the `LANGUAGE_FILTERS` entry whose `id` equals `filter.language`; if none, `throw new Error(\`unknown language filter: ${filter.language}\`)`.
   2. Split `filter.query.trim().toLowerCase()` on `/\s+/`, dropping empty strings, into `tokens`.
   3. For each item, in order, with index `i`:
      - Skip it when `filter.group !== null && item.group !== filter.group`.
      - Skip it unless every token is a substring of `` `${item.title} ${item.description} ${item.group}`.toLowerCase() ``. No tokens: every item passes.
      - If the language entry's `matches` is empty: push `i` to `matched`.
      - Else if `item.languages.length === 0`: push `i` to `anyLanguage`.
      - Else if some `item.languages` value is in `matches`: push `i` to `matched`.
      - Else: skip it.
   4. Return `{ matched, anyLanguage }`.

**Tests** (`integrations-filter.test.mjs`, `node:test` + `node:assert/strict`, on a fixed list of about six items covering a Python provider, a TypeScript framework, a Python+TypeScript framework, a Kotlin framework, a Java framework and a coding agent with no language):
- `no filter shows everything in order`.
- `a group filter keeps that group only`.
- `the search matches title, description and group, case-insensitively`.
- `every search word must match` (two words, only one item has both).
- `blank and whitespace-only searches match everything`.
- `a language filter moves language-free items to anyLanguage`.
- `JVM matches Java and Kotlin`.
- `an item in both Python and TypeScript matches either filter`.
- `group, language and search combine` (Coding agents + Python + a word: matched empty, anyLanguage holds the agent).
- `under "any", anyLanguage is empty`.
- `an unknown language id throws`.
- `the language ids are unique and "any" comes first`.
- `every filtered language has a chip`: import `LANGUAGE_CHIPS` from `../data/integrations.mjs` (a test-only import) and assert every `matches` value of every `LANGUAGE_FILTERS` entry is a key of it, and every key of it is in some entry's `matches`.

**Done when:**
- `cd website && node --test src/marketing/lib/` passes.
- `cd website && ../node_modules/.bin/biome check --write --diagnostic-level=error src/marketing/lib/` reports no error.

**Commit:** `Add the integrations directory's filter logic`

---

## D3: the page

**Goal:** `/integrations/` becomes the hero with setup code plus one searchable directory of small tiles, as the user approved.

**Edit:**
- `website/src/pages/integrations.astro`
- `docs/website/00-design.md`
- `docs/plans/website/README.md`

**Steps**

1. Frontmatter of `integrations.astro`:
   - Keep the imports of `Button`, `Section`, `SectionHeading`, `MarketingLayout`; add `Tabs` from `../marketing/components/Tabs.astro`; drop `Card`.
   - Import `{ groups, LANGUAGE_CHIPS, loadIntegrations, MIGRATING_GROUP, monogram }` from `../marketing/data/integrations.mjs` and `{ LANGUAGE_FILTERS }` from `../marketing/lib/integrations-filter.mjs`.
   - Keep `testLabel` as it is.
   - Compute:
     - `items = loadIntegrations()`.
     - `groupChips = groups(items).map((group) => ({ group, count: items.filter((i) => i.group === group).length }))`.
     - `anyLanguageItems = items.map((item, index) => ({ item, index })).filter(({ item }) => item.languages.length === 0)`.
     - `guides = items.filter((i) => i.group === MIGRATING_GROUP).length` and `integrationCount = items.length - guides` (the lede counts them apart; the directory lists both, as today, so the "All" chip shows `items.length`, which is `integrationCount + guides`; this difference is intended).
     - `tooltip(item)`: `` `${item.description} Tested against: ${item.tests.map(testLabel).join(", ")}.` ``.
     - `filterData = JSON.stringify(items.map(({ group, title, description, languages }) => ({ group, title, description, languages }))).replaceAll("<", "\\u003c")` (so no docs text can close the script tag).

2. Hero, a `Section tone="ink" pad="md" labelledby="integrations-title"` holding a `div.hero` with two children:
   - `SectionHeading` with `level={1}`, `eyebrow="Integrations"`, `title="Works with your stack."`, `id="integrations-title"`, and `lede={\`${integrationCount} integrations and ${guides} migration guides, each with a setup page and tested against real traces or SDK tests. Anything else that speaks OpenTelemetry works too.\`}`.
   - `div.setup` (`min-width: 0`, so a long line scrolls inside the code box) with a `Tabs id="setup" label="Set up cetra" labels={["Python", "TypeScript", "Any OTLP exporter"]}`, three `<div data-tab-panel>` children, each a `<pre class="code"><code>…</code></pre>`, then, after the closing `</Tabs>` and still inside `div.setup`, `<p class="setup-note">` with the text `OpenAI shown, abridged. Each integration's page has its own lines.` Without JavaScript the three panels all show, one under the other, as the site's other tabs do (Tabs.astro hides them at runtime); accept that.
   - The three code bodies, exactly (in Astro, write them inside `{\`…\`}` template literals so `{` and `}` are not parsed as expressions):

```text
pip install "cetra-sdk[openai]" openai

import cetra
cetra.init(instrument=["openai"])
```

```text
npm install @cetra-ai/sdk @arizeai/openinference-instrumentation-openai openai

import { init } from "@cetra-ai/sdk";
import { openai } from "@cetra-ai/sdk/openai";
import OpenAI from "openai";

init({ apiKey: process.env.CETRA_API_KEY, instrument: [openai(OpenAI)] });
```

```text
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:8080
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer sk-cetra-…"
# then start any OpenTelemetry SDK or collector
```

   These are lines of `docs/integrations/openai.mdx` (install lines 19 and 26, setup lines 41–44 and 60–67) and `docs/get-started/opentelemetry.mdx` (lines 93–94), abridged; do not add others.

3. Directory, a `Section tone="raised" divider labelledby="directory-title"` holding:
   - `<h2 id="directory-title" class="visually-hidden">All integrations</h2>`.
   - `<div class="controls" data-directory-controls hidden>` containing, in order:
     - `<label class="visually-hidden" for="integration-search">Search integrations</label>` and `<input id="integration-search" type="search" class="search" placeholder="Search: OpenAI, LangGraph, Claude Code…" autocomplete="off" />`.
     - `<div class="chips" role="group" aria-label="Group">`: a first `<button type="button" class="chip" data-group-chip="" aria-pressed="true">All {items.length}</button>`, then one per `groupChips` entry: `<button type="button" class="chip" data-group-chip={group} aria-pressed="false">{group} {count}</button>`.
     - `<div class="chips" role="group" aria-label="Language">`: one button per `LANGUAGE_FILTERS` entry, `data-language-chip={id}`, `aria-pressed` `"true"` for `any` and `"false"` otherwise, text `label`.
     - `<p class="count" aria-live="polite" data-directory-count></p>`.
   - `<ul class="tiles" role="list" data-directory-grid>` with one `<li class="tile" data-index={index}>` per item, in `items` order. Inside: `<a href={item.href} title={tooltip(item)}>` holding `<span class="mono" aria-hidden="true">{monogram(item.title)}</span>`, `<span class="name">{item.title}</span>` and, when `item.languages.length > 0`, `<span class="langs">` with one `<span class="lang">{LANGUAGE_CHIPS[language]}</span>` per language.
   - `<div class="any-language" data-any-language hidden>` with `<h3 id="any-language-title">Works with any language</h3>`, a one-line `<p>` `Tools you configure rather than import.`, and a `<ul class="tiles" role="list" aria-labelledby="any-language-title">` with one tile per `anyLanguageItems` entry (same markup as above, same `data-index`). This `<ul>` must not carry `data-directory-grid`: the script selects the main grid's tiles by it.
   - `<p class="empty" data-directory-empty hidden>No integration matches. <a href="#send-title">Anything that speaks OpenTelemetry works</a>.</p>`.
   - `<script type="application/json" id="integrations-data" is:inline set:html={filterData}></script>` (`is:inline` so Astro leaves it as written).
   - Do not give `.tile`, `.controls`, `.any-language` or `.empty` a `display` rule: the script shows and hides them with the `hidden` attribute, which a `display` rule would override. (`.tile a` is the flex box.)

4. Keep the teal "Anything that speaks OpenTelemetry" `Section` unchanged.

5. A `<script>` at the end of the file (Astro bundles it as TypeScript; `astro check` is strict). Write it as:

```ts
import { type FilterItem, filterIntegrations } from "../marketing/lib/integrations-filter.mjs";

const controls = document.querySelector<HTMLElement>("[data-directory-controls]");
const search = document.querySelector<HTMLInputElement>("#integration-search");
const data = document.querySelector<HTMLScriptElement>("#integrations-data");
const strip = document.querySelector<HTMLElement>("[data-any-language]");
const empty = document.querySelector<HTMLElement>("[data-directory-empty]");
const count = document.querySelector<HTMLElement>("[data-directory-count]");

if (controls && search && data && strip && empty && count) {
  const items: FilterItem[] = JSON.parse(data.textContent ?? "[]");
  const mainTiles = Array.from(document.querySelectorAll<HTMLElement>("[data-directory-grid] > li"));
  const stripTiles = Array.from(strip.querySelectorAll<HTMLElement>("li[data-index]"));
  let group: string | null = null;
  let language = "any";

  const render = () => {
    const { matched, anyLanguage } = filterIntegrations(items, { group, language, query: search.value });
    const shown = new Set(matched);
    const shownAny = new Set(anyLanguage);
    for (const tile of mainTiles) tile.hidden = !shown.has(Number(tile.dataset.index));
    for (const tile of stripTiles) tile.hidden = !shownAny.has(Number(tile.dataset.index));
    strip.hidden = anyLanguage.length === 0;
    const total = matched.length + anyLanguage.length;
    empty.hidden = total > 0;
    count.textContent = `${total} of ${items.length} integrations`;
  };

  // Each chip group is a radio-like set: the clicked chip is pressed, its siblings are not.
  const wire = (attribute: string, select: (value: string) => void) => {
    for (const chip of controls.querySelectorAll<HTMLButtonElement>(`[${attribute}]`)) {
      chip.addEventListener("click", () => {
        select(chip.getAttribute(attribute) ?? "");
        for (const sibling of chip.parentElement?.querySelectorAll<HTMLButtonElement>(`[${attribute}]`) ?? []) {
          sibling.setAttribute("aria-pressed", sibling === chip ? "true" : "false");
        }
        render();
      });
    }
  };
  wire("data-group-chip", (value) => {
    group = value === "" ? null : value;
  });
  wire("data-language-chip", (value) => {
    language = value;
  });
  search.addEventListener("input", render);

  controls.hidden = false;
  render();
}
```

   `FilterItem` is a JSDoc typedef in the `.mjs` file; if `astro check` cannot import it as a type, declare the same shape locally in the script (`{ group: string; title: string; description: string; languages: string[] }`) and say so in the report. Run Biome on the file and keep its formatting.

6. Styles (scoped `<style>`), using the site's tokens only:
   - `.hero`: grid, `gap: var(--space-7)`; at `min-width: 960px`, `grid-template-columns: minmax(0, 1fr) minmax(0, 1fr)` and `align-items: center`.
   - `.code`: `margin: var(--space-3) 0 0`, `padding: var(--space-4)`, `background: var(--c-ink-2)`, `border: 1px solid var(--line)`, `border-radius: var(--radius)`, `font-family: var(--font-mono)`, `font-size: var(--text-sm)`, `line-height: 1.7`, `overflow-x: auto`, `white-space: pre`.
   - `.setup-note`: `font-size: var(--text-xs)`, `color: var(--fg-muted)`, `margin-top: var(--space-2)`.
   - `.search`: full width, `padding: var(--space-3) var(--space-4)`, `background: var(--surface)`, `border: 1px solid var(--line-strong)`, `border-radius: var(--radius)`, `color: var(--fg)`, `font: inherit`.
   - `.chips`: flex, wrap, `gap: var(--space-2)`, `margin-top: var(--space-4)`. `.chip`: `font-size: var(--text-xs)`, `padding: 4px 12px`, `border: 1px solid var(--line-strong)`, `border-radius: 999px`, transparent background, `color: var(--fg-muted)`, `cursor: pointer`; `.chip[aria-pressed="true"]`: `border-color: var(--accent)`, `color: var(--accent)`.
   - `.count`: `font-size: var(--text-xs)`, `color: var(--fg-muted)`, `margin-top: var(--space-4)`.
   - `.tiles`: `list-style: none`, `margin: var(--space-4) 0 0`, `padding: 0`, grid `repeat(auto-fill, minmax(200px, 1fr))`, `gap: var(--space-2)`.
   - `.tile a`: flex, `align-items: center`, `gap: var(--space-3)`, `padding: var(--space-3)`, `background: var(--surface)`, `border: 1px solid var(--line)`, `border-radius: var(--radius)`, `color: var(--fg)`, `text-decoration: none`, `height: 100%`, `box-sizing: border-box`, `min-width: 0`; hover and `:focus-visible`: `border-color: var(--accent)`; `:focus-visible` also `outline: 2px solid var(--focus); outline-offset: -2px` (the band has `overflow: hidden`, so an outer ring would be clipped). `.chip:focus-visible` and `.search:focus-visible` get the same outline.
   - `.mono`: 28 px square, `flex: none`, `border-radius: var(--radius-sm)`, `background: var(--surface-2)`, `color: var(--accent)`, `font-weight: 600`, `font-size: var(--text-sm)`, centered.
   - `.name`: `font-size: var(--text-sm)`, ellipsis on one line (`white-space: nowrap; overflow: hidden; text-overflow: ellipsis`). Wrap `.name` and `.langs` in a `<span class="text">` with `min-width: 0` and `display: grid; gap: 2px` (adjust step 3's tile markup accordingly).
   - `.langs`: flex, wrap, `gap: 4px`. `.lang`: `font-family: var(--font-mono)`, `font-size: 11px`, `color: var(--fg-muted)`, `border: 1px solid var(--line)`, `border-radius: 999px`, `padding: 0 6px`.
   - `.any-language`: `margin-top: var(--space-7)`; its `h3`: `font-size: var(--text-lg)`; its `p`: `color: var(--fg-muted)`, `font-size: var(--text-sm)`.
   - `.empty`: `margin-top: var(--space-6)`, `color: var(--fg-muted)`.
   - Remove the old `.badges`, `.badge` and the old `.tiles` rules.

7. `docs/website/00-design.md`:
   - §5, the Integrations row (line 120): replace its middle cell with `Since 2026-11-27 (the user): a hero with the setup code (Python, TypeScript, any OTLP exporter), then one searchable directory: search, group chips, language chips (Python, TypeScript, JVM, .NET) and small tiles with a lettered square, the name and language chips, each linking to its docs page; under a language filter, the integrations used from any language (coding agents, gateways) show in a "Works with any language" strip; what each is tested against is in the tile's tooltip`.
   - §6.7 and M4's row stay.

8. `docs/plans/website/README.md`: in the "nineteen milestones, M1–M19" sentence write "twenty milestones, M1–M20", and add, following the header's column order (Milestone | Plan | Status), not M16–M19's, the row `| M20 Integrations directory | [\`20-m20-integrations-directory.md\`](20-m20-integrations-directory.md) | done (D1–D3) |` after M19's.

**Tests:** the page has no unit tests; `check-marketing.mjs` covers its names, ids, headings and weight. Check by hand (step "Done when").

**Done when:**
- `cd website && ../node_modules/.bin/biome check --write --diagnostic-level=error src/pages/integrations.astro` reports no error.
- `cd website && node --test src/marketing/` passes.
- Report to the coordinator, who runs `just check-docs` and checks the page in a browser at 375, 1100 and 1440 px: the controls appear, chips toggle `aria-pressed`, a search for `lang` narrows the grid, "Coding agents" + "Python" shows an empty main grid and the agents in the strip, nothing scrolls sideways.

**Commit:** `Make the integrations page a searchable directory with setup code`
