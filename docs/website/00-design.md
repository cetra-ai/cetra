# cetra.sh marketing site: design

Status: **open questions answered (§9), 2026-11-19.** The user reviews the whole doc before the M1 plan is written.

This doc is the high-level design of the marketing site at `https://cetra.sh/`. Each milestone (§10) gets its own plan in `docs/plans/website/` just before it starts.

Background, all in `docs/website/`:
- `research.md`: 23 reference sites compared, patterns and anti-patterns;
- `sites/`: one note per site;
- `positioning.md`: positioning, messaging and headline candidates;
- `evidence/`: 190 sourced user complaints and comments.

The comparison with screenshots is in the Artifact https://claude.ai/artifact/G8WLccev9n5VEv2ui4X1xH.

## 1. Goals and non-goals

**Goals**
1. **Sell.** Turn visitors into cloud sign-ups (demo first, then a plan) and self-hosted installs, and route companies to pricing, security and "talk to us". Revenue comes from cloud plans and self-hosted licence keys (D172, D177).
2. **Say the difference in five seconds.** Run it yourself in a minute, nothing to babysit, your data stays with you, or use our cloud (`positioning.md` §7).
3. **Look as good as the best in the category.** Braintrust is the bar among competitors; Linear is the bar for showing the product.
4. **Stay true.** Every number on the site is measured, dated and linked to its method. Nothing claims a feature the product doesn't have.
5. **Be cheap to keep right.** Static, in the main repo, deployed with the docs, checked in CI.

**Non-goals**
- A blog, newsletter, changelog page or customer stories. The docs already have a changelog (track W). Stories come after the first customers.
- Sign-up, login or any form on cetra.sh. Those live on the cloud. The site only links to them.
- Localisation: English only.
- A no-signup public demo instance. The demo is the cloud demo org (D187). See §9 for what the homepage shows instead.
- Building the benchmark itself. The site shows its results once they exist (§8, §10 M6).

## 2. Decisions already made (inputs to this design)

| Topic | Decision | Source |
|---|---|---|
| Name and brand | cetra, cetra.sh; the Caetra c logo; teal `#0f766e`, light teal `#2dd4bf` on dark, ink `#141414`, paper `#f5f5f4`; Space Grotesk for the name | D46, D189, `assets/brand/README.md` |
| Where it lives | **In the main repo**, in the existing Astro project `website/`. The marketing pages are at `/`, the docs at `/docs/` | User 2026-11-19; D-W-1, D-W-2 (PR #161) |
| Hosting | **Cloudflare Pages**, project `cetra-site`, deployed by `docs-deploy.yml` (direct upload with wrangler) | User 2026-11-19; D-W-3 |
| Analytics | Cloudflare Web Analytics only: no cookies, no consent banner | D-W-10 |
| Theme | **Dark** | User 2026-11-19 |
| References | **Bun** (selling structure), **Braintrust** (visual style and rhythm), **Linear** (how the product is shown) | User 2026-11-19 |
| Main hero action | **"Try the demo"** opens the cloud demo org (`/demo`, sign-in required, D187), labelled as needing a free account. Second: "Start free" (cloud sign-up). Below: the self-host install command | User 2026-11-19 (§9) |
| Headline direction | "Run it yourself, in a minute" (candidate A in `positioning.md` §7). Final wording reviewed in M2 | User 2026-11-19 |
| Comparison pages | **Yes, one per competitor, honest and dated** | User 2026-11-19 |
| Launch | **Everything at once**, except `/benchmarks/` (M6, after the benchmark exists). The holding page stays until every launch page is ready | User 2026-11-19 (§9) |
| Licence and plans | Open core, MIT core plus `ee/` under ELv2. Plans Free / Starter $30 / Pro $150 / Teams add-on $300 / Enterprise. Units = spans + scores | D172–D178 |
| Audiences | Developers are the way in; companies pay | User 2026-11-13, `positioning.md` §5 |

## 3. Architecture

```
website/                          (one Astro project, one Cloudflare Pages project)
├── src/pages/                    marketing pages: plain Astro pages, no Starlight
│   ├── index.astro               /             (replaces the holding page)
│   ├── pricing.astro             /pricing/
│   ├── self-hosting.astro        /self-hosting/
│   ├── security.astro            /security/
│   ├── integrations.astro        /integrations/
│   ├── compare/index.astro       /compare/
│   ├── compare/[slug].astro      /compare/<competitor>/
│   └── benchmarks.astro          /benchmarks/   (M6, after the benchmark exists)
├── src/marketing/                marketing-only components, layout, styles, content data
└── src/content/docs/docs/        the Starlight docs, unchanged (/docs/)
```

**Parts and contracts**
- **Marketing layout:** its own header, footer and styles, separate from Starlight. The header links to the docs, the docs header links back to cetra.sh. Starlight's styles never load on marketing pages, and marketing styles never leak into the docs.
- **Design tokens:** one CSS file of custom properties (colours, type scale, spacing, radii) used by every marketing component. The docs keep their own `custom.css`, aligned on the same brand colours.
- **Content as data:** typed data files (Astro content collections or TypeScript modules with types) hold everything that repeats or must stay true:
  - pricing plans;
  - comparison facts per competitor, each with a source URL and a "checked on" date;
  - the integrations list;
  - benchmark results.

  Pages render from them, and CI checks them (§7).
- **Single sources, no copies:**
  - Plan prices and limits come from one data file, written to match D177. A check fails if the pricing page and the docs disagree.
  - The integrations list is generated from the same source as the docs integration matrix (track W, D-W-6), not typed twice.
  - The GitHub star count is fetched at build time, with a fallback to "GitHub" when the fetch fails. It is never hard-coded.
- **Product visuals:**
  - **Coded UI scenes** (HTML/CSS components showing cetra's real screens with seeded, realistic data, the Linear/Braintrust pattern) for the hero and the product chapters. They are crisp, themeable and light, and they stay true because they copy the real UI's layout and use data shaped like `fixtures/api`.
  - **Real screenshots** from `just docs-screenshots` (Playwright on the MSW mocks, already in track W) where a full screen is better.
- **Links out:**
  - the cloud (demo `/demo`, sign-up, log in), using URLs from one config constant;
  - GitHub;
  - the docs.
- **No client framework.** Astro pages with small vanilla scripts for the few interactive bits (copy buttons, tabs, the stack diagram toggle). This keeps the pages fast; site speed is part of the pitch.

**Alternatives rejected**
- **A separate website repo.** The user chose the main repo (2026-11-19). That keeps one deploy, one CI and one source for shared facts.
- **Marketing pages inside Starlight (splash template).** It is too constrained for the Braintrust/Linear level of design and would mix marketing styles into the docs.
- **A React island framework for the scenes.** Not needed for static scenes. It could be added later for one interactive piece if a plan shows the need.
- **Embedding the real app (an iframe of `web/` on mock data).** It is heavy and fragile, and needs sign-in on the cloud anyway. Coded scenes plus the demo link do the job.

## 4. Visual direction

**Dark**, combining the three references:
- **Base:** ink `#141414`, with a slightly lighter raised surface for panels, and paper `#f5f5f4` for text.
- **Accent:** light teal `#2dd4bf` for actions and highlights, teal `#0f766e` for fills. One accent only, in the Bun spirit.
- **Rhythm (Braintrust):** full-width **colour bands** between chapters. In a dark design that means one deep-teal band, and one paper (light) band used once, for the comparison/pricing block, as a deliberate contrast. Section seams carry a recurring motif drawn from the logo: the caetra's rim arc and boss dot, used as a thin divider or as dots. It is used sparingly, never as a pattern wallpaper.
- **Type:**
  - **Space Grotesk** for display headings (the brand's face), large, tight tracking;
  - **Inter** for body text (the app's face, so the scenes and the page match);
  - **JetBrains Mono** for code and for eyebrows/labels (the app's mono).

  All three load from the site, with fallbacks.
- **Product (Linear):** coded UI scenes that bleed off the edge of their band, with soft fades at the edges. Every product chapter repeats one layout: title, one line, UI scene, small feature list.
- **Motion:** little of it. One orchestrated hero moment (the trace tree filling in), tab switches, copy feedback. Everything readable at rest, and `prefers-reduced-motion` respected. No scroll-triggered content that can render blank (an anti-pattern in `research.md` §6).
- **Mobile:** every section stacks to one column; scenes become scrollable crops.

## 5. Site map

| Page | URL | Job | Main sources |
|---|---|---|---|
| Home | `/` | Sell: what it is, why it's different, try it | §6 |
| Pricing | `/pricing/` | Cloud plans (D177) and the free self-host, with a usage calculator in units and the spend cap (D174) | Supabase, Resend, OpenObserve |
| Self-hosting | `/self-hosting/` | "One binary": install paths (binary, Docker, Compose, Helm), what one machine handles, upgrades, backups, the licence key for companies. Links to the docs for the how-to | Langfuse's self-host page, inverted: one box |
| Security | `/security/` | For buyers: data flow, where data lives (self-host / cloud region), no telemetry, GDPR deletion and retention (D181 wording), auth (OIDC, SSO), audit log, compliance status. Only true, current statements | Tailscale, Langfuse |
| Integrations | `/integrations/` | Every framework and SDK we ingest, each backed by a captured fixture, linking to its docs page | docs integration matrix |
| Compare | `/compare/` + `/compare/<x>/` | One page per competitor: Langfuse, Arize Phoenix, LangSmith, Braintrust, Helicone, Opik, LangWatch, Laminar, MLflow. TL;DR, a table by area, "when to choose them", migration notes, sources, "checked on" date | Langfuse vs LangSmith, PostHog, Supabase |
| Benchmarks | `/benchmarks/` | Named competitors on the same machine, method, tradeoffs, reproduction script (M6) | PlanetScale, Relax, Bun |
| Docs | `/docs/` | Existing (track W) | — |
| Demo | (cloud) `/demo` | The demo org (D187) | — |

The header holds Product (anchors on the homepage), Pricing, Docs, Compare, GitHub with its star count, Log in, and the "Try the demo" button. The footer holds every page, the licence, the status page if one exists, and contact.

## 6. Homepage, section by section

Copy here is **direction**, not final wording; M2 drafts it and reviews it with the user. No text, branding or illustration is copied from another site.

1. **Header.** Logo, nav, "Try the demo".
2. **Hero** (Bun structure, Linear product):
   - eyebrow: "Open-source LLM observability";
   - headline in the direction of *"LLM observability you run yourself. In a minute."*;
   - sub: one binary, nothing to babysit, your data stays on your server, or use our cloud;
   - buttons: **Try the demo** (main) and **Start free**;
   - under them, a copyable **install command** with OS tabs and a "what does this run?" link;
   - a coded **trace scene** (trace list → one agent trace's span tree, LLM messages, cost, a score) that bleeds off the fold.
3. **Proof strip.** Until we have logos: open-source facts that are true at launch, such as the licence (MIT core), GitHub stars, OpenTelemetry-native, the number of integrations backed by fixtures. Engine numbers join only once measured (M6).
4. **"Nothing else to run."** The stack contrast as a diagram: a typical self-hosted stack (Web, Worker, Postgres, Redis, ClickHouse, S3) against one cetra box. The "replaces X" framing comes from Bun. Each competitor is named only on its compare page, not here.
5. **Product chapters** (Linear's repeated layout, Braintrust's bands):
   1. Trace every LLM call (OTel from any framework; sessions, users, cost);
   2. Find problems fast (filters, full-text search, the SQL console);
   3. Evaluate (online judges, checks, datasets, experiments, presented as "when you're ready");
   4. Get alerted (error rate, cost, latency, score).

   Each has a coded scene.
6. **Your agents can read it too.** MCP and SQL: a coding agent querying production traces, shown as a terminal next to the UI. This uses the user's earlier idea, as a section, not the hero.
7. **Integrations.** A logo grid of frameworks and SDKs, each linking to its docs page; "and anything that speaks OpenTelemetry".
8. **Your data, your rules.** Self-host by default, no telemetry (to confirm in the code before stating it), retention and GDPR deletion, open Parquet files. Links to Security.
9. **Cloud or self-hosted.** Two cards, the audience routing. **Cloud:** start free, plans from $30, the demo. **Self-hosted:** free with no limits, licence key for SSO and audit, "talk to us". Links to Pricing and Self-hosting.
10. **How cetra compares.** A short honest table (setup, services to run, self-host limits, SQL access, OTel) linking to `/compare/`. This is the light paper band.
11. **FAQ.** The objections from `positioning.md` §7, as plain questions: "Will one machine handle my traffic?", "Is this a toy?", "What if the project disappears?", "Cloud or self-host?".
12. **Closing call to action.** "Try the demo" and the install command, repeated.
13. **Footer.**

## 7. Keeping it true (checks)

Run in CI with the docs checks (`just check-docs` or a sibling recipe):
- **Prices and limits** on the site match one data file, and that file matches D177. A test fails on any difference with the docs' pricing mentions.
- **Comparison facts** each need a source URL and a "checked on" date. The build fails when a date is older than a set age (e.g. 6 months), so stale claims get re-checked.
- **Integrations** come from the docs' matrix, never typed twice.
- **No unmeasured numbers.** Benchmark figures come only from the benchmark results file, with its machine, version and date.
- **Links:** internal links validated at build (already done for the docs); external links in the nightly lychee run (track W).
- **Accessibility and weight:** a Lighthouse-style budget (performance, accessibility ≥ 95; page weight cap) on the built pages.

## 8. Proof assets needed

| Asset | Needed by | Exists? | Owner |
|---|---|---|---|
| Coded UI scenes (trace list, span tree, evals, SQL, alerts) | M2 | No | This track |
| Real screenshots from the MSW mocks | M2–M4 | Yes (track W tooling) | Track W tooling |
| Install command that works on a clean machine | M2 | Packaging done (track N); exact command to confirm | N |
| Cloud URLs: demo, sign-up, log in | M2 | Cloud exists; URLs to confirm | Cloud track |
| Demo org populated | Launch | Planned (`docs/plans/demo-org/`) | Demo-org track |
| GitHub star count | M2 | Build-time fetch | This track |
| Integration list with fixtures | M4 | Yes (docs matrix) | Track W |
| Facts per competitor, sourced and dated | M5 | Partly (`docs/research/competitors.md`, `evidence/`) | This track |
| Benchmark results with method | M6 | **No**: D23 targets, the benchmark is not built | Storage / benchmark owner |
| Customer logos and quotes | Later | No: from design partners (`positioning.md` §5a) | User |

## 9. Answers to the open questions (user, 2026-11-19)

1. **The demo needs sign-in** (D187). Keep **"Try the demo"** as the main button, labelled honestly as needing a free account. The homepage's coded scenes show the product with no sign-up.
2. **Benchmarks:** launch **without** `/benchmarks/` and without engine numbers. M6 adds them when the benchmark is measured.
3. **Compare pages: all nine at launch**: Langfuse, Arize Phoenix, LangSmith, Braintrust, Helicone, Opik, LangWatch, Laminar, MLflow.
4. **Cloud region: named** (today `eu1`) on Security, Pricing and the "Cloud or self-hosted" cards, with more added as regions open. The exact cloud URLs (demo, sign-up, log in) come from the cloud track before M2. This is a lookup, not a decision.
5. **Fact checking:** a Sonnet agent verifies every comparison fact against its source before a page is published, with no separate user approval per page. The **"checked on" date is shown** on each page.

## 10. Milestones

Built in order, each milestone merged to `main` when done (no feature flag). Until M7, the new pages live at their URLs but the holding page stays at `/`, and the new pages carry `noindex` and are not linked from it.

| # | Delivers (what works when it's done) | Depends on | Leaves out |
|---|---|---|---|
| **M1 Foundation** | Marketing layout (header, footer), design tokens, fonts, the bands and the rim motif, base components (buttons, copy-command box, tabs, section and chapter layouts, card), docs header link back, CI checks wired (data files, links, accessibility and weight budget). A style page at `/_styleguide/` (noindex) to review the look | PR #161 merged (the `website/` project) | Real content, scenes |
| **M2 Homepage** | `/` content as in §6 (at a preview URL, the holding page still live): final copy reviewed with the user, coded UI scenes, install command, cloud links, stack diagram, FAQ | M1; cloud URLs; install command | Compare details, benchmark numbers |
| **M3 Pricing, self-hosting, security** | `/pricing/` from the plans data file with the units calculator and spend cap; `/self-hosting/`; `/security/` | M1; D172–D178 text confirmed | Enterprise contract terms |
| **M4 Integrations** | `/integrations/` generated from the docs matrix, logos, links to the docs pages | M1; the docs matrix | — |
| **M5 Compare** | `/compare/` and nine competitor pages (§9.3), facts in data files with sources and visible "checked on" dates, each fact verified by a Sonnet agent against its source, the staleness check | M1 | Benchmark numbers |
| **M6 Benchmarks** *(after launch, §9.2)* | `/benchmarks/` from a results file: machines, versions, dates, named competitors, tradeoffs, reproduction script; engine numbers added to the homepage proof strip | The benchmark itself (D23) | — |
| **M7 Launch** | The holding page replaced by the homepage; `noindex` removed; sitemap, `robots.txt`, Open Graph images per page, canonical URLs, 404 page; Web Analytics on; final review pass on every page by the user | M2–M5 | Benchmarks (M6) |

## 11. Risks

- **The scenes drift from the real UI** as the app changes. Mitigation: build them from the same data shapes as `fixtures/api`, and refresh them in the same PR as a visible UI redesign.
- **Comparison claims go stale or are wrong**, which costs trust (the Helicone anti-pattern). Mitigation: sources, dates, the staleness check, "when to choose them" honesty.
- **"Everything at once" delays the launch.** Mitigation: M6 is outside the launch (§9.2). Every other milestone ships to `main` behind `noindex`, so nothing waits on a long branch.
- **Font weight and page speed.** Three families is the most; subset them and preload only what's above the fold.
