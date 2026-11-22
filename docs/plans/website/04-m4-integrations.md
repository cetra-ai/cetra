# M4: integrations

Status: planned 2026-11-20. Two phases, I1–I2, done in order, one commit each.

**Summary.** `/integrations/` lists every framework, provider and instrumentation library cetra works with, and the homepage gets its integrations section (design §6.7). The list is never typed twice: it is parsed at build time from the docs' integrations page, `website/src/content/docs/docs/integrations/index.mdx`, which already holds the matrix and the groups (track W).

**Prerequisites**
- M3 done on `claude/website-m3`.
- The branch `claude/website-m4`, cut from `claude/website-m3`.

**Read first**
- `docs/website/00-design.md` §3 ("Single sources"), §5, §6.7.
- `docs/plans/website/README.md`.
- The M1 plan (components, checks).
- `website/src/content/docs/docs/integrations/index.mdx`, the whole file.
- `website/src/marketing/home/` (M2).

## What it is, and what it is not

When M4 is done:
- `/integrations/` exists, grouped like the docs (Providers, Frameworks, Instrumentation libraries, Migrating). Each item shows its description, its Python and TypeScript support, what it is tested against, and a link to its docs page.
- The homepage has an integrations strip linking to `/integrations/`.

M4 does **not** use third-party logos. They are other companies' trademarks, and they would need files and permission. Items are text tiles.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-W-6 | Integration pages state what backs them (captured fixtures or SDK tests) |
| Design §3 | The integrations list is generated from the docs' matrix, not typed twice |

## Versions

No new dependency.

---

## I1: parse the docs' integrations page

**Goal:** `loadIntegrations()` returns the docs' integrations as data, with tests on the real file.

**Edit:**
- new `website/src/marketing/data/integrations.mjs`
- new `website/src/marketing/data/integrations.test.mjs`

**Steps**

1. `integrations.mjs` exports:

```js
/**
 * @typedef {object} Integration
 * @property {string} group     the `## ` heading it sits under, e.g. "Providers"
 * @property {string} title     the LinkCard title
 * @property {string} description the LinkCard description
 * @property {string} href      the docs route, e.g. "/docs/integrations/openai/"
 * @property {boolean} python   ✓ in the Python column
 * @property {boolean} typescript ✓ in the TypeScript column
 * @property {string} tested    the "Tested against" cell, e.g. "captured"
 */

/**
 * @param {string} mdx
 * @returns {Integration[]}
 */
export function parseIntegrations(mdx) { /* steps 2–4 */ }

/**
 * Reads the docs page from disk. The cwd must be `website/`: `astro build`, `astro check` and
 * `pnpm --dir website exec node --test` all run there; `node --test` from the repo root does not.
 * @returns {Integration[]}
 */
export function loadIntegrations(path = "src/content/docs/docs/integrations/index.mdx") { /* readFileSync + parseIntegrations */ }

/** The groups in page order. @param {Integration[]} items */
export function groups(items) { /* unique `group` values in first-seen order */ }
```

2. **Table rows.** Every line starting with `| [` must match (throw an `Error` naming the line number otherwise, so a malformed row is never skipped silently). Every line matching `/^\| \[([^\]]+)\]\((\/docs\/integrations\/[a-z0-9-]+\/)\) \| (✓|—) \| (✓|—) \| (.*?) \| (.*?) \|$/` gives `href`, `python`, `typescript` and `tested`. The `tested` value is the 6th capture with backticks removed. The 5th column ("How") is ignored.
3. **Cards.** Walk the lines. A line `## <Group>` sets the current group. A line matching `/<LinkCard title="([^"]+)" description="([^"]+)" href="([^"]+)" \/>/` adds a card `{ group, title, description, href }`.
4. **Join.** Join each card to the table row with the same `href`. Throw an `Error` naming the `href` when a card has no row, or a row has no card. Return the cards in page order with the row's fields.

   Before writing, read the real `index.mdx` and confirm the two regular expressions match its 17 rows and 17 cards exactly. If its format differs (spacing, quotes, the dash character), adapt the expressions to the file and report it.

5. `integrations.test.mjs` (node:test). `loadIntegrations` reads relative to the cwd; the tests run with cwd `website/` (`pnpm --dir website exec node --test …`). Cases:
   - **"parses the real page"**: `loadIntegrations()` returns 17 items. `groups()` returns `["Providers", "Frameworks", "Instrumentation libraries", "Migrating"]`. OpenAI has `python: true`, `typescript: true`, `tested: "captured"`. Amazon Bedrock has `typescript: false` and `tested: "SDK test"`.
   - **"every href has a docs page"**: for each item, `src/content/docs/docs/integrations/<slug>.mdx` or `.md` exists.
   - **"a card without a row throws"** and **"a row without a card throws"**, on small inline MDX strings.
   - **"order follows the page"**: the first item is OpenAI and the last is From Laminar.
   - **"a malformed table row throws"**: an inline MDX with a line `| [X](/docs/integrations/x/) | yes | ✓ | how | captured |` (a `yes` where ✓ or — belongs) throws.

   If the real page has gained or lost integrations by the time you implement this, use the counts and names it has, and report them.

**Done when**
- `pnpm --dir website exec node --test src/marketing/` passes.
- `just check-docs` passes.

**Commit:** `Read the integrations list from the docs page`

---

## I2: the /integrations/ page and the homepage strip

**Goal:** the page and the homepage section, from `loadIntegrations()`.

**Edit:**
- new `website/src/pages/integrations.astro`
- new `website/src/marketing/home/Integrations.astro`
- `website/src/pages/home.astro` (insert the strip)
- `website/src/marketing/routes.mjs` (add `"/integrations/"`)
- `docs/plans/website/README.md` (status)

**Steps**

1. `integrations.astro` (`MarketingLayout title="Integrations" route="/integrations/"`, description `The frameworks, providers and instrumentation libraries cetra works with, over OpenTelemetry, and what each one is tested against.`):
   1. `Section tone="ink" pad="md"`: `SectionHeading level={1}` with eyebrow `Integrations`, title `Works with your stack.`, lede `cetra reads OpenTelemetry. These libraries are tested against recorded traces or SDK tests; anything else that speaks OpenTelemetry works too.`
   2. For each group, in order, a `Section` alternating `tone="raised"` and `tone="ink"` (first raised), each with `divider`:
      - an `h2` (`SectionHeading title={group}`);
      - a grid of tiles (`repeat(auto-fill, minmax(260px, 1fr)); gap: var(--space-4)`). Each tile is a `Card href={item.href}` titled `item.title` (the card's `h3`), with `item.description` and a row of small badges (`font-family: var(--font-mono); font-size: var(--text-xs); border: 1px solid var(--line); border-radius: 999px; padding: 2px 8px; color: var(--fg-muted)`):
        - `Python` when `python`;
        - `TypeScript` when `typescript`;
        - then, checked in this order: when `tested` contains both `captured` and `SDK test` (e.g. LangChain's `captured (Python), SDK test (TypeScript)`), the two badges `Recorded traces` and `SDK test`; else `Recorded traces` when it starts with `captured`; else `SDK test` when it is `SDK test`.
        The badges say what the repository tests, not which language each covers; the docs page has the detail.
   3. `Section tone="teal" divider`: `SectionHeading` with title `Anything that speaks OpenTelemetry`, lede `Point any OTLP exporter at cetra: /v1/traces over HTTP, or gRPC on port 4317.` and `Button href="/docs/get-started/opentelemetry/"` `Send OpenTelemetry`.

   Before writing, check that `/docs/get-started/opentelemetry/` exists, and that the docs state the `/v1/traces` path (`run-cetra.mdx`, the ports table) and port 4317 (`opentelemetry.mdx`). If not, stop with `QUESTION:`.

2. `home/Integrations.astro`: a `Section tone="ink" divider` with `labelledby="integrations-title"`.
   - `SectionHeading` with eyebrow `Integrations`, title `Works with your stack.` (`id="integrations-title"`), lede `OpenTelemetry in, from any framework. Each of these is covered by tests in the repository:`.
   - A wrapping row of pills (titles only), one per integration excluding the `Migrating` group. Each pill is an `a` to `item.href` with `font-size: var(--text-sm); padding: var(--space-2) var(--space-4); border: 1px solid var(--line); border-radius: 999px; color: var(--fg); text-decoration: none`, hover `border-color: var(--accent)`.
   - `Button href="/integrations/" variant="ghost"` `All integrations →`.

3. `home.astro`: insert `<Integrations />` after the Agents section and before DataOwnership (design §6 order).
4. Add `"/integrations/"` to `MARKETING_ROUTES`.
5. Build; run the checks; screenshot `/integrations/` and `/home/` at 1440 px and 390 px (as in the M2 plan, H3 step 4); fix what you see.
6. `docs/plans/website/README.md`: set M4's status to `done (I1–I2)`.

**Tests:** I1's tests; `check-marketing.mjs` covers both pages.

**Done when** `just check-docs` passes and prints `check-marketing: 6 pages ok`.

**Commit:** `Add the integrations page and the homepage strip`
