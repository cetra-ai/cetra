# Website research: who sells hardest to developers

Research for the cetra marketing site (`00-design.md` holds the recommendation). Captured on 2026-11-12 (Relax added on 2026-11-13) with headless Chromium at 1440×900, English locale, consent banners hidden without clicking anything. Nothing was signed up for or submitted.

- Per-site notes, one file per site with the same headings: `sites/<slug>.md`. `sites/competitor-summary.md` summarises the category.
- Screenshots (hero, full page, pricing and comparison pages) are in the comparison Artifact, not in the repo. The "Best screenshots" lines in the notes name files from that capture.
- Scores are our judgment on a 1–5 scale. They compare these sites with each other, not with the web at large.

## 1. Shortlist and why each was picked

We looked at 23 sites. 17 make up the main shortlist. The remaining 6 were captured for completeness: Clerk, Neon and Tailscale as extra dev-tool references, and LangWatch, Opik and MLflow as minor competitors.

Selection sources:
- design and devtool curation (Veza Digital's "Best DevTool Website Design Examples", SaaSFrame's developer-tools gallery, SaaS design roundups);
- how often each site is cited as a reference in devtool marketing discussions;
- for the competitors and analogues, their relevance to our pitch.

| Group | Site | Why it is on the list |
|---|---|---|
| Dev-tool bar | **Stripe** | The usual reference for developer marketing: quantified proof everywhere, live micro-demos, audience routing. Veza says it "still sets the bar". |
| | **Linear** | The most-cited "craft is the argument" site. A real product UI in the hero, extreme restraint. |
| | **Vercel** | Self-referential polish: the site's speed is the demo. The "Deploy now" verb CTA. Agent onboarding. |
| | **Supabase** | The sharpest positioning line in the genre ("open-source X alternative"). Uses open source as a weapon, with radically transparent pricing. |
| | **Resend** | Veza calls it "arguably the strongest pure developer landing page": its "Integrate tonight" multi-language code block, plus editorial beauty. |
| | **Raycast** | The strongest brand art motif among the dev-tool sites, and a single unambiguous download CTA with honest requirements. |
| | **PlanetScale** | An anti-marketing mono aesthetic, about 85 linked logos, and the best benchmark page in this set. Named competitors, a fairness section and a reproducible method. |
| | **PostHog** | Personality plus relentless self-serve selling ("97% pay $0", a one-command install wizard) and SEO-scale "vs" pages. |
| OSS single-binary analogues | **Bun** | Same pitch shape as ours: "fast" and "in a single binary". A benchmark race in the hero, and the install one-liner as the CTA. |
| | **PocketBase** | "Open source backend in 1 file": the shortest version of our claim, plus a real UI as hero and a live demo CTA. |
| | **OpenObserve** | Observability, open source, Rust + DataFusion + Parquet, single binary: our stack. Every claim is a multiplier against a named rival. |
| | **Relax** (Makepad) | Added at the user's suggestion. A Rust tool sold entirely on measured speed. It has the best engine-proof and honesty structure in the set, with "Tradeoffs" and "Where LLVM wins" in its main nav. |
| Direct competitors (the bar to beat) | **Langfuse** | Category leader by stars (37.4k). The most proof, and an honest self-host page. |
| | **Braintrust** | The best-looking competitor site, and the only competitor with a speed benchmark on the homepage. |
| | **Laminar** | Rust and open source like us. Tasteful dark design with coded product mocks. |
| | **Arize Phoenix** | Open source and light to run. The only competitor with a one-command run in the hero. |
| | **Helicone** | Simple, clear SaaS template with a real dashboard above the fold. |
| Minor competitors | LangWatch, Opik, MLflow | Checked for patterns: "Self-host in 15 min" CTA, compare callout, and MLflow's public live demo. |

## 2. Comparison table

Scores: **B**eauty, **S**elling, **C**larity (5-second test), **P**roof, product **D**emonstration, each out of 5.

| Site | Hero headline (verbatim) | Primary / secondary CTA | Proof | Product shown how | Style | B | S | C | P | D |
|---|---|---|---|---|---|---|---|---|---|---|
| Stripe | "Financial infrastructure to grow your revenue." | Get started / Sign up with Google | Live "% of global GDP" ticker in the hero; a band of big stats ($1.9T, 99.999%); ARR case studies | Bento of animated micro-demos; dev band with three integration paths | Light, Söhne Light 48px, animated mesh gradient, indigo | 5 | 5 | 3 | 5 | 4 |
| Linear | "The product development system for teams and agents" | (nav only) Sign up | Logo strip, 2 quotes, dated changelog | Real HTML product UI in the hero, bleeding off the fold; one UI per chapter | Dark #08090A, Inter 510 −1.4px, near-monochrome, "FIG" mono labels | 5 | 3 | 4 | 4 | 5 |
| Vercel | "Agentic Infrastructure" | Deploy now / Talk to sales | Top-tier logos, one customer + one big number per section | Almost none (drop-to-deploy hero) | Light, Geist −3.84px, sparse | 4 | 3 | 2 | 4 | 1 |
| Supabase | "Build in a weekend / Scale to millions" | Start your project / Request a demo | 114.9k stars in the nav, a giant star counter, tweet wall, logos | Product bento in the first viewport, tabbed dashboard, code | Light, Manrope + Inter, green second line | 3 | 5 | 5 | 5 | 4 |
| Resend | "Email for developers" | Get started / Documentation | Logo wall, a famous-founder quote | "Integrate tonight" code tabs, live 200 responses, code↔email split | Black, Domaine serif 96px, 3D cube | 5 | 4 | 5 | 4 | 4 |
| Raycast | "Your shortcut to everything." | Download for Mac (+ Homebrew line) | Celebrity quotes, community counts | Real UI with feature switching from section 2 | Black, red light-blade motif, Inter 600 | 5 | 4 | 4 | 3 | 5 |
| PlanetScale | "The world's fastest and most scalable cloud databases" | Get in touch / View sandbox | ~85 linked logos, a quote after every claim, benchmark page | ASCII diagrams, one latency chart | Light, all monospace, 16px H1, orange rule | 3 | 4 | 4 | 5 | 2 |
| PostHog | "Your product's context layer" | Get started / Install with AI (+ `npx` wizard) | 500k teams, "97% pay $0", live dogfooding counts | Scripted HTML demos in windows | Desktop-OS metaphor, hedgehog art, RoundHog | 4 | 5 | 3 | 4 | 4 |
| **Bun** | "Bun is a fast JavaScript runtime & toolkit. All in one." | `curl … \| bash` code block (the CTA) / quickstart | Tabbed benchmark race with method + "reproduce"; logos; video testimonials; comparison table | Benchmark card, autoplay terminal walkthrough | Light, Archivo 800 condensed, one hot-pink accent | 5 | 5 | 5 | 5 | 4 |
| PocketBase | "Open Source backend in 1 file" | Live demo / Read the documentation | None | Real admin UI fills the hero, plus a public live demo | Light, Source Sans, pale blue, mascot | 3 | 3 | 5 | 1 | 5 |
| OpenObserve | "Open-Source, Unified Observability for the AI Era" | Get Started Free / Talk to a Human | Rotating "8× vs Datadog / 140× vs Elastic" pill; to-scale $140 vs $1 bar; 23k stars; metric-chip testimonials | Real UI only in a mid-page tour | Light → dark gradient, Poppins, rainbow helix | 4 | 5 | 3 | 5 | 3 |
| Relax | "Rust without the wait." + a two-line contrast with rustc | GitHub / Try it (`git clone` + `cargo build`) | Stat strip with scenario and hardware (11 s vs ~3 min); published losses (1.12× slower on x64, 1.24× on arm64); per-workload chart with a dated method | Replayable race on one time axis; interactive edit-and-patch demo; 211 code blocks | Dark, system-ui 138px/800, teal→violet gradient word, uniform cards | 3 | 4 | 5 | 5 | 4 |
| Langfuse | "Open Source Agent Evals & Observability" | Start free / Documentation / Onboard with AI | Stat strip above the H1 (21 of Fortune 50, 90B obs/month), 12 logos, live stars + "release: today" | Tabbed real UI and video, below the fold | Stone #EDEDE8, yellow highlighter, docs-like three rails | 4 | 4 | 3 | 5 | 4 |
| Braintrust | "Ship quality agents at scale" | Start building / Contact sales | Elite logo wall in the first viewport, video testimonials, benchmark vs "Competitor A/B" | Coded UI mocks per pillar, starting in the first band | White + bold colour bands, ultramarine, tick-mark motif | 5 | 4 | 3 | 4 | 5 |
| Laminar | "Ship reliable agents" | Get started – free / Setup with agent | 5 small logos, 3.5k stars, "20× cheaper" vs unnamed | Loop diagram hero, coded mocks in every section | Graphite, terracotta accent, numbered 01–06 | 4 | 3 | 4 | 2 | 5 |
| Phoenix | "Open-source AI observability & evaluation" | Self-host / AX Cloud, plus `uvx arize-phoenix serve` | Stars, downloads, famous quotes, logo marquee | Integration constellation; screenshots late | Navy-black grid, Bricolage 200 | 4 | 4 | 5 | 4 | 3 |
| Helicone | "Build Reliable AI Apps" | Try for free | YC, 11.6B requests, "now part of Experiential Labs" banner | Real dashboard screenshot entering the first viewport, `baseURL` code | Light, Inter 600, sky blue, logo-tile cloud | 3 | 4 | 4 | 3 | 4 |
| LangWatch | "Agent Observability & Evals for AI Platform teams" | Book a demo / Self-host in 15 min | Logos, portraits | Animated split mock (fragile) | Warm serif, peach gradient | 4 | 3 | 4 | 3 | 4 |
| Opik | "AI Observability & Evals For the Agentic Era" | Try Opik Free / Get Demo | Big logos | Real screenshots everywhere | Generic Roboto corporate on navy | 2 | 3 | 4 | 3 | 4 |
| MLflow | "Deliver High-Quality AI, Fast" | **Try Demo (live public instance)** / Get Started | Linux Foundation, 28k stars, 30M downloads/month | Live demo; few screenshots | Stripe gradient, DM Sans 300 | 3 | 3 | 3 | 4 | 4 |
| Clerk | "Everything you need for authentication" | "Copy prompt" pill for coding agents | Few logos, tweet wall | Real HTML components as the tabbed hero | Light/dark bands with chamfered edges, Suisse | 5 | 4 | 4 | 3 | 5 |
| Neon | "The backend for apps and agents, built to scale on Lakebase Postgres." | Get started / Read the docs | Logos, scale ladder, architecture diagram | Illustrative mini-UIs | Black cinematic, spectrogram art | 5 | 4 | 3 | 4 | 3 |
| Tailscale | "The best secure connectivity platform for the AI era" | Start connecting devices / Contact sales | 50k businesses, case-stat cards, tweet wall | Terminal + admin UI composite | Warm off-white, brick red, Inter 500 | 4 | 4 | 3 | 5 | 4 |

### Pricing, comparison and docs entry points

| Site | Pricing page | Comparison pages | Docs in nav |
|---|---|---|---|
| Supabase | Four tiers, overage rates shown, compute calculator, spend cap | Long table-driven "Supabase vs Firebase" | Top level |
| PlanetScale | Hardware configurator with exact specs; the catalog can be queried by SQL or agents | A benchmarks page naming eight competitors, with a fairness section and method | Top level |
| PostHog | Free-tier-first ("97% use it for free"), per-product meters | Authored, dated "PostHog vs X" with ✓/✗ tables and "when to choose them" | Desktop icon |
| Resend | Volume slider with explicit overage, plus an "AI agents" pricing link | — | Top level |
| OpenObserve | Per-GB plans, savings slider, 4-way comparison table | Benchmark cards against named vendors | Top level |
| Langfuse | Tiers, calculator, downloadable comparison sheet | Long, dated, worked-dollar "vs LangSmith" with migration section | Top level |
| Braintrust | Meter explained line by line, unlimited users | — | Top level |
| Helicone | Tiers with a self-serving "compare to similar products" ✓/✗ table | Blog listicles; stale facts about rivals | Top level |
| Laminar | Unfinished (no heading, empty band) | — | Top level |

## 3. The best five and why

Ranked by how much each one should shape our site, not by raw score.

1. **Bun: the template for our pitch.** Its hero answers "what" (a fast runtime, one binary), "why switch" (0.21 s vs 4.45 s in a tabbed race) and "how to start" (the `curl | bash` block *is* the CTA) before you read a paragraph. The benchmarks state their hardware and scenario and link a "reproduce" script, which is why developers believe them. Later sections use "replaces X" cards and an autoplaying terminal walkthrough. It is the closest structure to "one binary, fast at volume, install in one line".
2. **Stripe: how to sell with numbers.** Every claim carries a number with a unit and a scope. A live counter opens the page. A dark "numbers band" breaks up the scroll. A bento of small animated product demos replaces feature bullets, and a developer band lets you pick an integration path. This is the level of polish and proof density to aim for, scaled down to one product.
3. **Linear: how to show the product.** A real UI, built in HTML (not a PNG), fills the hero and bleeds off the fold, with believable data doing the thing the headline claims. Each chapter repeats one layout: title, one line, UI composition, sub-feature chips. That makes a long page easy to skim. The craft signals the quality of the tool.
4. **Supabase: how to win an open-source fight.** It names the incumbent in the positioning ("open-source Firebase alternative"). It puts the GitHub stars in the nav, gives "self-host / no lock-in" its own section, and has the most transparent pricing page in the set (overage rates and a calculator), plus long table-driven "vs" pages. This is our model for pricing/self-host and comparisons.
5. **PlanetScale: how to make a speed claim credible.** Not the homepage but its **benchmarks page**. It names eight competitors with QPS and monthly cost side by side, concedes where benchmarks mislead, documents hardware and method, and gives an address for corrections. No competitor in our category has anything like it, and our D1 pitch depends on it.

Runners-up worth borrowing from: **Relax** (the closest content model for our pitch: headline + before/after contrast, stat strip with hardware on every number, a race on one time axis, then "What it costs you" *before* "What it gives you", and a per-workload chart with a dated method; weak on visual identity and length), **OpenObserve** (named-competitor multipliers and the to-scale "$140 vs $1" bar; same stack as ours), **Resend** (the "Integrate tonight" code block with language tabs; editorial beauty), **Clerk** (real components as an interactive tabbed hero; the "Copy prompt" CTA for coding agents), **PocketBase** ("in 1 file" headline; live demo as the primary CTA), **Braintrust** (the best competitor site: coded UI mocks, colour bands, a benchmark section).

## 4. How the competitors pitch, and the gap

Full summary: `sites/competitor-summary.md`. In short:
- **Interchangeable headlines.** "Ship quality agents at scale", "Ship reliable agents", "Build Reliable AI Apps", "Deliver High-Quality AI, Fast". Only Langfuse and Phoenix put "open source" in the H1.
- **Adoption proof, never engine proof.** Logo walls, star counts and volumes. No competitor publishes a named, reproducible benchmark: Braintrust compares against "Competitor A/B" and Laminar against an unnamed "Competition".
- **Nobody shows what self-hosting costs.** Langfuse diagrams six components (Web, Worker, Postgres, Redis, ClickHouse, S3) and says Compose is "not for production". Helicone admits "more complex self-hosting". Laminar, LangWatch and Opik hide their stacks behind one line. No site states the RAM, disk or VM size needed.
- **Open-source stories run through upsell funnels.** Phoenix leads to Arize AX, Opik to Comet. Braintrust is closed. Helicone and Langfuse are now owned by other companies (Helicone by Experiential Labs, Langfuse by ClickHouse).
- **One live demo in the category (MLflow).** One hero run command (Phoenix).
- **Market signal:** Langfuse's own LangSmith comparison page reports that LangSmith moved its cloud to a proprietary Rust + DataFusion engine. The closed leader adopted the architecture we ship as open source on one machine.

The white space: **"the whole platform in one binary, fast at volume, and here is the script that proves it,"** with a live demo and honest comparisons.

## 5. Patterns that sell hard to developers

1. **A headline that states the difference, not the category.** Bun: "fast … all in one". PocketBase: "in 1 file". Supabase: "open-source X alternative". The competitors' interchangeable "ship reliable agents" lines are the counter-example.
2. **Make the try-it action the CTA.** Use a copyable install command with OS tabs and a "view script" link (Bun, Phoenix, PostHog), or a live demo (PocketBase, MLflow). A "Sign up" button comes second, or not at all.
3. **Prove speed with a race, a method and a "reproduce" link.** Tabbed workloads, to-scale bars, named rivals, hardware stated (Bun, PlanetScale, OpenObserve, Relax).
4. **Publish your losses.** Relax puts "Tradeoffs" and "Where LLVM wins" in its nav and lists its costs before its benefits. PlanetScale's benchmark page concedes where benchmarks mislead. Admitting a measured weakness makes every other number believable.
5. **Every number has a unit and a scope.** "$1.9T in 2025", "0.21 s installing a Next.js app", "8× vs Datadog". Vague multipliers against unnamed rivals read as spin.
6. **Real product UI in or right under the hero, built in HTML with believable data.** Linear, Clerk, Braintrust and Laminar do this. Illustrations and abstract art (Vercel, Neon, Raycast's hero) sell mood, not the product.
7. **A code block in the visitor's language, with a visible result** (Resend's "Integrate tonight", Helicone's `baseURL`). It turns interest into a five-minute evening.
8. **"Replaces X" and before/after framing.** Bun's tool cards, Neon's before/after chart, Langfuse's six-box self-host diagram (unintentionally). For us: their six boxes against our one.
9. **Open source shown as live proof.** Star count in the nav, "latest release: today", licence named, no upsell funnel (Supabase, Langfuse).
10. **Transparent pricing with a calculator.** Supabase, Resend, OpenObserve. For a self-hosted binary, the calculator sizes the machine, not the bill.
11. **Honest, dated comparison pages** with a TL;DR, tables by area, a worked example, "when to choose them", and a migration guide (Langfuse vs LangSmith, PostHog vs X, Supabase vs Firebase).
12. **Agent-native onboarding.** A "Copy prompt" or "Install with AI" button and an MCP/SKILL.md (Clerk, PostHog, Vercel, Langfuse, Phoenix). It is now table stakes.
13. **Repeating chapter layout and one signature visual motif.** This keeps a long page skimmable and recognisable (Linear chapters, Braintrust ticks, Raycast blades, Bun's single accent).
14. **Momentum signals:** a dated changelog strip (Linear), "latest release" (Langfuse), live dogfooding counters (PostHog).

## 6. Anti-patterns

- **A category slogan as the H1** ("Agentic Infrastructure", "Ship quality agents at scale"). It only works with existing brand equity, which we don't have.
- **No CTA in the hero** (Linear), **a sales-only CTA** (PlanetScale "Get in touch", LangWatch "Book a demo"), or **too many CTAs** (OpenObserve has six).
- **The key claim hidden in a rotating pill** (OpenObserve) or **buried five screens down** (Braintrust's Brainstore benchmark).
- **Docs-chrome homepages:** three rails, banners, an AI box and 14 sections (Langfuse). The 5-second message drowns.
- **Gimmick metaphors that cost clarity, mobile use and deep links** (PostHog's desktop OS). Memorable, but not something a new product should copy.
- **Scroll-triggered content that renders blank** (Vercel, Laminar's pinned sections, LangWatch's empty hero animation). It also breaks link previews and screenshots.
- **Self-serving ✓/✗ tables and stale facts about rivals** (Helicone). Developers check, and it costs trust.
- **Inconsistent numbers on the same site** (Phoenix 10k vs 12.3k stars, MLflow 20k vs 28k, Langfuse 35k vs 37.4k). Pull them from one source at build time.
- **A banner about the company's future as the first line** (Helicone's acquisition notice).
- **A generic corporate template** (Opik: Roboto, breadcrumbs, a parent company's nav). It reads as "not a developer product".
- **Telling the same story three times** (Relax repeats the speed race in three sections; Bun has three benchmark blocks). Say it once, well, and link to the details.
- **Proof-free minimalism** (PocketBase has zero numbers, logos or quotes). Fine for a hobby project, too little for a platform pitch.
