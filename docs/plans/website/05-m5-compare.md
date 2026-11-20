# M5: comparison pages

Status: planned 2026-11-20. Two phases, C1–C2, done in order, one commit each.

**Summary.** `/compare/` and one honest, dated page per competitor (design §5, §9.3): Langfuse, Arize Phoenix, LangSmith, Braintrust, Helicone, Opik, LangWatch, Laminar and MLflow. Each page is rendered from a JSON fact file. Every fact carries its sources, and the file has a "checked on" date. The planner gathered the facts with research agents; a separate agent verified each one against its source before this plan (design §9.5). A test keeps the files well-formed, and fails the build when a file is older than 183 days, so stale claims get re-checked. The homepage gets a short comparison section (design §6.10).

**Prerequisites**
- M4 done on `claude/website-m4`.
- The branch `claude/website-m5`, cut from `claude/website-m4`.
- The verified fact files in `SCRATCH/compare/` (SCRATCH is `/tmp/claude-1000/-home-sebastien-travaux-llm-observer--claude-worktrees-website-research-c9eb27/36c8a93a-58fa-4df8-91cd-fccb6be3c70f/scratchpad`): `cetra.json` and the nine competitor files, plus `SCHEMA.md`.

**Read first**
- `docs/website/00-design.md` §5, §6.10, §7, §9.
- `docs/plans/website/README.md`.
- The M1 plan.
- `SCRATCH/compare/SCHEMA.md`.
- Two of the fact files (`langfuse.json`, `cetra.json`).

## What it is, and what it is not

When M5 is done:
- `/compare/` lists the nine comparisons.
- `/compare/<slug>/` shows, for one competitor:
  - a neutral one-line description;
  - a table of twelve areas with cetra's value and theirs, each with numbered source links;
  - "When to choose <them>" and "When to choose cetra";
  - how to switch;
  - the "checked on" date and how to report an error.
- The homepage has a "How cetra compares" section on the paper band.

M5 does **not**:
- make performance claims or show benchmarks (M6);
- use other products' logos;
- edit any fact. Values are rendered exactly as the files state them. A fact a phase finds wrong is reported, not changed.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| Design §9.3 | All nine competitors at launch |
| Design §9.5 | Facts verified by an agent against their sources; the "checked on" date is visible |
| Design §7 | Comparison facts need a source URL and a date; the build fails on stale dates |

## Rules for every phase

The rules in `docs/plans/website/README.md` apply: Biome `--write`; `astro build`; the `pageshots.mjs` helper to look at pages.

---

## C1: fact files, loader and checks

**Goal:** the ten fact files in the repo, a typed loader, and tests that keep them valid and fresh.

**Edit:**
- new `website/src/marketing/data/compare/*.json` (ten files)
- new `website/src/marketing/data/compare.mjs`
- new `website/src/marketing/data/compare.test.mjs`

(`compare.mjs` also holds `formatDate`, step 2, so the pages share it.)

**Steps**

1. Copy the ten JSON files from `SCRATCH/compare/` to `website/src/marketing/data/compare/`: `cetra.json`, `langfuse.json`, `phoenix.json`, `langsmith.json`, `braintrust.json`, `helicone.json`, `opik.json`, `langwatch.json`, `laminar.json`, `mlflow.json`. Do not copy `SCHEMA.md`. Run Biome's formatter on them (`biome check --write`); only whitespace may change, never content.

2. `compare.mjs`:

```js
// Comparison fact files (design §7, §9.5). One JSON file per product in ./compare/.
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

/** The areas, in table order, with their row labels. */
export const AREAS = [
  ["selfHostStack", "What you run to self-host"],
  ["selfHostLicence", "Self-hosted licence"],
  ["cloudFree", "Free cloud tier"],
  ["cloudPaidFrom", "Paid cloud from"],
  ["otel", "OpenTelemetry"],
  ["sql", "SQL over your data"],
  ["mcp", "MCP server"],
  ["onlineEvals", "Evaluations on live traffic"],
  ["datasetsExperiments", "Datasets and experiments"],
  ["promptManagement", "Prompt management"],
  ["gateway", "LLM gateway"],
  ["alerts", "Alerts"],
];

/** The competitors, in page order (design §9.3). */
export const COMPETITORS = [
  "langfuse",
  "phoenix",
  "langsmith",
  "braintrust",
  "helicone",
  "opik",
  "langwatch",
  "laminar",
  "mlflow",
];

/** Files older than this many days fail the build (design §7). */
export const MAX_AGE_DAYS = 183;

// Both `astro build` and `node --test` run with the cwd at website/ (import.meta.dirname is not
// reliable once Vite bundles this file).
const DIR = join(process.cwd(), "src/marketing/data/compare");

/**
 * @param {string} slug
 * @returns {Product}
 */
export function loadProduct(slug) {
  return JSON.parse(readFileSync(join(DIR, `${slug}.json`), "utf8"));
}

export function listFiles() {
  return readdirSync(DIR).filter((f) => f.endsWith(".json")).sort();
}

/**
 * Problems with one product file; empty when valid.
 * @param {any} p @param {string} fileSlug @param {Date} now
 */
export function validate(p, fileSlug, now) { /* step 3 */ }

/** "Unknown" is rendered as this (its sources stay listed: they are where we looked). */
export const UNKNOWN_TEXT = "Not found in their documentation";

/** "2026-11-20" -> "20 November 2026". @param {string} iso */
export function formatDate(iso) {
  return new Intl.DateTimeFormat("en-GB", {
    day: "numeric",
    month: "long",
    year: "numeric",
    timeZone: "UTC",
  }).format(new Date(`${iso}T00:00:00Z`));
}

/**
 * @typedef {object} Fact
 * @property {string} value
 * @property {string[]} sources
 */

/**
 * @typedef {object} Product
 * @property {string} slug
 * @property {string} name
 * @property {string} url
 * @property {string} checkedOn
 * @property {string} oneLine
 * @property {Record<string, Fact>} facts
 * @property {string[]} chooseThem
 * @property {string[]} chooseUs
 * @property {string} [notes]
 */
```

3. `validate(p, fileSlug, now)` returns an array of messages:
   - `slug must equal the file name` unless `p.slug === fileSlug`;
   - `name, url, checkedOn and oneLine are required` when any is missing or not a string;
   - `url must start with https://`;
   - `oneLine is N characters, the limit is 140`;
   - `checkedOn must be YYYY-MM-DD` unless it matches `/^\d{4}-\d{2}-\d{2}$/` and is a real date;
   - `checked on <date>, more than 183 days ago: re-check the facts` when `now - checkedOn > MAX_AGE_DAYS` days;
   - for each area in `AREAS`, using the area id as `<area>`:
     - `<area> missing` when absent;
     - `<area>: value is N characters, the limit is 120`;
     - `<area>: no source` when `sources` is empty;
     - `<area>: source must be https or a cetra.sh path` for a source that starts with neither `https://` nor `/`;
   - `facts has an unknown area: <name>` for keys not in `AREAS`;
   - `chooseThem must be an array of strings`, and `chooseThem has N items, 0 to 4 allowed`;
   - `chooseUs must be an array of strings`; for the cetra file `chooseUs must be empty`; for a competitor `chooseUs has N items, 1 to 4 allowed`; each item at most 120 characters (`chooseUs item is N characters, the limit is 120`);
   - banned words in any `value`, `oneLine`, `chooseThem` or `chooseUs` item, with the same expression as `check-marketing.mjs` (`/\b(seamless(ly)?|blazing(ly)?|effortless(ly)?|powerful|simply)\b/i`) → `<field>: banned word <w>`; and `<field>: write cetra in lower case` for `/\b(Cetra|CETRA)\b/`.

4. `compare.test.mjs` (node:test, cwd `website/`):
   - **"the files are exactly cetra plus the nine competitors"**: `listFiles()` equals the sorted list of `cetra.json` plus each `COMPETITORS` slug with `.json`;
   - **"every file is valid today"**: `validate(loadProduct(s), s, new Date())` is empty for every file. This is the staleness check;
   - **"a stale file is reported"**: a copy of `langfuse.json` validated with `now` 200 days after its `checkedOn` gives the `re-check` message;
   - **"a missing source is reported"**: a copy with one area's `sources` emptied;
   - **"a long value is reported"**: 121 characters;
   - **"an unknown area is reported"**;
   - **"a banned word is reported"**;
   - **"slug must equal the file name"**;
   - **"cetra's chooseUs must be empty and a competitor's must not"**;
   - **"cetra's prices match the pricing data"**: import `PLANS`, `OVERAGE_PER_100K` and `formatUnits` from `./pricing.mjs`; `loadProduct("cetra").facts.cloudFree.value` contains `formatUnits(PLANS[0].includedUnits)`, and `cloudPaidFrom.value` contains `"$" + PLANS[1].priceMonthly` and `"$" + OVERAGE_PER_100K + " per 100,000"`, so the comparison and the pricing page cannot drift;
   - **"formatDate"**: `formatDate("2026-11-20") === "20 November 2026"`.

   If a real file fails validation, stop with `QUESTION:` and the messages. Do not edit facts.

**Done when**
- `pnpm --dir website exec node --test src/marketing/` passes.
- `just check-docs` passes.

**Commit:** `Add the comparison fact files and their checks`

---

## C2: the comparison pages and the homepage section

**Goal:** `/compare/`, `/compare/<slug>/` for the nine competitors, and the homepage section.

**Edit:**
- new `website/src/pages/compare/index.astro`
- new `website/src/pages/compare/[slug].astro`
- new `website/src/marketing/compare/FactTable.astro`
- new `website/src/marketing/home/Compare.astro`
- `website/src/pages/home.astro`
- `website/src/marketing/routes.mjs`
- `docs/plans/website/README.md` (status)

**Steps**

1. `FactTable.astro`. Props: `{ them: Product; cetra: Product }`, with `type Product = ReturnType<typeof loadProduct>` (import `loadProduct`, `AREAS` and `UNKNOWN_TEXT` from `../data/compare.mjs`); the rows come from `AREAS`. It renders:
   - a `div.table-wrap` (`overflow-x: auto`) holding a `table` with a `caption` (class `visually-hidden`) `cetra and {them.name} compared`;
   - the `thead` row: `th` `Area`, `th` `cetra`, `th` `{them.name}`;
   - one `tr` per area: a `th scope="row"` with the label, a `td` with cetra's value, and a `td` with theirs. A value equal to `"Unknown"` is rendered as `UNKNOWN_TEXT`, in `.muted`;
   - after each value, one `sup` link per source: `<a href={source} aria-label={`Source ${n}`}>[{n}]</a>`. Numbers follow document order: row by row, and in each row cetra's sources, then theirs. Sources starting with `/` are site links (internal-link checked); `https` sources get `rel="noopener"`.
   - Under the table, `ol.sources` lists every source in the same numbering. Each item is an `a` whose text is the full URL for `https` sources, and `cetra.sh` followed by the path for `/` sources (e.g. `cetra.sh/docs/platform/mcp/`), in `font-family: var(--font-mono); font-size: var(--text-xs); overflow-wrap: anywhere`.
   - Styles: `table { width: 100%; border-collapse: collapse; font-size: var(--text-sm) }`, `th, td { text-align: left; vertical-align: top; padding: var(--space-3) var(--space-4); border-bottom: 1px solid var(--line) }`, `thead th { font-family: var(--font-mono); font-size: var(--text-xs); text-transform: uppercase; letter-spacing: var(--tracking-label); color: var(--fg-muted) }`, `tbody th { font-weight: 600; width: 22% }`, and `td` `width: 39%`. Under 720 px the table keeps `min-width: 640px` and scrolls.

   A source URL that repeats is listed once, keeping its first number.

2. `compare/[slug].astro`:
   - `getStaticPaths()` returns one entry per `COMPETITORS` slug: `{ params: { slug }, props: { slug, them: loadProduct(slug) } }`. The page reads `const { slug, them } = Astro.props;` (not `Astro.params`) and `const cetra = loadProduct("cetra");`. Then: `MarketingLayout title={`cetra vs ${them.name}`} route={`/compare/${slug}/`}`, with the description `` `How cetra and ${them.name} compare on self-hosting, pricing, OpenTelemetry, SQL, evaluations and alerts. Checked on ${them.checkedOn}.` `` (check-marketing enforces ≤ 160; the longest, Arize Phoenix, is 128).
   - Content:
     1. `Section tone="ink" pad="md"`:
        - `SectionHeading level={1}` with eyebrow `Compare`, title `cetra vs {them.name}`, lede `{them.oneLine}`;
        - then `p.muted` (`font-size: var(--text-sm)`): `Checked on {formatted date} against {them.name}'s own documentation, pricing and source. Spotted an error? Write to ` + the mailto `CONTACT_EMAIL` link + `.`
        - The date uses `formatDate(them.checkedOn)` from `compare.mjs`.
     2. `Section tone="raised" divider`, `SectionHeading` title `Side by side`, then `FactTable`.
     3. `Section tone="ink" divider`, two columns (one under 860 px):
        - left: `h2` `When to choose {them.name}` and a `ul` of `them.chooseThem` (when that list is empty, the paragraph `See the table for where {them.name} fits.`);
        - right: `h2` `When to choose cetra` and a `ul` of `them.chooseUs` (written per competitor, each true against that competitor's own facts).
     4. `Section tone="teal" divider`, `SectionHeading` title `Switching to cetra`. Then:
        - when `src/content/docs/docs/integrations/from-${slug}.mdx` exists (`existsSync`, relative to the cwd `website/`; it exists for `langfuse` and `laminar`), the paragraph `Keep the {them.name} SDK and send its spans to cetra as well.` and `Button href={`/docs/integrations/from-${slug}/`}` `Migration guide`;
        - otherwise `cetra reads OpenTelemetry. If your app already sends OTLP, point the exporter at cetra.` and `Button href="/docs/get-started/opentelemetry/"` `Send OpenTelemetry`.
        - In both cases, a second `Button href={CLOUD.demo} variant="secondary" external srHint="(needs a free account)"` `Try the demo`.

3. `compare/index.astro` (`MarketingLayout title="Compare" route="/compare/"`, description `How cetra compares with Langfuse, Arize Phoenix, LangSmith, Braintrust, Helicone, Opik, LangWatch, Laminar and MLflow.`):
   - `Section tone="ink"` with `SectionHeading level={1}` eyebrow `Compare`, title `How cetra compares.`, lede `Fact by fact, from each product's own documentation, with sources and the date we checked.`
   - `Section tone="raised" divider` with `SectionHeading title="Nine comparisons"` (the `h2` that the card `h3`s need), then a grid (`repeat(auto-fill, minmax(280px, 1fr)); gap: var(--space-4)`) of `Card href={`/compare/${slug}/`}` titled `cetra vs {name}`, body `{oneLine}`, then `p.muted` `Checked on {formatDate(checkedOn)}`.

4. `home/Compare.astro`, `Section tone="paper" divider labelledby="compare-title"`:
   - `SectionHeading` with eyebrow `Compare`, title `How cetra compares.` (`id="compare-title"`), lede `Four facts that matter when you run it yourself. Full comparisons, with sources, for nine products.`
   - A table inside `div.table-wrap` (`overflow-x: auto`). The table has `min-width: 760px`, the first column 16% and the four value columns 21% each. Copy the other `FactTable` styles inline into this component (it has five columns, so it does not use `FactTable`):
     - columns `Area`, `cetra`, `Langfuse`, `Arize Phoenix`, `LangSmith`;
     - rows `selfHostStack`, `selfHostLicence`, `otel` and `sql`;
     - values from the files, `Unknown` rendered as `UNKNOWN_TEXT`;
     - no source links in this short table, but each competitor column header links to its page (`/compare/<slug>/`).
   - `Button href="/compare/" variant="secondary"` `All comparisons`.
   - Insert `<Compare />` in `home.astro` after `CloudOrSelfHost` and before `Faq` (design §6 order).

5. `routes.mjs`: add `"/compare/"` and one `"/compare/<slug>/"` per competitor to `MARKETING_ROUTES`, built from `COMPETITORS` (import it from `./data/compare.mjs`) so the two lists cannot drift.

6. Build; run the checks; run `pageshots.mjs` on `/compare/`, `/compare/langfuse/` and `/home/`, and read the PNGs. Fix what is broken.

7. `docs/plans/website/README.md`: set M5's status to `done (C1–C2)`.

**Tests:** C1's tests; `check-marketing.mjs` now covers 16 pages.

**Done when** `just check-docs` passes and prints `check-marketing: 16 pages ok`. (The 183-day staleness rule runs in `compare.test.mjs` inside `just check-docs`, not in `astro build`.)

**Commit:** `Add the comparison pages`
