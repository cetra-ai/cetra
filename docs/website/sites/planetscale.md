# PlanetScale (https://planetscale.com)

## Hero
- Headline: "The world's fastest and most scalable cloud databases". It's a 16px bold monospace H1 with an orange left rule; the page has no big display type.
- Subhead (short excerpt): "PlanetScale brings you the fastest databases available in the cloud, built for exceptional speed and reliability." It's followed by a second paragraph on NVMe and "unlimited IOPS", with inline blue links.
- Primary CTA: "Get in touch" (solid orange #F35815, links to /contact). Secondary: "View sandbox" (orange-outlined button, no href, so it opens an in-page sandbox). There's also "Sign in".
- A yellow announcement bar sits above the nav ("Neki, sharded Postgres, is now available. Learn more").
- Visual: no hero image. The first viewport is text, then "PlanetScale powers Tier 0 databases at:" followed by a giant bordered logo grid (Square, Cash App, Blizzard, Cursor, Fin, Etsy, Slack, Notion, Cloudflare…).

## Structure
1. Announcement bar, then a pipe-separated text nav (Platform | Resources | Blog | Documentation | Pricing | Migrate).
2. Mono H1 and two paragraphs of plain-text positioning.
3. Logo grid: 5 columns × about 17 rows, roughly 85 customer logos, each linked to a case study or blog post.
4. Quote from Cursor's CPO ("millions of queries per second on petabytes of data").
5. Tabbed product explainer (Vitess / Neki / Postgres) with an ASCII-art architecture diagram (VTGate → Primary → Replicas), plus an Intercom quote.
6. "Performance": a real Insights latency graph showing p50/p95/p99 dropping after moving to Metal, a link to the Benchmarks page, and a Cash App quote.
7. "Uptime": the 99.999%/99.99% SLA, a list of online operations, a status-page link, and a Convex CEO quote.
8. "Cost": "less expensive than RDS/Aurora for ~85% of workloads", "No long sales process or annoying pitches", BYOC and marketplaces.
9. "Security": SOC, PCI and HIPAA bullets, Trust Center, and a Cash App quote.
10. "Features": long bulleted lists, shared and per engine (including "$5 single node" Postgres and an MCP server), plus a MyFitnessPal quote.
11. Dashed-border ASCII-style footer.

## Proof
Proof is close to the whole page:
- About 85 logos in the first scroll.
- Six named quotes interleaved after each claim section (Cursor, Intercom, Cash App ×2, Convex, MyFitnessPal).
- A real before/after latency chart.
- SLA numbers.
- "~85% of workloads cheaper" claim.
- A dedicated Benchmarks page with competitor names.

## Product shown how
- One real product screenshot (Query Insights latency chart) about two screens down.
- ASCII architecture diagrams (rendered in pre/code: 4 code blocks).
- "View sandbox" in the hero for hands-on use.
- Otherwise text-first: 1 image tag, 0 canvases and 0 videos.

## Visual style
- Type: everything is in system monospace (ui-monospace / SF Mono / Menlo), including the H1 at 16px, weight 700. It reads like a README or man page.
- Colour: off-white #FAFAFA, black text, link blue (about #0B6BCB), orange accent #F35815 (rgb(243,88,21)) for CTAs and the H1 rule, and a yellow #FFCC00-ish announcement bar.
- Light, dense, document-like, with dotted rule separators and box-drawing borders.
- No motion at all.
- Signature devices: ASCII diagrams, bordered logo table, dashed footer box, pipe-separated nav. It's a deliberate anti-marketing look that signals "engineers built this".

## Pricing / comparison pages
- Pricing (pages/planetscale-pricing): a configurator rather than tiers.
  - Pick region → engine (Vitess / Neki / Postgres) → HA or single node → EBS or Metal → instance (PS-5: 1/16 vCPU, 512MB, $15) → storage and egress sliders.
  - Shows a live "Estimated monthly total" and a node breakdown table.
  - The top banner says pricing can be queried with SQL ("Connect to pricing database") and links an agent-friendly pricing.md.
- Benchmarks (pages/planetscale-benchmarks): a head-to-head table naming 8 competitors (AlloyDB, Aurora, Crunchy, Heroku, Neon, Supabase, TigerData, and Xata "(declined access)"), showing their QPS, "Up to N× faster", their cost and "PlanetScale saves $X (Y%)". It's followed by honest sections: "What is benchmarking?" (admits benchmarks are often deceptive), Fairness, Compute (gave rivals double the CPU), Storage, and a reproducible Methodology, with an email address for corrections.
- Docs: "Documentation" is a top-level nav item. "Migrate" is also top-level.

## What makes it sell
- Credibility through restraint: the mono "no marketing" aesthetic plus the line "you can probably tell from our website that the tech speaks for itself" make the claims feel engineered, not spun.
- The logo grid is overwhelming in volume (about 85, all linked), so it doubles as a case-study index.
- Every claim section ends with a named customer quote that backs it.
- The benchmarks page names competitors, publishes cost and QPS side by side, concedes advantages and gives a reproducible method. It's the gold standard for "faster/cheaper" claims.
- The pricing configurator has exact hardware specs and an SQL/agent-queryable catalog.

## Weaknesses
- The hero has zero visual impact, and the 16px H1 fails at grabbing attention.
- The primary CTA is sales ("Get in touch"), with no self-serve "Start free" button in the hero.
- It's a wall of text, so the product is barely shown.

## Steal for cetra
- Publish a Benchmarks page modelled on theirs: a table of cetra vs Langfuse/Phoenix/Opik on the same box (ingest spans/s, query p95, RAM, disk, monthly infra cost), with a fairness section, reproducible scripts and a corrections email.
- Use ASCII or box-drawing architecture diagrams (OTLP → WAL → memtable → Parquet → DataFusion), which suit a Rust single-binary story.
- Put a quote after each claim (performance, reliability, cost), each one backing that claim.
- Offer a sizing configurator ("N spans/day → this VM, $X/month on Hetzner/AWS") plus a machine-readable pricing/sizing .md for agents.

## Scores
- Beauty 3/5: distinctive, principled mono look, but austere.
- Selling 4/5: claims backed by logos, quotes and benchmarks, but a sales-led CTA.
- Clarity 4/5: the headline says it all, though it's tiny.
- Proof 5/5: about 85 logos, six quotes, named-competitor benchmarks.
- Product-showing 2/5: one chart, ASCII diagrams and a sandbox button.

## Best screenshots
- caps/planetscale-hero.jpg: mono H1, plain-text pitch, bordered logo grid.
- caps/planetscale-part02.jpg: ASCII architecture, real latency chart, claim-plus-quote sections.
- pages/planetscale-benchmarks-part01.jpg: named-competitor benchmark table with cost savings and a fairness section.
- pages/planetscale-pricing-part01.jpg: hardware configurator pricing with SQL/agent-queryable catalog.
