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
- A public demo instance with real data. Since §9.6 "Try the demo" opens the web demo on sample data (D-C-22), not the cloud demo org (D187).
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
| Main hero action | **"Try the demo"** opens the web demo: the real UI on sample data, no sign-in (D-C-22), at `https://demo.cetra.sh`. Second: "Start free" (cloud sign-up). Below: the self-host install command. The cloud demo org (D187) is not linked from the site | User 2026-11-22 (§9.6), replacing 2026-11-19 |
| Headline direction | "Run it yourself, in a minute" (candidate A in `positioning.md` §7). Final wording reviewed in M2 | User 2026-11-19 |
| Comparison pages | **Yes, one per competitor, honest and dated** | User 2026-11-19 |
| Launch | **Everything at once**, except `/benchmarks/` (M6, after the benchmark exists). The holding page stays until every launch page is ready | User 2026-11-19 (§9) |
| Licence and plans | Open core, MIT core plus `ee/` under ELv2. Plans Free / Starter $30 / Pro $150 / Teams add-on $300 / Enterprise. Units = spans + scores | D172–D178 |
| Audiences | Developers are the way in; companies pay | User 2026-11-13, `positioning.md` §5 |
| Product scope sold | **The whole MVP**, including what main added on 2026-11-20/21: prompts and playground (D191), annotation queues (D192), the in-app assistant (D193), issue clustering (D194), coding-agent tracing (D195), `cetra-cli` and the docs MCP (track AO). The site sells hard, and never past what the docs state | User 2026-11-22 (§9.7) |

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
  - **Real screenshots of the UI in its dark theme** for the hero and the product chapters, made by `just docs-screenshots` (Playwright on the MSW mocks, track W) as `marketing-*` shots with `"theme": "dark"`, shown in a window frame. On phones they are scrollable crops.
  - **One coded scene:** a terminal transcript of a coding agent using the MCP server.
  - *Changed in the M2 plan (2026-11-20):* coded HTML scenes of every screen were dropped. Real screens stay true with no hand-built UI, and one command refreshes them when the UI changes.
- **Links out:**
  - the cloud (sign-up, log in) and the web demo (§9.6), using URLs from one config constant;
  - GitHub;
  - the docs.
- **No client framework.** Astro pages with small vanilla scripts for the few interactive bits (copy buttons, tabs, the stack diagram toggle). This keeps the pages fast; site speed is part of the pitch.

**Alternatives rejected**
- **A separate website repo.** The user chose the main repo (2026-11-19). That keeps one deploy, one CI and one source for shared facts.
- **Marketing pages inside Starlight (splash template).** It is too constrained for the Braintrust/Linear level of design and would mix marketing styles into the docs.
- **A React island framework.** Not needed for static pages. It could be added later for one interactive piece if a plan shows the need.
- **Embedding the real app (an iframe of `web/` on mock data).** It is heavy and fragile, and needs sign-in on the cloud anyway. Screenshots plus the demo link do the job.

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
- **Product (Linear):** screenshots in window frames. Every product chapter repeats one layout: title, one line and a small feature list beside the UI scene, alternating sides. Each scene is a crop of the one panel its chapter talks about, never the whole app, and is shown no larger than its CSS size (§9.10.2, D-W-23).
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
| Demo | `https://demo.cetra.sh` | The web demo on sample data (D-C-22, §9.6) | — |

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
   - a dark **screenshot of the trace view** (an agent trace's span tree, LLM messages, cost) under the text, full width: a crop of the trace panel with the timeline on, shown at its own size (D-W-23). Tabs above it switch between four example traces, Support agent, Coding agent, Voice agent and Document extraction, each with a one-line caption under the picture (D-W-24, M12).
3. **Proof strip.** Until we have logos: open-source facts that are true at launch, such as the licence (MIT core), GitHub stars, OpenTelemetry-native, the number of integrations backed by fixtures. Engine numbers join only once measured (M6).
4. **"Nothing else to run."** The stack contrast as a diagram: a typical self-hosted stack (Web, Worker, Postgres, Redis, ClickHouse, S3) against one cetra box. The "replaces X" framing comes from Bun. Each competitor is named only on its compare page, not here. Since 2026-11-23 (user) it is a 1:1 replacement: each service, labelled "What most alternatives make you run", points to the part of the one cetra box that replaces it (web UI and API, evaluations and alerts, embedded SQLite, built-in job queue, storage engine, local disk with S3 optional), and the caption links to `/compare/`.
5. **Product chapters** (Linear's repeated layout, Braintrust's bands):
   1. Trace every LLM call (OTel from any framework; sessions, users, cost);
   2. Find problems fast (filters, full-text search, the SQL console);
   3. Evaluate (online judges, checks, datasets, experiments, presented as "when you're ready");
   4. Get alerted (error rate, cost, latency, score).

   Each shows a dark screenshot of that screen. **Since M8 (§9.7) there are six:** Trace; Explore, with the in-app assistant as its screenshot; Issues (problems find you); Evaluate, with annotation queues as its screenshot; Prompts and the playground; Alert.
5b. **Coding agents** *(M8)*. "See what your coding agents cost": Claude Code, Codex, Gemini CLI and Copilot traced with one `cetra agents setup` command, cost per person, repository and branch. A terminal visual, like §6.6.
6. **Your agents can read it too.** MCP and SQL: a coding agent querying production traces, shown as a terminal next to the UI. This uses the user's earlier idea, as a section, not the hero. Since M8 it also offers `cetra-cli init` and the docs MCP.
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
- **Accessibility and weight:** `check-marketing.mjs` checks every built marketing page: one `h1`, no skipped heading level, `alt` on images, a name on every link and button, metadata, `noindex`, internal links, and a 350 KB cap without fonts. `contrast.test.mjs` checks WCAG AA contrast for every text colour pair of the tokens. CI never downloads a browser (`website/README.md`), so Lighthouse and axe are not used (M1 plan).

## 8. Proof assets needed

| Asset | Needed by | Exists? | Owner |
|---|---|---|---|
| Dark UI screenshots (trace view, trace list, SQL, evaluators, alerts) and the agent transcript | M2 | No | This track |
| Real screenshots from the MSW mocks | M2–M4 | Yes (track W tooling) | Track W tooling |
| Install command that works on a clean machine | M2 | Packaging done (track N); exact command to confirm | N |
| Cloud URLs: sign-up, log in; the web demo domain | M2; M8 | Cloud exists; URLs to confirm | Cloud track |
| `demo.cetra.sh` attached to the `cetra-web-demo` Worker | Launch | Not done | User |
| GitHub star count | M2 | Build-time fetch | This track |
| Integration list with fixtures | M4 | Yes (docs matrix) | Track W |
| Facts per competitor, sourced and dated | M5 | Partly (`docs/research/competitors.md`, `evidence/`) | This track |
| Benchmark results with method | M6 | **No**: D23 targets, the benchmark is not built | Storage / benchmark owner |
| Customer logos and quotes | Later | No: from design partners (`positioning.md` §5a) | User |

## 9. Answers to the open questions (user, 2026-11-19)

1. *(Replaced by §9.6 on 2026-11-22.)* **The demo needs sign-in** (D187). Keep **"Try the demo"** as the main button, labelled honestly as needing a free account. The homepage's screenshots show the product with no sign-up.
2. **Benchmarks:** launch **without** `/benchmarks/` and without engine numbers. M6 adds them when the benchmark is measured.
3. **Compare pages: all nine at launch**: Langfuse, Arize Phoenix, LangSmith, Braintrust, Helicone, Opik, LangWatch, Laminar, MLflow.
4. **Cloud region: named** (`eu1`; `us1` since 2026-11-22, on OVH in Virginia, llmobs-control D-Q-121) on Security, Pricing and the "Cloud or self-hosted" cards, with more added as regions open. The exact cloud URLs (demo, sign-up, log in) come from the cloud track before M2. This is a lookup, not a decision.
5. **Fact checking:** a Sonnet agent verifies every comparison fact against its source before a page is published, with no separate user approval per page. The **"checked on" date is shown** on each page.

Answers of 2026-11-22, after main gained the features of D191–D195:

6. **"Try the demo" opens the web demo** (D-C-22): the real UI on sample data, with no sign-in. It replaces §9.1. The link is `https://demo.cetra.sh`; that custom domain must be attached to the `cetra-web-demo` Worker before launch (the domain name is the planner's choice, to confirm at launch).
7. **Sell the full product story:** the hero names the breadth, the "one binary" box lists all of it, six product chapters, a coding-agents section, and the compare table, pricing and FAQ updated (§6).
8. **One pull request** for the whole site, from `claude/website`, replacing the stack #174–#180.

Answers of 2026-11-23, on a full feature table for `/pricing/` (M9), like the ones on Langfuse, Laminar and Arize:

9. **A feature table on each tab of `/pricing/`.**
   1. The Cloud table has four columns: Free, Starter, Pro, Enterprise. The Teams add-on's features read "Teams add-on" in the Pro column, as on Langfuse.
   2. The Self-hosted table has two columns, free and licence. It adds the self-hosted-only rows: install, S3, backup, sign-in providers.
   3. The Enterprise-only rows (dedicated cell, audit logs and SCIM, SLAs) are shown, backed by D177. Their docs pages come later.
   4. About 55 rows, with the limits first. The table replaces the "Every plan" bullet list.
   5. *(Planner's reading, not asked; D-W-21.)* Enterprise has everything that Pro with the Teams add-on has: unlimited seats, projects and rules, retention controls, single sign-on, project roles and the shared channel. Its volume, overage, history and ingest values read "Custom", following D177. Starter and Pro show unlimited projects, as the control plane's catalogue sets them.

Answers of 2026-11-24:

10. **The licence price and the homepage screenshots.**
   1. **The self-hosted licence shows no price** (D-W-22). It reads "Custom" with "Talk to us", like the Cloud Enterprise plan and the competitors' enterprise self-hosted tiers. It stays one licence with every enterprise feature, SAML included; the user chose this over a $5,400 Teams licence beside a custom-priced Enterprise one.
   2. **The homepage screenshots show one panel each** (D-W-23, M10). The whole-app captures at 1440×900 read at about 9 px on the page, and no competitor shows the whole app: Laminar shows one cropped panel, Langfuse drawings, Helicone one simplified dashboard. Each `marketing-*` shot is a crop of the panel its section talks about, in a smaller window. The six chapters put their text beside the picture, alternating sides. The hero keeps its picture under the text, full width, with the trace timeline on (the user, 2026-11-24, after seeing the preview).
   3. **The hero shows four example traces behind tabs** (D-W-24, M12): Support agent, Coding agent (a failed test run, then a fix), Research agent (searches in parallel, then a cited answer) and Document extraction (a cheap model, a failed schema check, a stronger model). The user chose labelled tabs over a carousel or a scroll strip. M13 replaces the research agent with a voice agent (§12).

## 10. Milestones

Built in order, each milestone merged to `main` when done (no feature flag). Until M7, the new pages live at their URLs but the holding page stays at `/`, and the new pages carry `noindex` and are not linked from it.

| # | Delivers (what works when it's done) | Depends on | Leaves out |
|---|---|---|---|
| **M1 Foundation** | Marketing layout (header, footer, release banner), design tokens, fonts, the bands and the rim motif, base components (buttons, copy-command box, tabs, section and chapter layouts, card), the marketing page checks (structure, metadata, `noindex`, internal links, weight, contrast). A style page at `/styleguide/` (noindex) to review the look. The docs already link back to `/` through Starlight's site title; the data-file checks come with their data in M3–M5 | PR #161 merged (the `website/` project) | Real content, scenes |
| **M2 Homepage** | `/` content as in §6 (at a preview URL, the holding page still live): copy drafted in the M2 plan (for the user's review), dark UI screenshots in window frames, the agents transcript, install command, cloud links, stack diagram, FAQ | M1; cloud URLs; install command | Compare details, benchmark numbers |
| **M3 Pricing, self-hosting, security** | `/pricing/` from the plans data file with the units calculator and spend cap; `/self-hosting/`; `/security/` | M1; D172–D178 text confirmed | Enterprise contract terms |
| **M4 Integrations** | `/integrations/` generated from the docs matrix, logos, links to the docs pages | M1; the docs matrix | — |
| **M5 Compare** | `/compare/` and nine competitor pages (§9.3), facts in data files with sources and visible "checked on" dates, each fact verified by a Sonnet agent against its source, the staleness check | M1 | Benchmark numbers |
| **M6 Benchmarks** *(after launch, §9.2)* | `/benchmarks/` from a results file: machines, versions, dates, named competitors, tradeoffs, reproduction script; engine numbers added to the homepage proof strip | The benchmark itself (D23) | — |
| **M8 Main sync** | The site matches main as of 2026-11-22: rebased onto it, the integrations parser takes the coding-agent rows, the web demo link (§9.6), six chapters and the coding-agents section, new compare rows (annotation queues, issue detection, coding-agent tracing, in-app assistant) with sourced facts, pricing and self-hosting updated | M1–M5, M7; main's D191–D195 | Benchmarks (M6) |
| **M9 Pricing feature table** | A feature table on both tabs of `/pricing/` (§9.9): limits first, then grouped features. Every row links to its docs page or cites a decision, and a test checks both. On phones, one Cloud plan column shows at a time. It replaces the "Every plan" list | M3, M8; PR #227 (the Cloud / Self-hosted switcher) | Docs for the Enterprise-only rows |
| **M10 Focused screenshots** | Each homepage screenshot is a crop of one panel, captured in its own window size (`viewport`, `clip` in `screenshots.json`). The product chapters show text beside the picture on wide screens, the hero keeps its picture under the text, and no picture is shown larger than its CSS size (§9.10.2) | M2, M8 | The docs' own screenshots |
| **M12 Hero scenarios** | Three more hand-written traces in the mock dataset (a coding agent, a research agent, a document extraction pipeline), their screenshots, and tabs in the hero that switch between the four (§9.10.3) | M11 | One scenario per product chapter (M13 replaces the research agent with a voice agent, §12) |
| **M13 Voice story, trace list, issues, alerts** | The voice trace replaces research (mocks, hero shot, tab); the mock trace list applies `status`, `tags` and `name` filters; each story's issue and alert rule in the mocks (§12) | M12 | The other mock objects |
| **M14 Prompts, dashboards, queues, assistant** | Each story's prompt and playground answer, dashboard, annotation queue and assistant conversation in the mocks (§12) | M13 | Screenshots and the site |
| **M15 Story screenshots and switcher** | One chapter shot per story (24), the stories module with outcome captions, hero tabs and chapter chips in sync through `?story=`, lazy panels (§12) | M14 | — |
| **M16 A mock project per story** | Three more mock projects (Code agent, Voice desk, Invoice pipeline) with traffic generated per profile; the story objects move to their projects; story shots retaken (§13) | M15 | — |
| **M17 A page per story** | Each story shown through its own page per chapter: sessions, transcript, SQL, spans by TTFT, check and judge evaluators, an experiment, prompt versions, the dashboard editor (§13) | M16 | — |
| **M7 Launch** | The holding page replaced by the homepage; `noindex` removed; sitemap, `robots.txt`, Open Graph images per page, canonical URLs, 404 page; Web Analytics on; final review pass on every page by the user | M2–M5 | Benchmarks (M6) |

## 11. Risks

- **The screenshots drift from the real UI** as the app changes. Mitigation: `just docs-screenshots marketing-…` rewrites them from the mocks; refresh them in the same PR as a visible UI redesign.
- **Comparison claims go stale or are wrong**, which costs trust (the Helicone anti-pattern). Mitigation: sources, dates, the staleness check, "when to choose them" honesty.
- **"Everything at once" delays the launch.** Mitigation: M6 is outside the launch (§9.2). Every other milestone ships to `main` behind `noindex`, so nothing waits on a long branch.
- **Font weight and page speed.** Three families is the most; subset them and preload only what's above the fold.

## 12. Story switcher (M13–M15)

Answers of 2026-11-24 (the user): every homepage picture follows one of four stories, in this order: **Support agent**, **Coding agent**, **Voice agent**, **Document extraction** (D-W-24, D-W-25). The voice agent replaces M12's research agent: research's point (cost) overlapped the other stories and fewer buyers build it, while voice agents are a fast-growing production use whose failure is latency, which the timeline shows best. Every caption names an outcome the reader wants (what they find or save), not what the picture contains. Each section shows the story's own data, tied to its hero trace, and a row of story chips above each picture switches the whole page (D-W-25).

### Goals and non-goals

Goals:
- The reader picks a story once, and every picture on the page (the hero and the six product chapters) shows that story.
- Each picture's content is hand-written and names the story's real objects. The coding story's issue, alert and dashboard are about `code-agent` and its failed `run_tests`, and their links open the hero's `coding-agent` trace in the demo.
- The assistant chapter shows the assistant answering a question about the story, then building a dashboard from it, with that dashboard behind its window.

Non-goals:
- Per-section stories. One choice drives the whole page.
- A fifth story.
- Audio in the voice trace. The mocks serve only PNG media; the voice story sells latency, which needs none.
- Changing the chapters' text. Only the picture, its alt and a one-line caption change with the story.
- Product changes. The pictures use what the UI does today.

### Architecture

**Mock data (web/, M13–M14).** Each story adds hand-written objects to the web UI's mocks, next to the generated data. All four stories live in the same mock project at once, so the demo shows them all, and each object is reached by its own id or URL. There is no global "story mode".
- **Traces.** The four hero traces already exist (`SHOWCASES`, M11–M12). The mock trace list learns to apply the filter language for `status`, `tags` and `name`, reusing the widget query's compiler, so a story's list can be a real filter.
- **Issues, alert rules, prompts, dashboards.** One of each per story, with fixed ids. Those that need the dataset's traces or clock (issues, alert rules) are served by the large mock's handlers (`web/src/mocks/generated/`), before the fixtures; the others by browser-only mock modules, as `fallbacks.ts` and `dashboards.ts` do today. The checked fixtures under `fixtures/api/` stay as they are.
- **Annotation queues.** One queue per story, with its own score fields, whose item is the story's hero trace. They are served by the large dataset's handlers, which own that route.
- **Assistant.** One scripted conversation per story, chosen by the question sent. Its stored messages match the live ones, so the picture shows the real question. Every trace it names exists.
- **Playground run.** The scripted answer depends on the prompt being run, so each story's prompt gets its own answer, tokens and cost.

**Screenshots (M15).**
- Each `marketing-*` shot gets one entry per story in `website/screenshots.json`.
- The support story keeps today's names, and the others take a `-coding`, `-voice` or `-extraction` suffix.
- That makes 28 marketing shots: 4 hero and 24 chapter.

**The switcher (website/, M15).**
- One data module lists the stories. For each story and each picture, it gives the screenshot, its alt and its caption.
- The hero's tabs and a chip row above each chapter picture both show the four stories, and they stay in sync: picking a story anywhere switches every picture.
- The choice is written to the URL as `?story=coding`, so a link opens that story.
- Pictures of the stories not shown are in hidden panels and load lazily. The page's first load is the same as today.
- Without JavaScript the page shows the support story.

### The stories, section by section

The content below is the direction; the plans fix the exact words. Captions are one line under the picture.

| Section | Support agent | Coding agent | Voice agent | Document extraction |
|---|---|---|---|---|
| **Hero** (done, M12) | `support-agent` trace | `coding-agent` trace, failed then passed tests | `voice-agent` trace: one caller turn (speech to text, the LLM, a booking tool, text to speech) that took 3.1 s, its reply starting 2.8 s after the caller stopped talking | `extract-invoice` trace, a retry after a failed check |
| **Trace list** | Traces tagged `email` | Only the failed traces (`status = 'error'`) | Voice traces sorted by duration (M15 fixes the exact URL) | Traces tagged `billing` |
| **Explore: assistant** | "Which answers scored low today?", then "Build me a dashboard of answer quality" | "Which traces failed in the last day?" (answer names `coding-agent`), then "Build me a dashboard for code-agent: failed runs, cost per run and p95 duration" | "Which calls were slow today?", then "Build me a dashboard of voice latency" | "Which invoices needed a retry?", then "Build me a dashboard of invoices, schema-check failures and cost" |
| **Issues** | "Customers ask how to export traces to CSV" (topic); its first example is the hero trace | "run_tests failed: 1 failed, 47 passed" (error); its example is the hero trace's `run_tests` span | "Reply started more than 800 ms after the caller" (score); its example is the hero trace's `check_availability` span | "Invoice total doesn't match its line items"; its example is the hero trace's first schema check |
| **Evaluate: annotation queue** | "Support answers": correctness and relevance, on the support trace | "Patch review": tests pass and a review verdict, on the coding trace | "Call review": resolved and tone, on the voice trace | "Invoice fields": schema valid and fields correct, on the extraction trace |
| **Prompts: playground** | `support-reply` v2, answering the CSV question | `write-patch`, producing a diff | `voice-reply`, a short spoken answer | `extract-invoice`, on two models side by side |
| **Alert** | Correctness average under 0.7 on `support-bot` | Error rate above 5% on `code-agent`, firing | p95 latency above 1.5 s on `voice-agent`, firing | Scanned PDFs over 10 an hour |

The assistant chapter's picture is wider than today: the story's dashboard fills the page, and the assistant window sits over it, showing the question, the answer, the build request and the "Drafted a dashboard" card. The picture shows the dashboard as saved; the caption doesn't describe the apply step.

### Decisions and trade-offs

- **One switcher for the page, with chips repeated per picture** (the user). Tabs per section with their own state were rejected: they make six independent choices and a busy page. Hero-only tabs were rejected because they're out of reach once the reader scrolls past the hero. A sticky bar was rejected because it's chrome that follows the reader for one feature.
- **Objects per story, no story mode in the mocks** (planner). A `?story=` mode in the mocks would make the demo show one story at a time and would need every handler to branch. Separate objects keep the demo whole and each shot a plain URL. The exceptions are the assistant, chosen by the question, and the playground run, chosen by the prompt. Both have one endpoint whose answer must differ.
- **The checked fixtures stay untouched** (planner). `fixtures/api/` is round-trip checked against the server types, and web tests count its rows. Story data lives in browser-only mock modules, which tests don't load unless they ask.

### Milestones

- **M13 Voice story, trace list, issues and alerts.** The voice trace replaces the research trace (mocks, hero shot and hero tab). The mock trace list applies `status`, `tags` and `name` filters. Each story has its issue and its alert rule in the mocks, reachable by URL in `just web-dev`. Depends on M12.
- **M14 Prompts, dashboards, queues and the assistant.** Each story's prompt and playground answer, dashboard, annotation queue and assistant conversation in the mocks. Depends on M13.
- **M15 Story screenshots and switcher.** 24 chapter shots from M13 and M14's data, the stories module with outcome captions, the hero tabs and chapter chips in sync through `?story=`, and lazy panels. Depends on M14.

### Open questions

None.

## 13. A project and a page per story (M16–M17)

Answers of 2026-11-24 (the user): the story pictures looked alike, since every story showed the same page per chapter over the same generic data. Both levers (D-W-26), the user's choice:

- **Lever A, first (M16): one mock project per story, with generated traffic shaped like it.** The org `acme` gets three more projects beside "Support bot": "Code agent" (`code-agent`), "Voice desk" (`voice-desk`) and "Invoice pipeline" (`invoice-pipeline`). Each has its own generated traces, with names, models, durations, costs, tags, sessions, error rate and payload text typical of its kind:
  - coding runs are long, with many tool calls and failing tests;
  - voice calls are short sessions of turns (time to first token comes with M17 if a picture needs it);
  - extraction is a batch of cheap invoices with a schema check;
  - support keeps today's data.

  Each story's hand-written objects (hero trace, issue, alert, prompt, dashboard, queue, assistant chat) move to its project. The story pictures then show the story's own project and data. Mock details (planner, 2026-11-25): each hero keeps its index and trace id in its own project; a story object is served only under its story's project; in the code agent's project the coding objects no longer filter on the `retry` tag; voice traces form calls of 4 to 12 turns with one caller each; extraction traces have no session and no user.
- **Lever B, then (M17): each picture is chosen for its story.** Answers of 2026-11-25 (the user, picture by picture): the Trace and Evaluate chapters show a different page per story; Explore, Issues, Prompts and Alerts keep one page, with each story's own data. Showing four different pages in every chapter would be too much for a viewer. Every picture is a page the chapter's text names. The pictures of a story tell one loop across the chapters: the trace, then what the assistant finds, the issue that groups it, how it is measured, the prompt that fixes it, and the alert that watches it.

| Chapter | Support agent | Coding agent | Voice agent | Document extraction |
|---|---|---|---|---|
| Trace | The Session page: a hand-written conversation of four turns by `support-agent` (the CSV export question, the export limit, a weekly schedule answered wrong with a thumbs-down and a low correctness, a short close); the conversation only, no trace pane | The Trace page's Transcript view of the coding hero trace: read the file, the tests fail, the patch, the tests pass | The Trace page's Chat view of one caller turn (not the hero's): the caller's recording with its transcript, the tool call, the agent's spoken reply | The trace list of one `invoice-pipeline` batch: about 200 invoices with their status, the batch's totals line (200 traces, 3 errors, its cost) instead of a re-extracted tag, three rows whose schema check failed twice |
| Explore | The assistant window alone. "Why did our thumbs-down rate double since Tuesday?" Answer: 62% of the downvoted answers since Tuesday 14:00 quote the old pricing page; retrieval returns `pricing-2025.md`, back in the index since deploy v2.14; 41 traces | "Why did cost per run triple yesterday?" Answer: 18% of runs call `run_tests` more than five times and make up 70% of the cost; they all touch `packages/api`, whose test suite times out at 30 s, so the agent retries | "Why do callers hang up?" Answer: 71% of hang-ups follow a turn over 2.5 s; almost all are `check_availability` on the calendar API (median 1.5 s, p95 3.5 s) | "Which vendors' invoices fail the most, and why?" Answer: Nordic Freight, 38% re-extracted, all on `total`; it writes amounts as "1.234,56" and the schema expects a dot; three other vendors use the same format |
| Issues | The issue page of a topic issue: "Customers asking for SSO on the Starter plan", 214 conversations this week, up from 30 | An error issue: "ContextWindowExceeded: diff over `<n>` files", new since Monday, 9% of runs | A bad-score issue: "Agent talked over the caller", an interruption check under 0.5 on 6% of calls, rising | An error issue: "Scanned PDF: OCR returned no text", 3% of invoices |
| Evaluate | The annotation queue: a reviewer grades a conversation that came from a thumbs-down, rubric correctness, tone and policy, "4 of 25" | A check evaluator (code, no LLM), "tests pass after the patch", with its pass rate by day | An LLM judge, "did the agent read the booking back to the caller?": its prompt, its model and a scored call with its reason | An experiment: extraction prompt v3 against v4 on 300 labeled invoices, accuracy per field (`total` 97% to 99%, `vat_number` 91% to 96%) and cost |
| Prompts | The playground with support-reply v2 and v3 side by side on the queue's conversation: v2 fails, v3 answers right, tokens, cost and latency of each | write-patch v4 adds "Split changes over 20 files into steps", run on the input of a run that hit the context window error; the answer is a three-step plan, not a diff | voice-reply v5 adds "Read the booking back in one sentence"; the reply "Done: Friday at 9:30, with Dr. Martin.", with its latency | extract-invoice v6 adds "Amounts may use comma decimals", run on the Nordic Freight invoice; "1.234,56" reads 1234.56 |
| Alert | The alerts page, a score rule: average correctness under 0.7 over one hour, firing, to the team's Slack channel, the chart dipping after a deploy | A cost rule: spend over $40 in one hour, firing, the chart tripling yesterday | A p95 latency rule on traces: over 1,500 ms, firing, to a webhook channel named "On-call" | An issue rule: "Scanned PDF: OCR returned no text" over 10 occurrences in one hour |

Rules for these pictures:
- **The data backs every number.** Each count and percentage a picture shows (an assistant's answer, an issue's counts, an experiment's accuracy) is what the story's project data gives; a test computes it from the dataset at the screenshot clock and compares.
- **Only real actions.** The assistant's buttons are actions it has: open the matching traces as a filter, draft a dashboard. No alert or prompt-fix actions it doesn't have. Alert rules use the metrics of spec 07 (error rate, cost as a sum over a window, p95 latency, score, issue occurrences) and its channel kinds (Slack, webhook, email); there is no per-run cost metric and no PagerDuty kind.
- **The support conversation in the queue fails for a reason a prompt can fix,** since the Explore picture blames retrieval for the old price, not the prompt.
- **Voice audio** comes from the themed audio player of cetra-ai/cetra#254: an audio part carries its transcript (`"transcript"`) and the Chat view shows it under the player. The mock serves short audio files as `data:` URIs (the screenshots run on the mocks).
- **The support Session picture** waits for the session page's redesign (another session) and is taken from the page as it is then.

Answers of 2026-11-25 on what the UI can't show yet (the user): the playground compares prompt versions side by side, an `issue_occurrences` alert rule gets a chart, and the trace list gets a total of the selected or filtered rows; the "App UI changes on main" session builds the three, and the pictures that need them wait for them on main. The coding Evaluate picture shows the online rule page of the `tests_pass` check with its hourly pass-rate line (no daily chart). Planner's choices, not objected to: the issue pictures show the kind through the source's name ("Topics", "Errors", "Bad scores") and counts, not percentages; the voice judge picture is the evaluator's Configuration tab with a test result on one call; the coding cost rule's step happens within the chart's last 24 hours; the assistant's actions are the Filter card's "Open in traces" and the Dashboard card's "Open in dashboard editor"; the hero pictures stay as they are.

### Milestones

- **M16 A mock project per story.** The three new projects in the mocks, a generation profile per project, the large mock serving each project its own dataset, the story objects moved to their projects, and the existing story screenshots retaken in their projects. Depends on M15. The support project's generated traces keep today's draws; tests that read a coding, voice or extraction hero from the support dataset move to its story's profile.
- **M17 Story data.** The generator learns what the pictures need: a hook that sees each trace's start time, more than one error class per profile, planned spans (a tool's count, name and duration), trace markers (tags, release), hand-written sessions and audio parts. Each profile then carries its patterns (support: the old pricing document, the thumbs-down rise since Tuesday 14:00, the SSO topic, a dipping correctness; coding: `run_tests` retries in `packages/api`, the context window errors since Monday, the cost step; voice: slow `check_availability` turns before hang-ups, the interruption score, the p95 rise; extraction: Nordic Freight's comma amounts, empty OCR on scanned PDFs, one batch of about 200 invoices with three double failures), each number backed by a test. Depends on M16.
- **M18 Pictures that need no UI change.** The story objects and screenshots of the pictures today's UI can show (Trace: support, coding, voice; Explore: all four; Issues: all four; Evaluate: all four, including the invoice dataset and experiment; Prompts: coding, voice, extraction; Alert: support, coding, voice), the switcher's names for chapters whose page differs per story, and the alts and captions. Depends on M17.
- **M19 Pictures that wait for the UI.** Prompts · Support (versions side by side), Alert · Extraction (the occurrences chart), Trace · Extraction (the selection total), each once its UI feature is on main. Depends on M18 and on those three features.
