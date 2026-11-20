# M3: pricing, self-hosting, security

Status: planned 2026-11-20. Three phases, P1–P3, done in order, one commit each.

**Summary.** Three pages from design §5:
- `/pricing/`: the cloud plans and the self-hosted offer, rendered from one typed data file that a test checks against D177. It has a usage calculator.
- `/self-hosting/`: the one-binary story, install paths, machine sizing, upgrades and the licence key.
- `/security/`: where data lives and how it is protected, stating only what is true today.

**Prerequisites**
- M2 done on `claude/website-m2` (H1–H3).
- The branch `claude/website-m3`, cut from `claude/website-m2`.

**Read first**
- `docs/website/00-design.md` §5, §7.
- `docs/website/positioning.md` §5a.
- `docs/plans/website/README.md` (conventions).
- The M1 plan (components and checks).
- `docs/mvp-decisions.md` rows D172–D178 and D181.
- The docs pages `website/src/content/docs/docs/platform/plans-and-licence.mdx`, `self-hosting/resources.md`, `self-hosting/security.md`, `self-hosting/upgrade.md`, `self-hosting/backup.md`, `self-hosting/install.md`, `platform/deletion-and-retention.mdx`, `platform/sso.mdx`.

## What it is, and what it is not

When M3 is done, the three pages exist, pass `check-marketing.mjs`, and appear in the header and footer (they filter on `MARKETING_ROUTES`).

M3 does **not**:
- add a payment or checkout flow (the cloud has its own);
- promise certifications. cetra has none today: the pages say so;
- add volume discount tiers to the calculator. D177 says "lower with volume" without numbers, so the calculator shows list price and says that volume lowers it.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D172 | Open core; the self-hosted core has no volume, seat or retention limits; the licence key is verified offline |
| D173 | Units = spans + scores, per org per month |
| D174 | Free: hard cap; paid: overage, with an optional spend cap |
| D175 | Data older than the plan's window is hidden, then deleted 30 days later |
| D176 | Owners, admins and members are seats; viewers are free |
| D177 | The plans and prices |
| D178 | Free: 1 alert rule and 1 active online rule; paid: unlimited |
| D181 | Deletion wording: hidden within seconds, out of live data within 24 h, bytes removed at GC |
| D-Q-82 | Self-hosted licence: $5,400 per licence per year, USD, tax exclusive (llmobs-control `docs/decisions.md`) |
| Design §9.4 | Name the cloud region: the EU (`eu1`, Hetzner) |

## Versions

No new dependency.

## Phases

```
P1 plans data and its test ──► P2 /pricing/ ──► P3 /self-hosting/ and /security/
```

---

## P1: the plans data file and its check

**Goal:** one typed source for every price and limit, checked against D177.

**Edit:**
- new `website/src/marketing/data/pricing.mjs`
- new `website/src/marketing/data/pricing.test.mjs`

**Steps**

1. `website/src/marketing/data/pricing.mjs`, exactly (plain JS with JSDoc types, so `node --test` and Astro both import it):

```js
// Cloud plans and the self-hosted licence (D172–D178, D-Q-82). Every price on the site comes
// from here; pricing.test.mjs checks the numbers against D177 in docs/mvp-decisions.md and the
// licence features against website/src/content/docs/docs/platform/plans-and-licence.mdx.
// `projects` and `support` are null where D177 states nothing.

/**
 * @typedef {object} Plan
 * @property {string} id
 * @property {string} name
 * @property {number} priceMonthly USD
 * @property {number} includedUnits spans + scores a month
 * @property {boolean} overage
 * @property {string} dataAccess
 * @property {string} seats
 * @property {string | null} projects
 * @property {string} rules
 * @property {string | null} support
 * @property {string} cta the button label; the button goes to CLOUD.signup
 * @property {string} blurb
 */

export const UNIT = "span or score";

/** USD per 100,000 units over the included amount (D177: "$4 per 100k, lower with volume"). */
export const OVERAGE_PER_100K = 4;

/** @type {Plan[]} */
export const PLANS = [
  {
    id: "free",
    name: "Free",
    priceMonthly: 0,
    includedUnits: 100_000,
    overage: false,
    dataAccess: "30 days",
    seats: "1 seat",
    projects: "1 project",
    rules: "1 alert rule and 1 online evaluation rule",
    support: null,
    cta: "Start free",
    blurb: "For trying cetra on a side project.",
  },
  {
    id: "starter",
    name: "Starter",
    priceMonthly: 30,
    includedUnits: 200_000,
    overage: true,
    dataAccess: "120 days",
    seats: "Unlimited seats",
    projects: null,
    rules: "Unlimited rules",
    support: null,
    cta: "Start free",
    blurb: "For a team shipping its first LLM features.",
  },
  {
    id: "pro",
    name: "Pro",
    priceMonthly: 150,
    includedUnits: 200_000,
    overage: true,
    dataAccess: "5 years",
    seats: "Unlimited seats",
    projects: null,
    rules: "Unlimited rules",
    support: "Priority support",
    cta: "Start free",
    blurb: "For production apps: long history, retention controls, higher ingest limits.",
  },
];

export const TEAMS_ADDON = {
  name: "Teams add-on",
  priceMonthly: 300,
  requires: "Pro",
  features: [
    "Single sign-on through your OpenID Connect provider, such as Okta",
    "Enforce single sign-on for your domains",
    "Project-level roles",
    "A shared Slack or Teams channel with us",
  ],
};

export const ENTERPRISE = {
  name: "Enterprise",
  features: [
    "A dedicated cell",
    "Custom licence terms for self-hosting",
    "Audit logs and SCIM",
    "Service level agreements",
  ],
};

export const SELF_HOSTED = {
  name: "Self-hosted",
  priceYearly: 0,
  features: [
    "Every core feature, with no limits on volume, seats or retention",
    "One binary, Docker, docker-compose or Helm",
    "Your data never leaves your infrastructure",
  ],
  licence: {
    name: "Self-hosted licence",
    priceYearly: 5_400,
    features: [
      "Single sign-on per org, with verified domains",
      "Enforce single sign-on for your domains",
      "Project-level roles",
    ],
  },
};

/**
 * Monthly list price of a paid plan for a number of units (no volume discount, no tax).
 * @param {{ priceMonthly: number, includedUnits: number, overage: boolean }} plan
 * @param {number} units
 * @returns {number | null} null when the plan has no overage and the units exceed it
 */
export function monthlyPrice(plan, units) {
  const extra = Math.max(0, units - plan.includedUnits);
  if (extra > 0 && !plan.overage) return null;
  return plan.priceMonthly + (extra / 100_000) * OVERAGE_PER_100K;
}

/**
 * The cheapest plan for a number of units: Free when it fits, else Starter.
 * @param {number} units
 */
export function suggestedPlan(units) {
  return units <= PLANS[0].includedUnits ? PLANS[0] : PLANS[1];
}
```

   The licence features are the three `ee` features of `plans-and-licence.mdx` (`org_sso`, `sso_enforcement`, `project_roles`). Audit logs and SCIM are listed only under Enterprise, as D177 does, because the self-hosted key does not unlock them yet. If `plans-and-licence.mdx` lists other features when you implement this, stop with `QUESTION:`.

2. There is no `plans.ts`: pages and scripts import `pricing.mjs` directly (Astro type-checks it through its JSDoc, as it does `routes.mjs`).

3. `website/src/marketing/data/pricing.test.mjs` (node:test).
   - Read `docs/mvp-decisions.md` (path from `import.meta.dirname`, `../../../../docs/mvp-decisions.md`) and take the line starting with `| D177 |`.
   - Then test:
     - **"D177 is found"**: the line exists.
     - **"Free matches D177"**: the line contains `Free: $0, 100k units/month` and `30 days data access`; `PLANS[0]` has `priceMonthly 0`, `includedUnits 100000` and `dataAccess "30 days"`.
     - **"Starter matches D177"**: the line contains `Starter: $30/month, 200k units/month included then $4 per 100k` and `120 days data access`; `PLANS[1]` agrees, and `OVERAGE_PER_100K === 4`.
     - **"Pro matches D177"**: the line contains `Pro: $150/month` and `5 years data access`; `PLANS[2]` agrees, with the same `includedUnits` as Starter.
     - **"Teams matches D177"**: the line contains `Teams add-on (on Pro): $300/month`; `TEAMS_ADDON.priceMonthly === 300`.
     - **"monthly price"**:
       - `monthlyPrice(starter, 0) === 30`;
       - `monthlyPrice(starter, 200_000) === 30`;
       - `monthlyPrice(starter, 300_000) === 34`;
       - `monthlyPrice(starter, 1_200_000) === 70`;
       - `monthlyPrice(pro, 1_200_000) === 190`;
       - `monthlyPrice(free, 100_000) === 0`;
       - `monthlyPrice(free, 100_001) === null`.
     - **"suggested plan"**: 50,000 → Free; 100,000 → Free; 100,001 → Starter.
     - **"licence features match the docs"**: read `website/src/content/docs/docs/platform/plans-and-licence.mdx` (cwd-relative `src/content/docs/docs/platform/plans-and-licence.mdx`), take the rows of the table under `## What needs a licence key` (lines starting with `` | ` ``), and assert their feature keys are exactly `org_sso`, `sso_enforcement` and `project_roles` (in any order), and that `SELF_HOSTED.licence.features` has three items. A new `ee` feature in the docs then fails this test until the page lists it.

**Done when**
- `pnpm --dir website exec node --test src/marketing/` passes.
- `pnpm --dir website exec astro check` passes.
- `just check-docs` passes.

**Commit:** `Add the cetra plans data and its check against D177`

---

## P2: the /pricing/ page

**Goal:** `/pricing/`, rendered from `pricing.mjs`, with the calculator.

**Edit:**
- new `website/src/pages/pricing.astro`
- new `website/src/marketing/pricing/` components: `PlanCards.astro`, `Calculator.astro`, `PricingFaq.astro`
- `website/src/marketing/routes.mjs` (add `"/pricing/"`)
- `website/src/marketing/data/pricing.mjs` and `pricing.test.mjs` (add `formatUnits` and its test)
- `website/src/marketing/home/CloudOrSelfHost.astro` (links to the new pages)

**Page** (`MarketingLayout title="Pricing" route="/pricing/"`, description `cetra Cloud plans from free to Pro, and free self-hosting with no limits. Pay for spans and scores, with a spend cap.`). Copy is exact.

1. `Section tone="ink" pad="md"`:
   - `SectionHeading level={1} align="center"` with eyebrow `Pricing`, title `Free to start. Free to self-host.`, lede `cetra Cloud bills by usage, with a free plan and a spend cap. Self-hosted cetra is free with no limits; companies add a licence for single sign-on.`

2. `Section tone="ink" pad="md"`, **PlanCards**:
   - First, `<h2 class="visually-hidden">Plans</h2>` (the cards are `h3`s and must follow an `h2`).
   - Three cards for `PLANS` in a grid (`repeat(3, minmax(0,1fr))`, one column under 960 px), then two wide cards, `TEAMS_ADDON` and `ENTERPRISE`, side by side.
   - Each plan card (`Card tone="surface"`; Pro gets `border-color: var(--accent)` and a small `p.eyebrow` badge `Most teams`) shows:
     - the name as the card's `h3`;
     - the price (`$0`, `$30`, `$150`, in `font-family: var(--font-display); font-size: var(--text-3xl)`) followed by `/ month`;
     - the blurb;
     - a `Button` to `CLOUD.signup`, `external`, with the `cta` label;
     - a `ul` of lines built from the data:
       - `${formatUnits(includedUnits)} spans and scores a month`;
       - then `Then $4 per 100,000` when `overage`, else `Hard limit, no overage`;
       - `dataAccess` followed by ` of history`;
       - `seats`, then `projects` and `support` when not null, then `rules`.
   - `formatUnits` lives in `pricing.mjs` (add it there, with a test in `pricing.test.mjs`): `100000` → `"100,000"` (`toLocaleString("en-US")`).
   - Teams card: `Teams add-on`, `$300 / month on top of Pro`, its features, `Button href={`mailto:${CONTACT_EMAIL}`} variant="secondary"` `Talk to us`.
   - Enterprise card: `Enterprise`, `Custom`, its features, the same `Talk to us` button.
   - Under the grid, `p.muted` (`font-size: var(--text-sm); text-align: center`): `Prices in USD, excluding tax. A unit is one span or one score. Viewers are free on every plan. cetra Cloud runs in the EU.`

3. `Section tone="raised" divider`, **Calculator** (`labelledby="calc-title"`):
   - `SectionHeading` with eyebrow `Estimate`, title `What will it cost?` (`id="calc-title"`), lede `Enter your monthly spans and scores. The estimate is the list price; volume lowers it.`
   - Below it, a `div.calc` (not a `form`: nothing is submitted) holding:
     - `<label for="calc-spans">Spans a month</label><input id="calc-spans" type="number" min="0" step="10000" value="1000000" inputmode="numeric">`;
     - `<label for="calc-scores">Scores a month</label><input id="calc-scores" type="number" min="0" step="1000" value="50000" inputmode="numeric">`;
     - an output `<output id="calc-out" for="calc-spans calc-scores" aria-live="polite">`.
     Inputs use `background: var(--surface); color: var(--fg); border: 1px solid var(--line-strong); border-radius: var(--radius-sm); padding: var(--space-2) var(--space-3); font: inherit`.
   - Output: three rows (`Free`, `Starter`, `Pro`), each with the plan name and its estimate, rounded to whole dollars with `Math.round`: `$0`, `$64` or `$184` for the default 1,000,000 spans and 50,000 scores, and `Not available above 100,000 a month` for Free when the units exceed it. The suggested plan is marked with `p.eyebrow` `Suggested`.
   - In the script, read the inputs with `document.getElementById("calc-spans") as HTMLInputElement` (and the same for scores and `HTMLOutputElement`), so `astro check` passes.
   - Under the output, `p.muted`: `You can set a spend cap: ingestion stops at the cap, so the bill never passes it. Large volumes get lower rates; talk to us.`
   - The script imports `monthlyPrice`, `suggestedPlan` and `PLANS` from `../data/pricing.mjs` (Astro bundles it). It recomputes on `input`. Negative or empty inputs count as 0. It renders the initial values on load, and the server-rendered HTML holds the same initial estimate, so the page is complete without JavaScript: compute it in the frontmatter with the same functions.

4. `Section tone="teal" divider`, the self-hosted offer (`labelledby="self-title"`):
   - `SectionHeading` with eyebrow `Self-hosted`, title `Run it yourself, free.` (`id="self-title"`), lede `The same software on your own server: every core feature, no usage limits, and your data stays with you.`
   - Two `Card`s:
     - **`Self-hosted`**: `$0`, the `SELF_HOSTED.features`, `Button href="/docs/self-hosting/install/"` `Install guide`. (P3 points it at `/self-hosting/` once that page exists.)
     - **`Self-hosted licence`**: `$5,400 / year`, the `licence.features`, `p.muted` `Billed yearly, per licence. A 30-day trial is available from sales.`, `Button href={`mailto:${CONTACT_EMAIL}`} variant="secondary"` `Talk to us`.

5. `Section tone="ink" width="narrow"`, **PricingFaq**: `SectionHeading` with eyebrow `Questions`, title `Pricing questions`. Then `<details>` items in the M2 FAQ style (copy the CSS from `website/src/marketing/home/Faq.astro`):
   1. `What is a unit?` / `One span or one score sent to cetra. A trace with 20 spans is 20 units. Scores from your LLM judges count too; the spans of the judge calls do not.`
   2. `What happens at the limit?` / `On Free, cetra stops accepting new spans for the rest of the month and tells you why. On paid plans, ingestion continues and the extra units are billed, unless you set a spend cap.`
   3. `What does "history" mean?` / `How far back you can see your data. Older data is hidden, and an upgrade shows it again. It is deleted 30 days after it leaves the window.`
   4. `Who counts as a seat?` / `Owners, admins and members. Viewers are free and unlimited.`
   5. `Is self-hosted cetra limited?` / `No. Without a licence key it has every core feature and no limits on volume, seats or retention. The licence adds single sign-on and project roles.`

**Steps**
1. Write the components and the page as above. Add `"/pricing/"` to `MARKETING_ROUTES`.
1a. In `home/CloudOrSelfHost.astro` (M2), after the two cards, add a centred row with `Button href="/pricing/" variant="ghost"` `See pricing →`. P3 adds `Self-hosting →` and `Security →` next to it.
2. Build, run `node scripts/check-marketing.mjs` from `website/`, and fix the findings.
3. Screenshot the page at 1440 px and 390 px (as in M2's H3 step 4). Check the calculator by typing 300000 spans and 0 scores: Starter shows `$34` and Pro `$158`. Report what you saw.

**Tests:** `formatUnits` in `pricing.test.mjs` (100000 → "100,000", 0 → "0"). `check-marketing.mjs` covers the page.

**Done when**
- `just check-docs` passes and prints `check-marketing: 3 pages ok`.
- The calculator shows the expected numbers.

**Commit:** `Add the pricing page`

---

## P3: /self-hosting/ and /security/

**Goal:** the two pages buyers and operators read before they choose.

**Edit:**
- new `website/src/pages/self-hosting.astro`
- new `website/src/pages/security.astro`
- `website/src/marketing/routes.mjs` (add both routes)
- `website/src/pages/pricing.astro` (the Self-hosted card's button)
- `website/src/marketing/home/CloudOrSelfHost.astro` (two links)
- `docs/plans/website/README.md` (status)

### /self-hosting/

`MarketingLayout title="Self-hosting" route="/self-hosting/"`, description `Run cetra on one machine: one binary, Docker or Helm. What it needs, how to upgrade and back up, and what a licence adds.`

1. `Section tone="ink"`:
   - `SectionHeading level={1}` with eyebrow `Self-hosting`, title `One binary on one machine.`, lede `cetra runs as a single process with one data folder. No database server, no queue, no cache to run next to it.`
   - `CopyCommand id="sh-install" commands={INSTALL_COMMANDS} note="Install guide" noteHref="/docs/self-hosting/install/"`.
2. `Section tone="raised" divider`, `SectionHeading` with title `Ways to run it`. Four `Card`s with `href`:
   - `Binary`: `An install script for Linux and macOS on Apple silicon, or a tarball with checksums.` → `/docs/self-hosting/install/`
   - `Docker`: `A multi-arch image that runs as a non-root user on a read-only file system.` → `/docs/self-hosting/install/#docker`
   - `docker-compose`: `One server, optionally with a local S3-compatible store.` → `/docs/self-hosting/docker-compose/`
   - `Kubernetes`: `A Helm chart for clusters.` → `/docs/self-hosting/kubernetes/`
3. `Section tone="ink" divider`, `SectionHeading` with title `What it needs`, lede `From the resources guide. Budgets are configurable.` Then a `dl` grid of four pairs (`dt` in `.eyebrow`, `dd` in `font-size: var(--text-lg)`):
   - `Memory` / `8 GiB with the default settings; 2 to 4 GiB with the small-machine settings`
   - `Disk` / `A local SSD or NVMe for the data folder`
   - `Object storage` / `Optional: keep Parquet files and media in S3`
   - `Ports` / `8080 for the UI and API, 4317 and 4318 for OTLP`

   Then `Button href="/docs/self-hosting/resources/" variant="ghost"` `Sizing guide →`.
4. `Section tone="teal" divider`, `SectionHeading` with title `Day two`. Three `Card`s:
   - `Upgrades`: `Replace the binary or the image. Database migrations run on start, and Parquet files are never rewritten.` → `/docs/self-hosting/upgrade/`
   - `Backups`: `Back up a running server with one command, keep the secret key with it, and rehearse the restore.` → `/docs/self-hosting/backup/`
   - `Monitoring`: `Health and readiness endpoints, and Prometheus metrics on a private port.` → `/docs/self-hosting/monitoring/`
5. `Section tone="ink" divider`, `SectionHeading` with eyebrow `For companies`, title `Add a licence for single sign-on.`, lede `The core is MIT and free. A licence key, checked offline with no call home, unlocks the enterprise features in the same binary.` Then the `SELF_HOSTED.licence.features` as a `ul` and a row of buttons: `Talk to us` (mailto `CONTACT_EMAIL`, primary) and `See pricing` (`/pricing/`, secondary).

Before writing, check each docs link exists in `website/src/content/docs/docs/self-hosting/` and that `install.md` has a `## Docker` heading. Check that the claims in the cards are in those pages:
- the read-only root file system: `security.md`;
- "Parquet files are never rewritten": `upgrade.md`;
- the Prometheus endpoint: `monitoring.md`;
- the 8 GiB default and the small-machine settings: `resources.md`;
- the online backup command (`cetra admin backup`): `backup.md`;
- macOS on Apple silicon: `install.md`.

If one is not there, stop with `QUESTION:`.

### /security/

`MarketingLayout title="Security" route="/security/"`, description `Where cetra keeps your data, how it is protected, and how deletion and retention work, for self-hosted cetra and cetra Cloud.`

1. `Section tone="ink"`: `SectionHeading level={1}` with eyebrow `Security`, title `Your data, where you decide.`, lede `This page states what cetra does today. It makes no claim about certifications cetra does not have.`
2. `Section tone="raised" divider`, `SectionHeading` with title `Where your data lives`. Two `Card`s:
   - `Self-hosted`: `Traces go from your app to your server. cetra sends no usage data anywhere, and a licence key is verified offline.`
   - `cetra Cloud`: `Your org lives on a cell in the EU (eu1), on Hetzner servers and object storage in the EU.`
3. `Section tone="ink" divider`, `SectionHeading` with title `How it is protected`. A `ul` (`list-style: none; padding: 0; display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: var(--space-3) var(--space-6)`, one column under 720 px; each `li` has a 6 px `var(--accent)` dot `::before`, `display: flex; gap: var(--space-3); align-items: baseline`), exact items:
   - `Passwords hashed with Argon2id`
   - `Sign-in with GitHub, Google or your OpenID Connect provider`
   - `API keys stored as SHA-256 hashes, with optional expiry`
   - `Write-only public keys for browser feedback`
   - `Roles: owner, admin, member, viewer`
   - `Stored LLM keys and webhook secrets encrypted with a key you hold`
   - `LLM judges and webhooks cannot reach private network addresses by default`
   - `Logs carry IDs and sizes, never prompts, outputs or secrets`
4. `Section tone="teal" divider`, `SectionHeading` with title `Deletion and retention`. Three `p`s:
   - `Delete by user, by trace, by filter, by project or by org. Deleted data disappears from every view within seconds and, by default, leaves live storage within 24 hours. The bytes are then removed from old files when cetra cleans them up.`
   - `Set retention per project. Days past it are removed.`
   - then a link `How deletion works` → `/docs/platform/deletion-and-retention/`.
5. `Section tone="ink" divider`, `SectionHeading` with title `Compliance`. One `p`: `cetra is not SOC 2 or ISO 27001 certified yet. If you need a security review, write to us and we will answer your questionnaire.` and the mailto `CONTACT_EMAIL` link.

Before writing, check each claim against the repo and stop with `QUESTION:` if one is not supported:
- Argon2id: spec 08 or `crates/server/src/auth/`;
- GitHub and Google OAuth and OIDC: `platform/sso.mdx` or `self-hosting/authentication.mdx`;
- SHA-256 key hashes: `docs/spec/08-auth.md`; key expiry: `platform/api-keys.mdx`;
- `pk-` write-only keys: `platform/api-keys.mdx`;
- roles: `platform/organizations.mdx`;
- the encrypted secrets and `secret.key`: `self-hosting/security.md`;
- private-network egress: `self-hosting/security.md`;
- logs: `self-hosting/security.md`;
- "sends no usage data": `crates/server/src/config.rs` refuses `telemetry.*` keys, and D172 says the licence check has no phone-home;
- the deletion timing (D181): `platform/deletion-and-retention.mdx`.

**Steps**
1. Write both pages. Add `"/self-hosting/"` and `"/security/"` to `MARKETING_ROUTES`. In `pricing.astro`, change the Self-hosted card's button to `Button href="/self-hosting/"` `How to self-host`. In `home/CloudOrSelfHost.astro`, add `Button href="/self-hosting/" variant="ghost"` `Self-hosting →` and `Button href="/security/" variant="ghost"` `Security →` to the row P2 added. Add that file to this phase's Edit list.
2. Build; run the checks; screenshot both pages at 1440 px and 390 px as in M2; fix what you see.
3. `docs/plans/website/README.md`: set M3's status to `done (P1–P3)`.

**Tests:** `check-marketing.mjs` covers both pages.

**Done when** `just check-docs` passes and prints `check-marketing: 5 pages ok`.

**Commit:** `Add the self-hosting and security pages`
