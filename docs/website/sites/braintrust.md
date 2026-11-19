# Braintrust (https://www.braintrust.dev)

## Hero
- **Eyebrow pill (mint):** "One platform for agent observability"
- **Headline (verbatim):** "Ship quality agents at scale"
- **Subhead (verbatim):** "Discover patterns in production, turn them into evals, and improve quality with every release."
- **Primary CTA:** "Start building", an electric-blue rgb(44,31,234) / #2C1FEA pill → `/signup`.
- **Secondary CTA:** "Contact sales", a light grey square button → `/contact`.
- **Tertiary:** a "Build with agents" dropdown with Claude/Cursor-style icons.
- **Hero visual:** no screenshot. A full-width generative "barcode" field of blue vertical ticks, dots, arrows and crosshairs, likely canvas-animated (6 canvases on the page). It reads as a stylised trace/event stream.
- **Logo wall:** a 2×8 grid starts inside the first viewport, under the mono caps label "TRUSTED BY THE BEST AI TEAMS". Logos: Airtable, Notion, Lovable, Instacart, Stripe, Samsara/Robinhood (the wall rotates), Vercel, Dropbox, Coursera, Replit, Superhuman, Granola, Mixpanel, MongoDB, Cloudflare, Semgrep. Several carry "Watch video" / "Read story" links.

## Structure
1. Nav: Product and Resources mega-menus, Customers, Pricing, Contact us, Sign in, and a "Sign up" pill.
2. Hero: eyebrow pill, headline, subhead, two CTAs, "Build with agents", animated tick-field.
3. Logo wall with video and story links.
4. Full-bleed blue band titled "Inspect agent traces in real time": a large, realistic Logs and Trace UI mock, rendered as HTML text (a support-agent example with a sidebar listing Overview, Logs, Dashboards, Patterns, Topics, Review, Playgrounds, Experiments, Datasets, Prompts, Scorers…). It has slide dots, so it is a carousel.
5. Pink band, "Agents fail differently than normal software…". A sticky side index (WORKFLOW / PLATFORM / SCALE / SECURITY / CUSTOMERS), three pillars (trace, evals, catch issues early), an Observe → Evaluate → Improve loop graphic on maroon, plus links to "three pillars" and an "eval maturity assessment".
6. "Agent observability and evals for the whole team.": three alternating rows, each a UI mock next to a feature list: Observability (stacked cost chart), Evals (experiment score-diff grid across GPT/Claude/Gemini/Mistral), Discovery (the Loop agent chat transcript with JSON tool calls).
7. "Everything you need to build smarter, faster": a horizontal card carousel (Loop agent, Pattern automations, Task-specific trace views, Trace to dataset, MCP, Framework agnostic, Native SDKs with a TypeScript `Eval(...)` code sample).
8. Dark-green band, "Brainstore, the database built for AI data at scale. Faster investigations with Nitro.": a benchmark section with a cold/warm cache toggle. It claims 10.95×, 18.28× and 17.48× speedups against anonymised "Competitor A/B", with millisecond bars; "Competitor B: Incorrect results" on phrase counting.
9. "Secure by default. Compliant from day one.": SOC 2 / GDPR / HIPAA seals and six features.
10. Black band, "Built for teams running agents in production.": a bento of video testimonials (Cloudflare, Pylon, eve, Box, Notion "<24hrs to deploy a new frontier model", Replit, Graphite "5% reduction", Retool).
11. Closing "Trace everything" email signup, then a blue footer with a giant wordmark.

## Proof
- **Logo wall in the first viewport:** 16 high-recognition AI-native brands (Notion, Stripe, Vercel, Replit, Cloudflare…), with video and story links.
- **Benchmarks:** the Brainstore/Nitro section has multiplier headlines and raw ms numbers, plus an "Explore the benchmarks" link. The competitors are anonymised as "Competitor A/B", which weakens the claim.
- **Testimonials:** an 8-tile grid with real people, titles, video portraits, and two case-study metrics (Notion <24hrs, Graphite 5%).
- **Compliance:** SOC 2 Type II, HIPAA and GDPR seals.
- **Missing:** no GitHub stars or OSS proof (Braintrust is not open source; GitHub appears only in the footer as "Open source libraries and tools"). No company-wide volume numbers.

## Product shown how
- A realistic UI mock arrives at roughly 1.1 viewports (blue band right after the logos). It is coded HTML, not a bitmap, so it is crisp and carries believable data (order-status support agent, trace with tool call).
- Product mocks are everywhere afterwards: cost chart, eval diff matrix, Loop agent transcript, annotation view, datasets picker.
- 4 videos (testimonials). One TypeScript code sample (`Eval("Support Agent", {...})`). 11 code blocks counted, mostly the JSON inside the Loop transcript.
- No live demo or sandbox. The CTA is signup.

## Visual style
- **Type:** custom "btSans" (60px, weight 400, normal tracking) for headings; light-weight, large, Swiss-feeling. Inter for body. Mono caps for labels ("TRUSTED BY THE BEST AI TEAMS", benchmark legends).
- **Palette:** white base, electric ultramarine #2C1FEA as the brand colour (buttons, hero ticks, product band, footer), plus full-bleed colour bands: bubblegum pink, deep maroon/burgundy, forest green with lime accents, and black for testimonials. Pricing cards reuse green, maroon and blue. Light page with dark bands.
- **Density:** medium. Big type and generous whitespace, but every band is packed with UI mocks.
- **Motion:** canvas tick-field animation in the hero, product carousel dots, card carousel arrows, cache toggle on benchmarks.
- **Signature devices:** the barcode/tick-mark motif, which repeats at section seams and in the footer as stepped "notches" between colour bands; two-tone headings (black sentence plus a coloured or grey continuation); pill-shaped secondary links with an arrow.

## Pricing / self-host / comparison pages
- **Pricing (`/pricing`):** headline "Predictable pricing. Designed to scale." Three bold coloured cards: Starter $0 (green), Pro $249/month (maroon), Enterprise custom (blue). The meter is spelled out in rows: model credits, processed data GB ($4/GB → $3/GB), scores ($2.50/1k → $1.50/1k), retention (14 → 30 days, +$0.50/GB/month). Unlimited users on every tier. Then a usage calculator, a detailed feature table and an FAQ. "6–12 months free for qualifying startups."
- **Self-host:** essentially hidden. The only mention on either page is the Enterprise card ("on-prem or hosted deployment for high volume or privacy-sensitive data"). No infra footprint shown, no open-source product, no self-host docs entry on the homepage.
- **Vs pages:** none in the nav or homepage; competitors are anonymised in benchmarks.
- **Docs entry:** Resources mega-menu ("Docs: How Braintrust works") and the footer. The hero has no docs CTA.

## What makes it sell
- A first viewport full of the right logos (Notion, Stripe, Vercel, Cloudflare, Replit). For AI-native buyers this is the strongest social proof in the category.
- It looks expensive and confident: big light type, a bold colour-band rhythm, and a distinctive tick-mark motif that is clearly "Braintrust".
- The product is shown early and believably: the coded UI mock with realistic data, then one mock per pillar.
- It owns its database story (Brainstore/Nitro) with multiplier numbers. It is the only competitor whose homepage leads a section with a speed benchmark.
- Video testimonials with faces and two quantified case outcomes.
- The pricing meter is explained line by line, with unlimited users.

## Weaknesses we can exploit
- **Closed source and SaaS-first.** Self-hosting exists only as an Enterprise "on-prem" line, with no footprint, no download and no free self-host. "Download one binary, run it free, own your data" is the opposite pitch.
- **The benchmarks are against anonymous "Competitor A/B",** with no method on the page (only a link). We can publish named, reproducible benchmarks with a script anyone can run.
- **Vague hero copy.** "Ship quality agents at scale" says nothing about what the product is or how it is different. The real differentiator (Brainstore speed) is buried 5 screens down.
- **Metered on GB processed + scores + retention, 14-day retention on free.** A self-hosted binary has no meter and retention limited only by disk.

## Steal for cetra
- An early full-bleed band with a coded (HTML, not PNG) product mock carrying realistic data. It stays crisp, theme-able and fast.
- A benchmark block with a big multiplier headline plus raw ms bars and a cold/warm toggle. Ours would name competitors and link the reproduction script.
- A sticky mini-index (WORKFLOW / PLATFORM / SCALE / SECURITY / CUSTOMERS) to orient long scrolls.

## Scores
- Beauty 5/5: bold colour bands, distinctive tick motif, crisp type.
- Selling 4/5: elite logos and testimonials, but a generic headline.
- Clarity (5-second test) 3/5: "ship quality agents" doesn't say what it is.
- Proof 4/5: top logos, videos, benchmarks, but anonymised competitors.
- Product-showing 5/5: realistic coded UI mocks for every pillar.

## Best screenshots
- `caps/braintrust-hero.jpg`: light big-type hero, animated blue tick-field, elite logo wall in the first viewport.
- `caps/braintrust-part01.jpg`: blue band with a realistic Logs/Trace UI mock, then the pink "agents fail differently" band.
- `caps/braintrust-part03.jpg`: Brainstore benchmark bars on green, compliance seals, video-testimonial bento.
