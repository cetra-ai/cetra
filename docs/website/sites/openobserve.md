# OpenObserve (https://openobserve.ai)

## Hero
- **Headline:** "Open-Source, Unified Observability for the AI Era". "Unified Observability" is set in a rainbow gradient (purple → blue → green → yellow).
- **Subhead:** none. A rotating pill badge above the headline cycles through claims: "8x more cost efficient vs Datadog", "5-15x faster vs Grafana stack" and "140x more storage efficient vs Elastic" (all three appear across the captures).
- **Primary CTA:** "Get Started Free →" (purple #5527CA family, a button with no href, i.e. a JS action).
- **Secondary CTA:** "Talk to a Human" (outlined, goes to /demo/).
- **Nav CTAs:** ⌘K search, a GitHub "23k" star count, "Download" (/downloads/), "Get Demo" and "Log In". A purple webinar banner sits on top.
- **Hero visual:** an animated canvas (1 canvas). A colourful double-helix ribbon of L/M/T (logs, metrics, traces) chips flows from an "Observability layers" card (Frontend, AI & LLM, API, Application, Database, Network, Infrastructure). It runs through an "Autocorrelation Engine — ANALYZING 1.29M signals/s · 90K pairs correlated" pill into an "AI SRE agent — INVESTIGATING" card with a service graph and impact chips.

## Structure
1. Webinar bar, then the nav with stars and Download.
2. Hero: rotating claim pill, gradient headline, two CTAs, the animated helix with UI cards.
3. Customer logo marquee (Foxconn, Siedle, Banorte, imo banco…).
4. "Four reasons teams switch to OpenObserve": a tabbed list (efficiency, unified, open source at scale, no lock-in). The efficiency panel shows "5 PB/day ingested by a single customer" and a to-scale bar chart ("Elasticsearch $140 vs OpenObserve $1 ← that 4-pixel sliver is the entire bar"). Below it, a black strip: "vs Grafana 5-15× faster / vs Datadog 8× cheaper / vs Elastic 140× storage", plus "Calculate your savings".
5. "One stack, every signal": an interactive isometric architecture illustration ("Click a layer of the stack to open it").
6. "Every signal, one platform": a product tour with 12 tabs (Logs, Metrics, Traces, Dashboard, Alerts, AI SRE, LLM Observability, RUM…) and a real UI screenshot per tab.
7. "Measured against industry leaders": three benchmark cards with "Read case study" links, "Calculate your savings" and "See all comparisons".
8. "One platform, every workload": tabs (Kubernetes, AWS, GCP, Azure, AI/LLM…), each with a curl install snippet, a "~3 min setup" tag and a mini timeline.
9. "Plug into your stack": a two-row integration icon marquee.
10. Testimonial carousel: each quote has three metric chips ("< 1 hour, Migrated off Datadog", "4x lower cost").
11. SEO resource cards (Datadog Pricing, Prometheus Alternatives…), then an FAQ accordion.
12. Dark closing CTA ("Ready to get started?") and a big footer with a "Compare" column (vs Datadog, Splunk, New Relic, Grafana, Dynatrace, Dash0), SOC2 and the star count.

## Proof
- Named-competitor multipliers are everywhere: hero pill, section 4 strip and section 7 cards. The rule is stated outright: "Every claim is measured against a named product – not an industry average."
- Scale: "5 PB/day by a single customer", "+10K enterprises", "23k stars" (shown in the nav and the footer).
- An enterprise logo marquee under the hero, testimonials with quantified chips, case-study links, and SOC2 Type II.

## Product shown how
- The hero is an abstract animation with stylised UI cards, not the real product.
- Real UI screenshots appear only in the tabbed product tour, about 2,000px down.
- Install snippets per platform (the Kubernetes curl one-liner).
- Interactive pieces: the stack explorer, workload tabs and testimonials carousel.

## Visual style
- **Type:** Poppins 700 for headings (h1 36px, -0.9px tracking, modest size), Inter for body.
- **Colours:** white, then a vertical gradient that moves from pale blue into cyan (#2BA0D0-ish), periwinkle and purple, ending in deep navy-purple (#1F0B49, sampled) with an orange glow in the closing CTA. Accent purple #5527CA (CTA bg rgb(85,39,202)). The rainbow gradient appears on headline keywords.
- **Mode and density:** light at the top, dark at the bottom, medium-high density, 433 images. Motion comes from the canvas, marquees and carousel.
- **Signature devices:** pill eyebrows with green dots ("Why OpenObserve", "Benchmarks"), the to-scale bar with its "4-pixel sliver" annotation, metric chips inside testimonials, the isometric stack illustration.

## Pricing / comparison pages
- **Pricing (pages/openobserve-pricing):**
  - Headline "Finally, an observability platform with transparent pricing". The pill says "No per-host, per-seat or per-user fees". A logo row follows.
  - Cloud / Self Hosted toggle.
  - Two plans: Cloud Professional at "$0.50/GB ingested + $0.01/GB queried" with retention spelled out, and Enterprise "Custom" with a long ✓ list.
  - **The best piece:** "Estimate your monthly cost", a slider ("1 GB/day: Datadog est. $140/mo vs OpenObserve $15/mo, You save 89%") with a "Detailed Bill Analyzer".
  - A 4-way comparison table (OpenObserve / Datadog / Splunk / Dash0) on pricing model, per-seat fees and "Open source core: Yes, AGPL-3.0", with links to vs pages.
  - Then testimonials and 13 pricing FAQs.
- **Comparisons:** a dedicated "Compare" footer column and a "Datadog Bill Analyzer" tool.
- **Docs:** "Docs" in the top nav and ⌘K search.

## What makes it sell
- Every number names the competitor it beats, which makes the claims feel testable.
- The to-scale bar ($140 vs $1, "that 4-pixel sliver is the entire bar") makes 140× visceral.
- A savings slider on the pricing page turns the cost advantage into your own number.
- Testimonials carry hard metrics, including migration time ("< 1 hour").
- The GitHub star count and the Download button in the nav signal open source and self-hostability.
- Its architecture copy ("Columnar Parquet storage, written in Rust on the DataFusion engine. There is no index to build…") is exactly our stack, and it turns engineering into a cost argument.

## Weaknesses
- The hero is generic-AI busy: the rainbow helix and "for the AI Era" say little. The best claims are hidden in a small rotating pill.
- Too many CTAs (Download, Get Demo, Log In, Get Started, Talk to a Human, webinar), so focus is diluted.
- The real product appears late. The ending (SEO cards and FAQ) feels like content marketing.

## Steal for cetra
- The "measured against a named product" rule. Show a black strip of three multipliers against Langfuse, Phoenix and Opik, each linking to its methodology.
- A to-scale comparison bar with a wry annotation (e.g. RAM or containers needed: theirs against our one binary).
- A pricing-page slider: spans/day mapped to "their cloud bill vs cetra on one VM".
- Testimonial cards with 3 metric chips each, plus a GitHub star count and Download in the nav.

## Scores
- Beauty 4/5 — polished gradients and illustrations; hero is over-decorated.
- Selling 5/5 — quantified competitor claims, savings calculator, migration proof.
- Clarity 3/5 — vague headline; key numbers sit in a rotating pill.
- Proof 5/5 — stars, PB/day, logos, metric-chip testimonials, SOC2.
- Product-showing 3/5 — real UI only in a mid-page tabbed tour.

## Best screenshots
- caps/openobserve-hero.jpg — rotating competitor-claim pill, gradient headline, animated correlation-engine hero.
- caps/openobserve-part01.jpg — "Four reasons" panel with the to-scale $140 vs $1 bar and the multipliers strip.
- caps/openobserve-part02.jpg — product tour with a real UI, benchmark cards against named vendors, install-snippet workload tabs.
- pages/openobserve-pricing-part01.jpg — per-GB plans, savings slider and 4-way comparison table.
