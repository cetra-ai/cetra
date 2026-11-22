# Langfuse (https://langfuse.com)

## Hero
- **Headline (verbatim):** "Open Source Agent Evals & Observability" (three lines, each line on a yellow highlighter band).
- **Subhead (verbatim, short):** "Langfuse is one open platform for every AI call your company makes and every agent your teams build. … Cloud or self-hosted."
- **Primary CTA:** "Start free" (dark #222220 button with a keyboard-shortcut chip "S") → `/cloud`.
- **Secondary CTA:** "Documentation" (light button, shortcut "D") → `/docs`; a third button "Onboard with AI" (no href, opens a prompt panel).
- **Above the headline:** a stat strip, "Used by 21 of the Fortune 50 · 90B+ observations/month · 100,000+ engineers building on Langfuse".
- **Hero visual:** none. The headline sits in a thin-ruled box with corner tick marks, with a 2×6 customer logo grid directly below it (Ramp, Canva, Expedia, Pigment, Khan Academy, Hugging Face, Intuit, SumUp, Merck, Samsara, Cisco, Rocket Money). Six logos carry a tiny "Read story" link.
- **Page chrome:** the page is framed like docs. The left rail shows Community Stats (GitHub stars 37.4k, Q&A threads 1.9k, roadmap threads 1.7k, "Latest OSS release: today"), the changelog, self-hosting guides and blog posts. The right rail is an "On this page" table of contents. A floating "Ask a question" AI box sits at the bottom. The top banner reads "Langfuse launches AI Gateway". The logo carries "by ClickHouse" (Langfuse is now part of ClickHouse, Inc.).

## Structure
1. Announcement bar (AI Gateway launch) and nav: Product, Resources, Solutions mega-menus, Customers, Docs, Changelog, Pricing, Start free, Get Demo.
2. Hero: stat strip, headline, subhead, three CTAs, logo grid with "Read story" links.
3. "Every AI call. One platform.": a boxes-and-arrows architecture diagram (agents → SDKs/OTel or AI Gateway → Langfuse modules → MCP/CLI/API/SDKs).
4. Tabbed product screenshot (Observability / AI Gateway / Metrics / Prompt Management / Evaluation / Annotations / Playground), auto-advancing, with a "Watch demo" button. It shows a real trace tree with costs.
5. "Learn how the best teams build with AI.": Academy promo and a loop diagram (Trace → Monitor → Build datasets → Experiment → Evaluate).
6. "All the tools, one integrated platform.": a bento grid of 8 feature cards with greyscale isometric line illustrations, ending in a Canva testimonial strip.
7. "Works with any stack.": languages, gateways, frameworks and model providers as chips, plus a marquee of 100+ integrations.
8. "Open platform. Open source.": self-host targets (Compose, Helm, Terraform AWS/GCP/Azure), MIT, APIs and exports, community numbers.
9. "Made for developers, loved by agents.": the Halo assistant, SKILL.md, CLI and MCP server, with a screenshot.
10. "Enterprise scale and security.": architecture list (ClickHouse, Redis queue, S3), reliability numbers, compliance badges, Canva strip again.
11. "Why use Langfuse?": a two-column table of 10 reasons.
12. "Start improving your agents in under 5 minutes.": copy-paste prompts for Claude Code, Cursor and Codex (tracing, evals, prompts, gateway).
13. FAQ (7 questions, including "How does Langfuse compare to LangSmith, Braintrust, Arize, Galileo, and Datadog?" and "What's the latency impact?"), plus an "Ask anything else" box.
14. Footer.

## Proof
- **Top of hero:** the stat strip (21 of the Fortune 50, 90B+ observations/month, 100k+ engineers) and 12 enterprise logos. Six logos link to customer stories (Ramp, Canva, Khan Academy, Hugging Face, SumUp, Merck).
- **Left rail on every page:** live GitHub stars (37.4k) and "Latest OSS release: today". This shows activity, not just size.
- **Mid-page:** the Canva quote strip appears twice. Reliability numbers: 65M+ SDK installs/month, 90B+ observations/month, 50,000+ companies, 99.9% uptime. Compliance: SOC 2 Type II, ISO 27001, EU/US/Japan regions, FIPS 140-3/Iron Bank, HIPAA, GDPR. Community: "35,000+ GitHub stars, 5,000+ Discord members". This is stale next to the 37.4k in the rail.
- **Missing:** no latency or ingest benchmarks with method; "query millions of traces in milliseconds" has no source. The only performance-ish proof is volume served.

## Product shown how
- Real product UI appears in the third block (about 1.3 screens down). It is a tabbed, auto-advancing screenshot carousel with a "Watch demo" video button (the json counts 1 video).
- Before that, an architecture diagram in the same thin-line style. Feature cards use abstract isometric line illustrations, not UI.
- No code snippet on the homepage (codeBlocks 0). The "get started" block is AI-agent prompts, not code.
- The FAQ mentions a public demo project. Nothing is interactive in-page apart from the tabs.

## Visual style
- **Type:** headings in f37Analog (68px, weight 500, normal tracking), a soft grotesk. Body in Inter. Small mono-style labels ("Read story", dates, "100+ integrations").
- **Palette:** warm off-white/stone background rgb(237,237,232) (#EDEDE8) with panels at #F6F6F3. Near-black #222220 buttons and text. A single lemon-yellow highlighter accent behind key heading words. Logos greyscale. Light mode only.
- **Density:** very high. Three-column app-like shell (left rail, content, right TOC) and long sections with many chips. It reads like documentation more than a landing page.
- **Signature devices:** highlighter-marked headline words; thin hairline boxes with corner crop marks (blueprint feel); keyboard-shortcut chips on every button; persistent "Ask AI" box.
- **Motion:** auto-advancing tabs and a scrolling integrations marquee; otherwise static.

## Pricing / self-host / comparison pages
- **Pricing (`/pricing`):** Cloud/Self-hosted toggle above four tiers: Hobby (free, 50k units), Core ($29), Pro ($199), Enterprise ($2,499). Each tier has "Trusted by" avatar logos. Then a very long feature matrix, a pricing calculator, discounts, and a 15-question FAQ. The FAQ includes "How does pricing compare to LangSmith, Braintrust, Arize, Datadog?" and links an editable comparison spreadsheet. Usage unit: $8 per 100k units.
- **Self-host (`/self-hosting`, a docs page):** the infra footprint is shown openly. A deployment-architecture diagram lists Langfuse Web, Langfuse Worker, PostgreSQL, Redis/Valkey, ClickHouse and S3/Blob storage, plus an optional AI Gateway. A table states plainly that Docker Compose is "Single VM without high availability, scaling, or backups" and that production means Kubernetes/Helm or Terraform. They frame it as a strength: "you run the same infrastructure that powers Langfuse Cloud". The homepage also lists "Clickhouse OLAP database / Async ingestion via Redis queue / S3/Blob storage" as an enterprise-scale feature.
- **Vs pages (`/faq/all/langsmith-alternative`):** long, docs-styled, fact-dense comparison. It opens with a TL;DR and covers license and self-hosting tables, storage (ClickHouse vs LangSmith's new "SmithDB", described as a proprietary Rust engine on Apache DataFusion and Vortex), a worked price example ($621 vs $3,895/month), evals, alerts, MCP and security, then "Why teams move away", a migration path and an FAQ. A "Compare" section exists in the footer. The homepage FAQ names LangSmith, Braintrust, Arize, Galileo and Datadog.
- **Docs entry:** the "Documentation" button in the hero, the nav item, the left rail and the "D" shortcut. The site and the docs share one shell.

## What makes it sell
- The proof starts before the headline: Fortune-50 share, 90B observations/month and 12 logos, all visible without scrolling.
- Open-source credibility is shown live: a star count and "Latest OSS release: today" on every page.
- It covers the whole loop (trace → monitor → datasets → experiments → evals), shown as a diagram and as tabs. Buyers see "one platform" rather than reading it.
- It is honest and specific with engineers: an architecture list, latency FAQ, compliance list, and a worked-dollar comparison against LangSmith.
- It meets developers where they work: SKILL.md, MCP, CLI, and copy-paste prompts for coding agents as the onboarding path.
- The pricing is transparent, with a calculator and a downloadable comparison sheet.

## Weaknesses we can exploit
- **The self-host footprint is heavy and they admit it:** Web, Worker, Postgres, Redis/Valkey, ClickHouse and S3, and Compose is "not for production". A single binary with no external services, production-capable on one VM, is a direct counter-claim they cannot make.
- **No performance numbers with method.** "Millions of traces in milliseconds" has no source; their scale proof is customer volume, not engine speed. A reproducible benchmark page (ingest/s, query p95, RAM on a single box) would beat this.
- **Overloaded, docs-like homepage.** Three rails, a banner, a hiring badge, an AI box, ~14 sections and repeated proof. The 5-second message is diluted and the first viewport has no product UI.
- **Now owned by ClickHouse.** Their storage story is "use ClickHouse", so they cannot plausibly pitch "no database to run". The 35k vs 37.4k star mismatch shows the copy is not well maintained.

## Steal for cetra
- A one-line stat strip above the headline. Ours would carry measurable engine facts (e.g. ingest rate / binary size / RAM) instead of customer counts.
- A self-host page that leads with a deployment-architecture diagram and a "best for / deployment / responsibility" table. Ours would have one box in the diagram, and the contrast does the selling.
- A long, sourced, dated "vs" page format: TL;DR, a table per dimension, a worked cost example, a "which should you choose" section that admits trade-offs, and a migration section.

## Scores
- Beauty 4/5: refined blueprint-and-highlighter look, but cluttered by rails.
- Selling 4/5: strong proof up front, every objection answered somewhere.
- Clarity (5-second test) 3/5: clear headline, drowned by three columns of chrome.
- Proof 5/5: Fortune-50 share, volumes, logos, stories, live stars, compliance.
- Product-showing 4/5: real tabbed UI and demo video, but below the fold.

## Best screenshots
- `caps/langfuse-hero.jpg`: highlighter headline in a crop-marked box, with stats strip, logo grid and docs-style rails.
- `caps/langfuse-part01.jpg`: architecture diagram, tabbed real-UI trace screenshot and the evaluation-loop diagram.
- `pages/langfuse-selfhost-part01.jpg`: self-host architecture diagram showing Postgres + Redis + ClickHouse + S3 + Web + Worker.
