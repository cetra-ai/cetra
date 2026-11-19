# PostHog (https://posthog.com)

## Hero
- **Headline:** "Your product's context layer" ("context layer" set on a pale-blue highlight).
- **Subhead:** "PostHog ingests and stores your analytics, errors, replays, and business data so you and your agents can query and act on it." Below it: "Join 500,000+ teams already shipping with PostHog."
- **Primary CTA:** "Get started" (orange #CD8407, goes to app.posthog.com/signup). It sits in a white "Set up for free" card with three checkmarks: "97% of users pay us $0", "No credit card required", "Setup wizard installs PostHog for you".
- **Secondary CTA:** "Install with AI" (outlined, opens the wizard). Further down: "Happier in the terminal? Skip the browser:" `npx -y @posthog/wizard@latest`.
- **Hero visual:** the whole site is a fake desktop OS. There is a grass wallpaper with a 3D hedgehog, desktop icons down both sides (Home, MCP, Self-driving, Pricing, Docs, Demo, Talk to a human, About us, Changelog, Handbook, Store, Careers, Trash), and each page opens as a draggable window. Under the hero sits a purple tabbed panel ("Ask PostHog anything / Give agents product context / Ship with PostHog") with an auto-playing mock AI chat that answers "Why are signups growing, but paid conversions falling?" It is labelled as sample data.

## Structure
1. Hero with headline, subhead and the "Set up for free" card with two CTAs and a terminal one-liner.
2. Tabbed autoplay demo of the AI agent, plus a list of product chips ("Built-in tools for your agents", 18 products).
3. "Social proof" logo wall, with a joke toggle between "Colorful logos" and "Sleek logos".
4. "How PostHog helps you build better products", in four numbered steps:
   - 1. Get the data in: a mock app beside a live event stream (`posthog.capture()`), and an AI analytics box-plot widget showing latency, tokens and cost.
   - 2. Query with your agent: an ASCII terminal running "posthog mcp", the `npx @posthog/wizard mcp add` command, and client logos (Claude, Cursor, Codex…).
   - 3. Give the data to your agent: a mock "Self-driving inbox" with triaged reports.
   - 4. Let PostHog self-drive: a mock GitHub PR, "PostHog wants to merge into main".
5. "Self-driving in PostHog, right now": a live counter ("117 pull requests … in the last 7 days") and real PR titles with numbers.
6. "Shameless CTA": a parody software box ("PostHog Web, Digital download, $0, >1 left at this price!!").

## Proof
- "500,000+ teams" in the hero and "97% of users pay us $0" in the CTA card.
- A logo wall in section 3, with a self-aware caption ("Yes they actually use us…").
- Dogfooding numbers: 117 merged PRs in 7 days, with real PR titles from their own repo. This is unusual and very credible.
- The pricing page adds "600k+ companies". The vs page names Supabase, Lovable and ElevenLabs.
- The homepage has no testimonial quotes and no benchmarks.

## Product shown how
- The homepage uses scripted, interactive-looking HTML recreations of the product, not screenshots: the chat agent, the live event table, the inbox and the PR. These start in the first viewport.
- The product page (/product-analytics) puts a real chart screenshot in the hero, with a hedgehog illustration in front of it.
- Every product page has a "Demo" desktop icon and a "Watch a demo" link.

## Visual style
- **Type:** custom "RoundHog" for headings and body. The h1 is 36px/800 with -0.9px letter-spacing, which is small for a hero because the window frame constrains it.
- **Colours:** page bg #EEEFE9 (warm off-white), window body pale sage, CTA orange #CD8407/#EB9D2A with a 3D bottom border, accent purple #B62AD9, highlighter-yellow and blue text marks.
- **Mode and density:** light, very dense. 0 videos and 0 canvas; motion comes from autoplaying DOM demos. 14 code blocks.
- **Signature devices:** the desktop-OS metaphor, hedgehog mascot art everywhere, highlighter marks on key words, chunky buttons, and self-deprecating jokes (the cookie banner with a caricature, "Not endorsed by Kim K").

## Pricing / comparison pages
- **Pricing (pages/posthog-pricing):**
  - Headline "97% of companies use PostHog for free." with a retro "PostHog 3000" software-box illustration.
  - Then a per-product free allowance table (1M events, 5K recordings, 100K exceptions, AI Observability 100K events/month…).
  - A two-step "Free → Pay-as-you-go" ladder, billing limits ("going viral won't require another funding round"), and an interactive pricing calculator.
  - "Talk to a technical account exec – no SDRs".
- **vs page (pages/posthog-vs, PostHog vs Amplitude):**
  - A long, authored, dated comparison article with a sidebar listing a dozen "best X tools" listicles.
  - Feature tick tables (✓/✗) per product, plus free-tier and pricing comparisons and "When to choose…" guidance.
- **Docs:** a top-nav item and a desktop icon. Each product page has Product / Pricing / Docs / Learn tabs.

## What makes it sell
- Free tier as the headline offer: "97% pay $0" is stated three times before you scroll.
- One-command install (`npx @posthog/wizard`) on every page, with an AI-install alternative.
- Dogfooding proof with live numbers and real PR titles.
- The personality is unforgettable. Humour lowers the guard of developers who distrust marketing.
- Product pages are structured like docs (Who is it for, MCP tools, example prompts), so they serve evaluators well.
- SEO comparison and listicle content at scale captures "X alternatives" searches.

## Weaknesses
- The desktop-OS metaphor costs clarity. The 5-second test is cluttered, the h1 is small and the viewport is crowded.
- Content sits in scrolling windows, which hurts mobile, deep linking and screenshots (page height 900).
- Its breadth (18+ products) dilutes the single message.

## Steal for cetra
- A "Set up for free" card next to the headline: 3 checkmark facts, a primary CTA, and a copyable one-liner.
- A numbered 4-step "how it works" (ingest → query → evaluate → act), each step paired with a live-looking HTML mini-demo instead of a static screenshot.
- Live dogfooding numbers (e.g. spans we ingest about ourselves, real eval runs).
- A free-tier-first pricing page, plus dated, authored "cetra vs X" pages with ✓/✗ tables and "when to choose them" honesty.

## Scores
- Beauty 4/5 — distinctive, crafted illustration world; busy, not elegant.
- Selling 5/5 — free, one-command install, proof, humour: relentless.
- Clarity 3/5 — OS chrome and 18 products blur the core message.
- Proof 4/5 — 500k teams, live PR counter; no quotes.
- Product-showing 4/5 — rich scripted demos, few real screenshots.

## Best screenshots
- caps/posthog-hero.jpg — desktop-OS metaphor, setup card with free facts and the agent demo tabs.
- pages/posthog-pricing-hero.jpg — "97% use it for free" pricing hero with the retro software-box art.
- pages/posthog-product-analytics-hero.jpg — product page as a docs-like window with a sidebar, CTA pair and a real chart.
- pages/posthog-vs-hero.jpg — authored "PostHog vs Amplitude" comparison with a listicle sidebar and a table of contents.
