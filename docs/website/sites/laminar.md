# Laminar (https://www.lmnr.ai → redirects to https://laminar.sh)

## Hero
- **Headline (verbatim):** "Ship reliable agents"
- **Subhead (verbatim):** "Laminar is an open-source agent observability platform. It automatically catches agent failures at scale and helps you fix them." The words "open-source" are a terracotta link to the GitHub Apache-2.0 licence.
- **Primary CTA:** "Get started – free", a terracotta oklch(0.70 0.116 46) (≈ #D27A55) button → `/sign-up`.
- **Secondary CTA:** "Setup with [coding-agent icon] in minutes", an outlined button with no href. It opens a setup panel, presumably agent-assisted setup.
- **Nav:** Docs, Blog, Pricing, Book demo, a GitHub star counter (3,485), Sign in, Sign up. A top banner reads "Introducing flow-1, frontier trace intelligence at 1/20th of the cost – Watch the video".
- **Hero visual:** a dark, dotted-grid loop diagram of the product cycle. "Your agent" → Production → "Traces" (grid) → "flow-1" (their model) → "Issues" (grid of coloured warning icons) → "Eval cases" (✓/✗ grid) → "Fix" → back to the agent, with "Development" and "Production" halves. It is a schematic, not a screenshot. Five customer logos in grey tiles sit at the bottom of the first viewport (Browser Use, OpenHands, metaforms, vorflux, passionfroot).

## Structure
1. Announcement bar (flow-1 model launch) and nav with GitHub stars.
2. Hero: headline, subhead, two CTAs, loop diagram, five logo tiles.
3. 01 "See clearly what your agent is doing": a dark trace transcript UI (Gemini calls, web_search, fetch_page, cost and tokens per step, cost heatmap timeline).
4. 02 "Discover failures without defining them" (Signals) and 03 "Understand agent failures at scale" (Signal clusters): sticky scroll-driven panels. **Capture problem:** they rendered as a ~1,000px blank area in the full-page capture, though the text exists in innerText.
5. "Frontier trace analysis at 1/20th of the cost": flow-1 scatter chart (trace-analysis intelligence % vs traces analysed per dollar) against Claude Opus 5, Sonnet 5, GPT-6 Sol/Luna and Gemini 3.8 Flash.
6. 04 "Be notified and investigate in Slack.": Slack thread mock with alert, cluster and a conversational follow-up.
7. 05 "Codex, fix my agent with Laminar": terminal mock of a coding agent using `lmnr-cli sql query` and patching a prompt.
8. 06 "Did the new release break anything?": eval comparison UI (0.41 → 0.58 reward, per-row durations).
9. "Two lines to integrate with your stack": 15 framework/SDK names in three columns.
10. "Comprehensive platform for agent development.": a 2×3 card grid (input extraction, custom SQL dashboards, full SQL access with a query and result table, annotation UI, browser screen recording, full-text search).
11. One testimonial (Magnus Müller, CEO, Browser Use: "millions of agent sessions…").
12. "20x cheaper storage": a short bar comparison, Laminar vs an unnamed "Competition", with "Read more".
13. "Open source from day zero": 3.5k GitHub stars, Apache 2.0, "Set up with Docker", Helm on AWS/GCP, and a terminal showing `git clone … && docker compose up`.
14. "Ready for Enterprise": Apache 2.0, Helm, PII redaction, HIPAA and SOC 2 Type II seals.
15. Closing CTA "Ship reliable agents" (Get started – free / Book a demo), footer, and a dotted grid of warning icons as a decorative footer.

## Proof
- **First viewport:** five small logos (Browser Use, OpenHands, metaforms, vorflux, passionfroot). These are agent-builder startups, not enterprise names.
- **GitHub stars** in the nav (3,485) and again in the OSS section. Y Combinator in the footer.
- **One quote** (Browser Use CEO), mid-page.
- **Numbers:** "1/20th of the cost" and "20x more traces per dollar" for flow-1 (with a chart against named frontier models), and "20x cheaper storage" (vs an unnamed "Competition", no method shown). HIPAA and SOC 2 Type II.
- **Missing:** no customer stories, no usage volume, no ingest or query benchmarks.

## Product shown how
- Product UI starts right after the hero (section 01, about 1 viewport down) with a realistic dark trace transcript built from HTML.
- Every numbered section pairs copy with a coded mock: Slack thread, Claude-Code-style terminal, eval diff, SQL editor with a result table, annotation panel, browser-recording player, full-text search with highlights.
- 13 code blocks, including SQL, a shell (`git clone … docker compose up`) and a CLI transcript. No video embedded on the homepage; the banner links to a video.
- No live demo or playground.

## Visual style
- **Type:** headings in "sansLanding" (42px, weight 480, -0.63px tracking); small, restrained headings rather than a giant display. Inter for body. Monospace for the terminal and SQL.
- **Palette:** near-black graphite background oklch(0.134 0 0) ≈ #1A1A1A, panels in a slightly lighter grey ≈ #222, off-white text, one warm terracotta/orange accent ≈ #D27A55 (CTAs, links, highlighted words). Status colours (green/red/purple/yellow) appear only inside the mocks. Dark mode only.
- **Density:** low to medium. A narrow (~880px) centred column with lots of black space and numbered sections (01–06), quiet and engineer-oriented.
- **Motion:** scroll-pinned sections (02/03), a blinking terminal cursor, and an animated dotted-grid banner and footer.
- **Signature devices:** dotted-grid textures; numbered sections; every claim paired with a coded mock in a grey frame; terracotta as the single accent.

## Pricing / self-host / comparison pages
- **Pricing (`/pricing`):** four dark cards (Free $0 with 1 GB and 7-day access; Starter $30 with 3 GB then $2/GB, highlighted in terracotta; Pro $150 with 10 GB then $1.50/GB and 6-month access; Enterprise custom with "On-premise"). Then an interactive calculator (tokens per run × runs per month × % analysed by Signals → estimated monthly bill), a feature matrix, and a FAQ. The page has no H1 and the area above the cards is empty, which looks unfinished. Signals (their LLM analysis) is billed per Mtok.
- **Self-host:** presented on the homepage only as "Open source from day zero / Set up with Docker / Deploy on AWS or GCP with Helm charts" with a `docker compose up` terminal. The infra footprint is not shown: competitors.md lists Postgres + ClickHouse plus optional RabbitMQ, Redis, Quickwit and S3, and none of this appears. The pricing page has no self-host column; "On-premise" is an Enterprise bullet.
- **Vs pages:** none linked. Competitors are unnamed ("Competition").
- **Docs entry:** "Docs" is the first nav item; no docs CTA in the hero.

## What makes it sell
- Crisp positioning: "open-source agent observability that automatically catches failures and helps you fix them". The loop diagram makes "catch → cluster → eval → fix" concrete.
- The product is shown in every section as a coded mock with believable agent data. It feels like a real tool, not marketing.
- A clear AI-native differentiator: their own fine-tuned model (flow-1) with a cost/quality scatter chart against named frontier models.
- It speaks to coding-agent workflows (Codex/Claude Code terminal with SQL over traces) and Slack. It knows where its users live.
- Quiet, tasteful dark aesthetic with one accent: credible to engineers.

## Weaknesses we can exploit
- **The self-host story is a single `docker compose up` line that hides a Postgres + ClickHouse (+ optional RabbitMQ, Redis, Quickwit, S3) stack.** It is also written in Rust, so "Rust" alone isn't a differentiator against them. "One binary, no database" is.
- **Thin proof:** five small startup logos, one quote, 3.5k stars, and "20x cheaper storage" against an unnamed competitor with no method.
- **Generic headline.** "Ship reliable agents" is almost identical to Braintrust's "Ship quality agents at scale"; the real hooks (open source, automatic failure detection) are in the subhead.
- **Rough edges:** the pricing page has no heading and an empty band above the cards; the scroll-pinned sections can render blank (as in this capture); the free tier has only 7-day access and 1 seat.

## Steal for cetra
- Numbered feature sections (01–06), each one claim plus one coded UI mock with realistic agent data in a framed panel.
- An "Open source from day zero" block: a short list (stars, licence, Docker, Helm) beside a terminal showing the exact install. Ours would be a one-line binary install followed by "listening on :4318".
- An interactive pricing/cost calculator driven by workload inputs (runs/month × tokens/run). Ours could estimate disk and RAM for self-hosting rather than a bill.

## Scores
- Beauty 4/5: tasteful dark graphite and terracotta, but sparse and sometimes empty.
- Selling 3/5: good product story, weak social proof and generic headline.
- Clarity (5-second test) 4/5: the subhead says open-source and what it does.
- Proof 2/5: five small logos, one quote, an unnamed "Competition" chart.
- Product-showing 5/5: realistic coded mocks right after the hero, in every section.

## Best screenshots
- `caps/laminar-hero.jpg`: dark hero with the agent → traces → flow-1 → issues → evals → fix loop diagram.
- `caps/laminar-part03.jpg`: coding-agent terminal, eval comparison UI and the SQL-powered feature grid.
- `caps/laminar-part04.jpg`: testimonial, "20x cheaper storage" bar, open-source block with `docker compose up` terminal.
