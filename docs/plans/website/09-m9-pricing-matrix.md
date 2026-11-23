# M9: pricing feature matrix

Status: planned 2026-11-23. Two phases, X1–X2, done in order on one branch (`claude/website-pricing-matrix`), one commit each.

**Summary.** `/pricing/` has a Cloud / Self-hosted switcher (PR #227). The Cloud tab shows three plan cards, a ten-bullet "Every plan" list and the calculator. The Self-hosted tab shows two cards. Langfuse, Laminar, Arize, Braintrust and Helicone each put a full feature comparison table under their plan cards. M9 adds one to each tab. On the Cloud tab it has four plan columns and about 60 rows, the limits first, then the features in groups, and it replaces the "Every plan" list. On the Self-hosted tab it has two columns: free and licence. Every row links to the docs page that backs it, or names the decision that sells it. A test checks both.

**Prerequisites**
- The branch `claude/website-pricing-matrix`, cut from `origin/main` at `392cc49` (PR #227 merged).
- `pnpm install --frozen-lockfile` passes.

**Read first**
- `docs/website/00-design.md` §9.9 (the user's answers for this milestone).
- `docs/plans/website/README.md` § Conventions for every phase.
- `website/src/marketing/data/pricing.mjs` and `pricing.test.mjs`: the plan data that this milestone reads.
- `website/src/pages/pricing.astro`, `website/src/marketing/components/Tabs.astro` and `website/src/marketing/pricing/PlanCards.astro`.
- `website/src/marketing/compare/FactTable.astro` and the `.fact-table` rules in `website/src/marketing/styles/base.css`: the existing table look, which is reused.
- `website/STYLE.md`.

## What it is, and what it is not

When M9 is done:
- The Cloud tab of `/pricing/` shows, in order: the plan cards, the calculator, then a section titled "The whole product, on every plan." that holds the Cloud table (Free, Starter, Pro, Enterprise).
- The Self-hosted tab shows its two cards, then the Self-hosted table (Self-hosted, Self-hosted licence).
- In both tables, each column header shows the plan name and its price. Rows are grouped, and the limits group comes first. A cell is a check mark, a dash or a short value. A row label links to its docs page when it has one.
- At 720 px and below, the Cloud table shows one plan column at a time, picked with a row of buttons above it (Free by default). The Self-hosted table shows both columns at every width.
- The ten-bullet "Every plan" list and `EVERY_PLAN` are gone.
- `just check-docs` passes.

M9 does **not**:
- add docs pages for audit logs, SCIM, SLAs or the dedicated cell (§9.9: later, by the user);
- change the plan cards, the calculator, the FAQ or the docs;
- show numbers for ingest rate limits (the control plane's numbers are proposals, so the table says "Standard" or "Higher" as D177 does);
- add tooltips, row filtering or a "show all" toggle.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| Design §9.9.1 | Cloud table: 4 columns (Free, Starter, Pro, Enterprise). Teams add-on features show "Teams add-on" in the Pro cell (user, 2026-11-23) |
| Design §9.9.2 | Self-hosted table: 2 columns, free and licence, with the self-hosted-only rows (user, 2026-11-23) |
| Design §9.9.3 | Enterprise-only rows (dedicated cell, audit logs and SCIM, SLAs) are shown, backed by D177; docs come later (user, 2026-11-23) |
| Design §9.9.4 | About 55 rows; the table replaces the "Every plan" list (user, 2026-11-23). The data has 61 Cloud rows and 57 Self-hosted rows |
| Design §9.9.5, D-W-21 | Planner's reading, for the user to veto: Enterprise gets everything Pro and the Teams add-on get (unlimited seats, projects and rules, retention controls, single sign-on, project roles, the shared channel), and its volume, overage, history and ingest values read "Custom". Starter and Pro show unlimited projects, from the control plane's catalogue |
| D173–D178, D-AQ-10, D-IC-13 | The plan values |

## Versions

No new dependency.

## Phases

```
X1 (data + tests) ──► X2 (component + page)
```

X2 imports X1's module, so they run in order.

---

### X1: feature matrix data

**Goal:** one data module holds every row of both tables, and a test ties each row to a docs page or a decision.

**Edit:**
- `website/src/marketing/data/features.mjs` (new)
- `website/src/marketing/data/features.test.mjs` (new)

**Steps**

1. Create `website/src/marketing/data/features.mjs` with exactly this content. Biome may reformat it, but the strings and values must stay as written:

```js
// The pricing page's feature tables (design §9.9). Each row links to the docs page that backs it
// (`doc`, a path under website/src/content/docs/docs/) or names the decision that sells it
// (`source`, with a `claim` phrase found in that decision). features.test.mjs checks both, and checks the limit rows against pricing.mjs.
import { ENTERPRISE, formatMoney, PLANS, SELF_HOSTED } from "./pricing.mjs";

/**
 * A table cell: `true` is included, `false` is not, a string is a value such as "30 days".
 * @typedef {boolean | string} Cell
 */

/**
 * @typedef {object} FeatureRow
 * @property {string} label
 * @property {string | null} doc docs page, e.g. "tracing/traces"; null only with `source`
 * @property {string} [source] the decision that backs a row with no docs page, e.g. "D177"
 * @property {string} [claim] with `source`: a phrase of that decision's row that backs this row
 * @property {Cell[]} [cloud] Free, Starter, Pro, Enterprise; absent: not in the Cloud table
 * @property {Cell[]} [selfHosted] Self-hosted, licence; absent: not in the Self-hosted table
 */

/** @typedef {{ id: string, title: string, rows: FeatureRow[] }} FeatureGroup */

/** @typedef {{ name: string, price: string }} Column */

/** The value a Pro cell shows for a feature of the Teams add-on. */
export const TEAMS_CELL = "Teams add-on";

/** @type {Column[]} */
export const CLOUD_COLUMNS = [
  ...PLANS.map((plan) => ({ name: plan.name, price: `${formatMoney(plan.priceMonthly)} / month` })),
  { name: ENTERPRISE.name, price: "Custom" },
];

/** @type {Column[]} */
export const SELF_HOSTED_COLUMNS = [
  { name: SELF_HOSTED.name, price: formatMoney(SELF_HOSTED.priceYearly) },
  { name: SELF_HOSTED.licence.name, price: `${formatMoney(SELF_HOSTED.licence.priceYearly)} / year` },
];

const ALL = [true, true, true, true];
const BOTH = [true, true];
const U = "Unlimited";
const TEAMS = [false, false, TEAMS_CELL, true];
const LICENCE = [false, true];
const ONE_THEN_UNLIMITED = ["1", U, U, U];

/** @type {FeatureGroup[]} */
export const FEATURE_GROUPS = [
  {
    id: "limits",
    title: "Plan limits",
    rows: [
      { label: "Spans and scores a month", doc: null, source: "D177", claim: "100k units/month", cloud: ["100,000", "200,000", "200,000", "Custom"], selfHosted: ["No limit", "No limit"] },
      { label: "Above the included amount", doc: null, source: "D177", claim: "$4 per 100k", cloud: ["Hard limit", "$4 per 100,000", "$4 per 100,000", "Custom"] },
      { label: "Spend cap", doc: null, source: "D174", claim: "optional spend cap", cloud: [false, true, true, true] },
      { label: "History", doc: null, source: "D177", claim: "30 days data access", cloud: ["30 days", "120 days", "5 years", "Custom"], selfHosted: ["No limit", "No limit"] },
      { label: "Seats (viewers are free)", doc: null, source: "D176", claim: "viewers are free and unlimited", cloud: ONE_THEN_UNLIMITED, selfHosted: [U, U] },
      { label: "Projects", doc: null, source: "D-W-21", claim: "unlimited projects", cloud: ONE_THEN_UNLIMITED, selfHosted: [U, U] },
      { label: "Alert rules", doc: "platform/alerts", cloud: ONE_THEN_UNLIMITED, selfHosted: [U, U] },
      { label: "Active online evaluation rules", doc: null, source: "D178", claim: "1 active online-evaluation rule", cloud: ONE_THEN_UNLIMITED, selfHosted: [U, U] },
      { label: "Annotation queues", doc: "evaluation/annotation-queues", cloud: ONE_THEN_UNLIMITED, selfHosted: [U, U] },
      { label: "Issue sources", doc: "evaluation/issues", cloud: ONE_THEN_UNLIMITED, selfHosted: [U, U] },
      { label: "Ingest rate limit", doc: null, source: "D177", claim: "higher ingest rate limits", cloud: ["Standard", "Standard", "Higher", "Custom"], selfHosted: ["No limit", "No limit"] },
    ],
  },
  {
    id: "tracing",
    title: "Tracing",
    rows: [
      { label: "Trace tree and timeline", doc: "tracing/traces", cloud: ALL, selfHosted: BOTH },
      { label: "Sessions and users", doc: "tracing/sessions", cloud: ALL, selfHosted: BOTH },
      { label: "Cost and token tracking", doc: "tracing/cost-and-tokens", cloud: ALL, selfHosted: BOTH },
      { label: "Metadata, tags and environments", doc: "tracing/metadata-and-tags", cloud: ALL, selfHosted: BOTH },
      { label: "Images, audio and files", doc: "tracing/media", cloud: ALL, selfHosted: BOTH },
      { label: "Project dashboard", doc: "tracing/dashboard", cloud: ALL, selfHosted: BOTH },
    ],
  },
  {
    id: "search",
    title: "Search, SQL and dashboards",
    rows: [
      { label: "Filters, full-text search and saved views", doc: "tracing/filters-and-search", cloud: ALL, selfHosted: BOTH },
      { label: "SQL console with saved queries", doc: "tracing/sql-console", cloud: ALL, selfHosted: BOTH },
      { label: "Custom dashboards", doc: "tracing/custom-dashboards", cloud: ALL, selfHosted: BOTH },
    ],
  },
  {
    id: "evaluation",
    title: "Evaluation",
    rows: [
      { label: "Code checks: regex, JSON schema and more", doc: "evaluation/evaluators", cloud: ALL, selfHosted: BOTH },
      { label: "LLM judges on your own model connection", doc: "evaluation/evaluators", cloud: ALL, selfHosted: BOTH },
      { label: "Webhook evaluators", doc: "evaluation/evaluators", cloud: ALL, selfHosted: BOTH },
      { label: "Evaluation rules on live traffic", doc: "evaluation/online-rules", cloud: ALL, selfHosted: BOTH },
      { label: "Spend caps per judge rule", doc: "evaluation/online-rules", cloud: ALL, selfHosted: BOTH },
      { label: "Scores from code, the API or the UI", doc: "evaluation/scores", cloud: ALL, selfHosted: BOTH },
    ],
  },
  {
    id: "datasets",
    title: "Datasets and experiments",
    rows: [
      { label: "Versioned datasets", doc: "evaluation/datasets", cloud: ALL, selfHosted: BOTH },
      { label: "Datasets from production traces", doc: "evaluation/datasets", cloud: ALL, selfHosted: BOTH },
      { label: "Experiments, compared side by side and run in CI", doc: "evaluation/experiments", cloud: ALL, selfHosted: BOTH },
    ],
  },
  {
    id: "annotation",
    title: "Annotation",
    rows: [
      { label: "Manual scoring of traces, spans and sessions", doc: "evaluation/annotation", cloud: ALL, selfHosted: BOTH },
      { label: "Annotation queues", doc: "evaluation/annotation-queues", cloud: ALL, selfHosted: BOTH },
    ],
  },
  {
    id: "prompts",
    title: "Prompts",
    rows: [
      { label: "Prompt registry with versions and labels", doc: "evaluation/prompts", cloud: ALL, selfHosted: BOTH },
      { label: "Prompt fetch with cache and fallback", doc: "evaluation/prompts", cloud: ALL, selfHosted: BOTH },
      { label: "Playground with four models side by side", doc: "evaluation/playground", cloud: ALL, selfHosted: BOTH },
    ],
  },
  {
    id: "issues",
    title: "Issues and alerts",
    rows: [
      { label: "Errors grouped into issues", doc: "evaluation/issues", cloud: ALL, selfHosted: BOTH },
      { label: "Issues from bad scores and user feedback", doc: "evaluation/issues", cloud: ALL, selfHosted: BOTH },
      { label: "Regression detection", doc: "evaluation/issues", cloud: ALL, selfHosted: BOTH },
      { label: "Alerts to Slack, email and webhooks", doc: "platform/alerts", cloud: ALL, selfHosted: BOTH },
    ],
  },
  {
    id: "agents",
    title: "Agents and integrations",
    rows: [
      { label: "MCP server for coding agents", doc: "platform/mcp", cloud: ALL, selfHosted: BOTH },
      { label: "In-app AI assistant", doc: "platform/assistant", cloud: ALL, selfHosted: BOTH },
      { label: "cetra-cli for setup and queries", doc: "reference/cetra-cli", cloud: ALL, selfHosted: BOTH },
      { label: "Tracing for Claude Code, Codex, Gemini CLI and Copilot", doc: "integrations", cloud: ALL, selfHosted: BOTH },
      { label: "Python and TypeScript SDKs, framework integrations", doc: "integrations", cloud: ALL, selfHosted: BOTH },
      { label: "OpenTelemetry (OTLP) ingest", doc: "reference/otlp", cloud: ALL, selfHosted: BOTH },
      { label: "REST API", doc: "reference/api/overview", cloud: ALL, selfHosted: BOTH },
    ],
  },
  {
    id: "data",
    title: "Data and privacy",
    rows: [
      { label: "Masking and content capture controls", doc: "tracing/privacy-controls", cloud: ALL, selfHosted: BOTH },
      { label: "Deletion and GDPR erasure", doc: "platform/deletion-and-retention", cloud: ALL, selfHosted: BOTH },
      { label: "Retention controls per project", doc: "platform/deletion-and-retention", cloud: [false, false, true, true], selfHosted: BOTH },
      { label: "Usage page by project and day", doc: "platform/plans-and-licence", cloud: ALL, selfHosted: BOTH },
    ],
  },
  {
    id: "deployment",
    title: "Deployment",
    rows: [
      { label: "One binary, Docker, Compose or Helm", doc: "self-hosting/install", selfHosted: BOTH },
      { label: "S3-compatible storage", doc: "self-hosting/s3", selfHosted: BOTH },
      { label: "Online backup and restore", doc: "self-hosting/backup", selfHosted: BOTH },
      { label: "Sign-in with GitHub, Google or OpenID Connect", doc: "self-hosting/authentication", selfHosted: BOTH },
    ],
  },
  {
    id: "access",
    title: "Access and security",
    rows: [
      { label: "Organizations, projects and roles", doc: "platform/organizations", cloud: ALL, selfHosted: BOTH },
      { label: "API keys with rotation", doc: "platform/api-keys", cloud: ALL, selfHosted: BOTH },
      { label: "Single sign-on per org (OpenID Connect)", doc: "platform/sso", cloud: TEAMS, selfHosted: LICENCE },
      { label: "Enforce single sign-on for your domains", doc: "platform/sso", cloud: TEAMS, selfHosted: LICENCE },
      { label: "Project-level roles", doc: "platform/plans-and-licence", cloud: TEAMS, selfHosted: LICENCE },
      { label: "Audit logs and SCIM", doc: null, source: "D177", claim: "audit logs, SCIM", cloud: [false, false, false, true] },
      { label: "Dedicated cell", doc: null, source: "D177", claim: "dedicated cell", cloud: [false, false, false, true] },
      { label: "Self-hosted licence included", doc: null, source: "D177", claim: "self-hosted licence", cloud: [false, false, false, true] },
    ],
  },
  {
    id: "support",
    title: "Support",
    rows: [
      { label: "GitHub issues and email", doc: "help/support", cloud: ALL, selfHosted: BOTH },
      { label: "Priority support", doc: null, source: "D177", claim: "prioritized support", cloud: [false, false, true, true] },
      { label: "Shared Slack or Teams channel", doc: null, source: "D177", claim: "dedicated Slack or Teams support channel", cloud: TEAMS },
      { label: "Service level agreements", doc: null, source: "D177", claim: "SLAs", cloud: [false, false, false, true] },
    ],
  },
];

/**
 * The docs URL of a row's page: "tracing/traces" → "/docs/tracing/traces/".
 * @param {string} doc
 */
export function docHref(doc) {
  return `/docs/${doc}/`;
}

/**
 * The groups of one table, each with only the rows that table shows; a group left empty is
 * dropped.
 * @param {"cloud" | "selfHosted"} table
 * @returns {{ id: string, title: string, rows: { label: string, href: string | null, cells: Cell[] }[] }[]}
 */
export function groupsFor(table) {
  return FEATURE_GROUPS.map((group) => ({
    id: group.id,
    title: group.title,
    rows: group.rows
      .filter((row) => row[table] !== undefined)
      .map((row) => ({
        label: row.label,
        href: row.doc === null ? null : docHref(row.doc),
        cells: /** @type {Cell[]} */ (row[table]),
      })),
  })).filter((group) => group.rows.length > 0);
}
```

2. Create `website/src/marketing/data/features.test.mjs` with `node:test` and `node:assert/strict`, in the style of `pricing.test.mjs`. Read `docs/mvp-decisions.md` the same way it does. Resolve docs paths against `new URL("../../content/docs/docs/", import.meta.url)`. A `doc` resolves when one of these files exists: `<doc>.md`, `<doc>.mdx`, `<doc>/index.md` or `<doc>/index.mdx`. Tests, each a separate `test(...)` with the name given:
   - **"every row is backed by a docs page or a decision"**: for every row, either `doc` is a non-empty string and `source` and `claim` are absent, or `doc === null` and both `source` and `claim` are non-empty strings.
   - **"every docs page exists"**: every non-null `doc` resolves. The failure message names the row label and the doc.
   - **"every decision source exists and states the claim"**: every `source` matches `/^D[\w-]*\d+$/`; a line starting `| <source> |` exists in `docs/mvp-decisions.md`; and that line includes `claim` (exact, case-sensitive substring). The failure message names the row label, the source and the claim.
   - **"every row is in at least one table, with the right number of cells"**: `cloud` or `selfHosted` is defined. `cloud`, when defined, has `CLOUD_COLUMNS.length` (4) cells, and `selfHosted` has `SELF_HOSTED_COLUMNS.length` (2).
   - **"cells are booleans or short strings"**: each cell is a boolean, or a string of 1 to 24 characters.
   - **"labels are unique within a group"**.
   - **"the columns match pricing.mjs"**: `CLOUD_COLUMNS` names are `["Free", "Starter", "Pro", "Enterprise"]` and prices are `["$0 / month", "$30 / month", "$150 / month", "Custom"]`. `SELF_HOSTED_COLUMNS` names are `["Self-hosted", "Self-hosted licence"]` and prices are `["$0", "$5,400 / year"]`.
   - **"the limit rows match the plans"**: find rows in group `limits` by label.
     - "Spans and scores a month" cloud `[0..3]` equals `PLANS.map((p) => formatUnits(p.includedUnits))`.
     - "History" cloud `[0..3]` equals `PLANS.map((p) => p.dataAccess)`.
     - "Above the included amount": a plan with `overage: false` shows "Hard limit"; the others show `` `${formatMoney(OVERAGE_PER_100K)} per 100,000` ``.
     - "Seats (viewers are free)": Free is `"1"` and `PLANS[0].seats === "1 seat"`; Starter and Pro are "Unlimited" and their `seats` is "Unlimited seats".
     - "Projects": Free is `"1"` and `PLANS[0].projects === "1 project"`.
     - The four rule rows ("Alert rules", "Active online evaluation rules", "Annotation queues", "Issue sources") show `"1"` on Free, and `PLANS[0].rules` includes respectively "1 alert rule", "1 online evaluation rule", "1 annotation queue" and "1 issue source". On Starter and Pro they show "Unlimited", and `rules` is "Unlimited rules".
     - Enterprise (index 3) of "Spans and scores a month", "Above the included amount" and "History" is "Custom".
   - **"Teams add-on rows match the licence"**: every row whose cloud cells are exactly `[false, false, TEAMS_CELL, true]` and which has `selfHosted` has `selfHosted` equal to `[false, true]`. There are exactly 3 such rows with `selfHosted`, matching `SELF_HOSTED.licence.features.length` (3).
   - **"retention controls are Pro and up"**: the row "Retention controls per project" is `[false, false, true, true]` on Cloud, and `true` in both self-hosted columns.
   - **"each table has about 55 to 60 rows"**: the summed row count of `groupsFor("cloud")` is 61 and of `groupsFor("selfHosted")` is 57. (A changed count is a deliberate edit of this test.)
   - **"groupsFor drops rows and empty groups"**: `groupsFor("cloud")` has no group with id `deployment`. `groupsFor("selfHosted")` has no row labelled "Dedicated cell", and its `support` group has exactly 1 row. Every resolved row's `href` is `null` exactly when its source row's `doc` is `null`, and otherwise equals `docHref(doc)`.
   - **"docHref"**: `docHref("tracing/traces") === "/docs/tracing/traces/"`, and `docHref("integrations") === "/docs/integrations/"`.

**Tests:** the list in step 2.

**Done when**
- `pnpm --dir website exec node --test src/marketing/data/features.test.mjs src/marketing/data/pricing.test.mjs` passes.
- `pnpm --dir website exec biome check src/marketing/data/features.mjs src/marketing/data/features.test.mjs` has no errors.
- Nothing outside the **Edit** list changed (`git status`).

**Commit:** `Add the pricing feature matrix data`

---

### X2: feature tables on the pricing page

**Goal:** both tabs of `/pricing/` show their feature table, the Cloud one with a plan picker on phones. The "Every plan" list is removed.

**Edit:**
- `website/src/marketing/pricing/FeatureTable.astro` (new)
- `website/src/pages/pricing.astro`
- `website/src/marketing/data/pricing.mjs` (remove `EVERY_PLAN` only)

**Steps**

1. In `pricing.mjs`, delete `EVERY_PLAN` and its doc comment. Before deleting, check with `grep -rn EVERY_PLAN website/` that only `pricing.astro` and `pricing.mjs` use it. If anything else does, stop and ask.

2. Create `website/src/marketing/pricing/FeatureTable.astro`.

   Props:

   ```ts
   import type { Cell, Column } from "../data/features.mjs"; // the JSDoc typedefs of X1
   interface Props {
     /** The table's id prefix, unique on the page. */
     id: string;
     /** The visually hidden caption. */
     caption: string;
     columns: Column[];
     groups: { id: string; title: string; rows: { label: string; href: string | null; cells: Cell[] }[] }[];
     /** Show one column at a time at 720 px and below, picked with buttons. */
     picker?: boolean;
   }
   ```

   Markup, in this order:
   - A root `<div class="matrix" data-matrix data-picker={picker ? "" : undefined}>`.
   - When `picker` is true, `<div class="picker" role="group" aria-label="Plan to show">` holds one `<button type="button" data-col={i} aria-pressed={i === 0 ? "true" : "false"}>{column.name}</button>` per column.
   - `<table class="fact-table feature-table">` containing:
     - `<caption class="visually-hidden">{caption}</caption>`;
     - a `thead` row (inside `<thead>`): the first cell is `<th scope="col"><span class="visually-hidden">Feature</span></th>`, then one `<th scope="col" data-col={i}>` per column, holding `<span class="plan">{name}</span><span class="price">{price}</span>`;
     - one `<tbody>` per group. Its first row is `<tr class="group"><th scope="rowgroup">{group.title}</th>` followed by one empty `<td data-col={i}></td>` per column, then `</tr>`. No `colspan`: the empty cells hide with the others when the picker shows one column, so the column count stays right. Then one row per feature: `<th scope="row">` holding `<a href={href}>{label}</a>` when `href` is set, else the label as text; then one `<td data-col={i}>` per cell.
   - A cell renders as follows:
     - `true`: `<svg class="check" viewBox="0 0 16 16" width="16" height="16" aria-hidden="true"><path d="M3 8.5l3.2 3L13 4.5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /></svg><span class="visually-hidden">Included</span>`.
     - `false`: `<span class="none" aria-hidden="true">—</span><span class="visually-hidden">Not included</span>`.
     - A string: the text.

   Script (an Astro `<script>`, bundled):
   - For each `[data-matrix][data-picker]` root, take its picker buttons and every element with `[data-col]` inside the `table`.
   - Define `select(i)`. It sets each button's `aria-pressed` to `"true"` exactly when its `data-col` equals `i`. It toggles the class `off` on each table `[data-col]` element whose `data-col` is not `i`, and removes it from the others.
   - Each button's click calls `select(Number(button.dataset.col))`. Call `select(0)` once on load.

   Styles (scoped; cells and headers are elements of this component, so no `:global` is needed except where stated):
   - `.matrix { margin-top: var(--space-7); }`.
   - `.feature-table { table-layout: fixed; }`. `thead th:first-child { width: 34%; }`. Value columns share the rest: no width is set, and fixed layout splits it.
   - **Sticky header.** `Section.astro` sets `overflow: hidden` on `.section`, which stops `position: sticky` from working. So add `:global(.section:has(.matrix)) { overflow: visible; }`. Then `.feature-table thead th`: `position: sticky; top: 65px` (the marketing header is 64 px plus a 1 px border, `Header.astro`), `background: var(--band-bg)`, `z-index: 2`, `vertical-align: bottom`.
   - The value header cells, `thead th[data-col]`, and every `td`: `text-align: center`, `overflow-wrap: anywhere`.
   - `.plan`: `display: block`, `color: var(--fg)`, `font-family: var(--font-display)`, `font-size: var(--text-base)`, `text-transform: none`, `letter-spacing: normal`.
   - `.price`: `display: block`, `margin-top: var(--space-1)`, `text-transform: none`, `letter-spacing: normal`, `color: var(--fg-muted)`.
   - `tr.group th`: `padding-top: var(--space-6)`, `font-family: var(--font-mono)`, `font-size: var(--text-xs)`, `text-transform: uppercase`, `letter-spacing: var(--tracking-label)`, `color: var(--accent)`, `font-weight: 600`. `tr.group th, tr.group td`: `border-bottom: 1px solid var(--line-strong)`.
   - `tbody th[scope="row"]`: `font-weight: 400`. Its link uses `color: var(--fg)`, with an underline in `var(--line-strong)` and `var(--accent)` on hover.
   - `.check { color: var(--accent); vertical-align: middle; }` and `.none { color: var(--fg-muted); }`.
   - `.picker { display: none; }`.
   - `@media (max-width: 900px)`: `td` and `thead th[data-col]` use `font-size: var(--text-xs)` and `padding-inline: var(--space-2)`.
   - `@media (max-width: 720px)`:
     - `.picker` shows as a flex row with `flex-wrap: wrap`, gap `var(--space-1)` and `margin-bottom: var(--space-4)`, in a pill like the hosting tab list in `pricing.astro`: padding `var(--space-1)`, `1px solid var(--line)` border, `border-radius: 999px`, `background: var(--surface)`, `width: fit-content`.
     - Its buttons: `border-radius: 999px`, `padding: var(--space-2) var(--space-3)`, `font-size: var(--text-xs)`, `background: transparent`, `color: var(--fg-muted)`, no border, `cursor: pointer`.
     - `[aria-pressed="true"]` uses `background: var(--accent)` and `color: var(--accent-fg)`.
     - `.off { display: none; }`.
     - `thead th:first-child { width: 60%; }`.
     - `.feature-table tbody th { position: static; }`. This overrides base.css's sticky-left rule, which is for tables that scroll sideways; this one doesn't. Leave `thead th` alone: it stays sticky at the top.
   - Focus: buttons get `outline: 2px solid var(--focus); outline-offset: 2px` on `:focus-visible`.
   - Without JavaScript the picker buttons do nothing and all columns show. That's accepted: the tabs need JavaScript too.

3. In `pricing.astro`:
   - The `pricing.mjs` import becomes `import { formatMoney, SELF_HOSTED, TEAMS_ADDON } from "../marketing/data/pricing.mjs";` (`EVERY_PLAN` removed, `TEAMS_ADDON` added). Add `import FeatureTable from "../marketing/pricing/FeatureTable.astro";` and `import { CLOUD_COLUMNS, groupsFor, SELF_HOSTED_COLUMNS } from "../marketing/data/features.mjs";`.
   - In the first tab panel, keep the PlanCards section. Then put the Calculator section (`tone="raised" divider`, unchanged). Then replace the old teal "Every plan" section with:

     ```astro
     <Section tone="ink" divider labelledby="every-title">
       <SectionHeading
         eyebrow="Every plan"
         title="The whole product, on every plan."
         id="every-title"
         lede="Free included. Plans differ in volume, history, seats and how many rules you set up. Retention controls come with Pro, and single sign-on and project roles with the Teams add-on."
       />
       <FeatureTable id="cloud-features" caption="cetra Cloud plans compared" columns={CLOUD_COLUMNS} groups={groupsFor("cloud")} picker />
       <p class="muted table-note">{`Teams add-on: ${formatMoney(TEAMS_ADDON.priceMonthly)} a month on top of Pro. Prices in USD, excluding tax.`}</p>
     </Section>
     ```

   - In the second tab panel, after the self-hosted cards section, add:

     ```astro
     <Section tone="ink" divider labelledby="self-features-title">
       <SectionHeading
         eyebrow="Self-hosted"
         title="Everything, on your own server."
         id="self-features-title"
         lede="Every core feature with no limits, for free. The licence adds single sign-on and project roles."
       />
       <FeatureTable id="self-features" caption="Self-hosted cetra compared" columns={SELF_HOSTED_COLUMNS} groups={groupsFor("selfHosted")} />
     </Section>
     ```
   - In the style block, delete the `.every` rules (both the base one and the `min-width: 860px` one). Add `.table-note { margin-top: var(--space-5); font-size: var(--text-sm); }`.
   - Re-indent the two tab panels' content consistently (two spaces deeper than `<div data-tab-panel>`). The current file has uneven indentation there.

4. Build and run the checks:
   - `pnpm --dir website exec astro check`;
   - `pnpm --dir website exec astro build`;
   - `node website/scripts/check-marketing.mjs`, which checks every row link resolves to a built page, one `h1`, no skipped heading levels and the page weight (at most 350,000 bytes). Report the size of `website/dist/pricing/index.html` (`wc -c`).

5. Screenshots: run the pageshots tool (README § Conventions) on `/pricing/` and `/pricing/#self-hosted`. Read all four PNGs and check that:
   - there is no `HORIZONTAL OVERFLOW`;
   - at 1440, the Cloud table shows 4 value columns with check marks and the group headings in teal;
   - at 390, the Cloud table shows the picker and exactly one value column (Free);
   - the Self-hosted table shows 2 columns at both widths;
   - nothing overlaps at the top of the table. Full-page shots cannot show sticky behaviour; the main session checks that by scrolling in a browser.

   If a check fails, fix it within the **Edit** files. If it can't be fixed there, stop and ask.

**Tests:** X1's tests cover the data. This phase is checked by `astro check`, the build, `check-marketing.mjs` (links, headings, weight) and the screenshots in step 5.

**Done when**
- `just check-docs` passes.
- The step 5 screenshots are checked and look as described.
- `git status` shows only the **Edit** files changed.

**Commit:** `Show the full feature table on the pricing page`
