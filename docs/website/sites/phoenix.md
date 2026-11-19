# Arize Phoenix (https://phoenix.arize.com → https://arize.com/phoenix/)

## Hero
- **Headline (verbatim):** "Open-source AI observability & evaluation" (three lines, under an "Arize Phoenix" logo lockup).
- **Subhead (verbatim):** "Trace, evaluate, and improve AI agents with Phoenix. Run locally or self-host in your environment."
- **CTAs:** the first button is "Self-host" (white outline) → `arize.com/docs/phoenix/self-hosting`. The second is "AX Cloud" (grey rgb(77,77,77) fill, "start free / no credit card required") → the Arize AX signup URL. The filled button is the paid cloud and the outlined one is OSS.
- **Command boxes in the hero:** "GET STARTED LOCALLY: `$ uvx arize-phoenix serve`" with a copy button, then "ADD TRACING USING A CODING AGENT: `$ npx -y @arizeai/phoenix-cli setup`" (npx/install tabs, coding-agent icons, "What does this run?" link). This is the only competitor whose hero lets you run the product with one command.
- **Hero visual:** right half is a dark grid "integration constellation". A large Phoenix bird tile sits in the centre, wired to tiles for Anthropic, Vercel, Agno, Gemini, OpenAI, and capability tiles (Tracing, Evaluation, Prompt IDE, Iteration).
- **Nav:** Arize AX / Phoenix OSS / Docs / Learn / Company / Pricing, a GitHub "Phoenix 12.3k ★" pill, Slack icon, AX sign in / AX sign up, and a "Looking for Phoenix Cloud?" link.

## Structure
1. Arize nav (shared with the paid AX product).
2. Hero: headline, subhead, Self-host / AX Cloud buttons, two copyable commands, integration constellation.
3. "Trusted by Builders": badges (12.3k+ stars, 3M+ downloads), four big stats (3M+ downloads monthly, 10k+ GitHub stars, 7k+ community, 22M+ OTel instrumentation monthly downloads), then a testimonial carousel (Hamel Husain, Jerry Liu of LlamaIndex, ComputerWorld, VentureBeat…).
4. "CUSTOMERS / AI Engineers & Fortune 500 alike": a two-row scrolling logo marquee (Shopee, eBay, Amazon, Moloco, Garena, Atlassian, Taxfix, Microsoft, Apple, MongoDB, Scale, Rippling, Ramp, Booking.com, Bill, ByteDance, Twilio Segment).
5. Alternating feature rows with real dark UI screenshots: "Talk with your traces" (PXI agent), "Get visibility into your agents" (trace tree), "Measure and improve agent quality" (experiments), "Test changes with evidence" (Prompt playground), "Agent Native" (`npx skills add Arize-ai/phoenix`, terminal screenshots).
6. "WHY DO I NEED PHOENIX? A systematic way to improve AI quality": a five-step loop diagram (Observe → Annotate → Hypothesize → Experiment → Measure) with a five-column legend.
7. "OSS Core. Built by Engineers for builders like you.": four "Own your AI Stack" tiles (Privacy, OSS Community, Built on Standards, Vendor Agnostic).
8. "Deploy Anywhere in seconds": Local / Docker / Kubernetes cards, each with "Read the docs", plus an arrow to a white "Arize AX Cloud platform – Start for Free" card.
9. "Get started in Seconds": a learning bento (Agent Mastery, Docs, Evals webinar, Deep Learning Courses, Paper Readings).
10. "Stay in the Loop with Phoenix" newsletter, then footer.

## Proof
- **GitHub stars** in the nav pill (12.3k) and repeated as badges. Note the stat block itself says "10k+ GitHub Stars" while badges say 12.3k: inconsistent.
- **Adoption stats** right after the hero: 3M+ monthly downloads, 22M+ monthly OTel instrumentation downloads, 7k+ community.
- **Testimonials:** eight named people, including well-known voices (Hamel Husain, Jerry Liu) and press quotes (ComputerWorld, VentureBeat). Several are dated (Observe 2023 launch coverage).
- **Logo marquee:** Fortune-500-grade (Microsoft, Amazon, Apple, eBay, Booking.com, ByteDance). These are presumably Arize customers in general, not necessarily Phoenix users; the page doesn't say.
- **Missing:** no benchmarks, no performance or volume numbers, no case-study links.

## Product shown how
- The first visual is an abstract integration diagram. Real product screenshots start about 1.7 viewports down, after stats, testimonials and logos.
- Then five alternating screenshot rows (PXI chat, trace tree, experiments chart, prompt playground, terminal). They are real UI bitmaps in dark theme, slightly soft at this size.
- The product can be run immediately: the hero command `uvx arize-phoenix serve` is the strongest "try it now" device in the category. No hosted demo or video (0 videos).

## Visual style
- **Type:** "Bricolage Grotesque" at 58px, **weight 200** (very thin, elegant display). Body in Source Sans Pro. Monospace in command boxes. Uppercase tracked eyebrow labels ("TRACING", "EVALUATION", "CUSTOMERS").
- **Palette:** deep navy-black (html rgb(18,18,33) ≈ #121221; sections near #000), with a faint grid-line background. White and grey text, cyan-blue links and accents (≈ #2BA4E0, also the "Subscribe" button). Arize magenta/pink appears only on the AX card and logo. Dark mode only.
- **Density:** medium. A big hero, then a fairly conventional SaaS rhythm (stats → testimonials → logos → alternating rows → loop → OSS → deploy → resources).
- **Motion:** testimonial carousel, logo marquee, glowing tiles; the loop diagram is likely animated.
- **Signature devices:** the grid-wired tile constellation; the phoenix bird mark rendered as a glowing tile or particle field; command boxes with copy buttons.

## Pricing / self-host / comparison pages
- **Pricing:** the nav "Pricing" goes to Arize (AX) pricing, which was not captured. The OSS product has no pricing page because it is free; the paid path is the separate AX SaaS.
- **Self-host:** prominent and honest about being free. The hero button is literally "Self-host". The homepage shows Local / Docker / Kubernetes. The self-hosting docs page (`pages/phoenix-selfhost.*`, captured for this report) says "free to self-host with no feature limitations… can be fully air-gapped". It lists Terminal, Docker/Compose, K8s, Helm, AWS CloudFormation, Railway, Render, Cloud Run and Azure, plus Docker image tags. The **infra footprint is not drawn anywhere**; "database connections" appears only as an env-var topic. Phoenix runs on SQLite or Postgres in a Python process (competitors.md), so it is genuinely the lightest competitor. The site does not make that a selling point.
- **Vs pages:** none on the homepage. The docs have "Arize Phoenix or Arize AX" (an internal upsell comparison).
- **Docs entry:** nav "Docs" dropdown, a "Docs" tile in the resources bento, and "Read the docs" on each deploy card. Docs are Mintlify-style, with `/llms.txt` advertised at the top and an "Ask Assistant" box.

## What makes it sell
- "Try it in one command" in the hero (`uvx arize-phoenix serve`), plus a coding-agent setup command. Lowest-friction start in the category.
- "Open-source" and "self-host" are the first words and the first button. The OSS positioning is unambiguous.
- Heavy social proof right after the hero: big download and stars numbers, famous-name testimonials, Fortune-500 logo marquee.
- A clear improvement-loop narrative (observe → annotate → hypothesize → experiment → measure) that teaches a method, not just features.
- An elegant, thin-weight display type on a dark grid that feels premium and technical.

## Weaknesses we can exploit
- **A split identity.** The page is really an Arize AX funnel: AX sign in/up in the nav, the filled CTA is AX Cloud, the deploy section arrows into AX, and pricing is AX pricing. OSS users are visitors in someone else's store. A product whose open-source binary *is* the product can say "no upsell funnel".
- **No performance or scale story at all.** No benchmarks, no "how many spans/s on one box". Python-based, and the site never claims speed. A fast Rust single binary can own "lightweight *and* fast at volume".
- **Inconsistent and dated proof:** 10k+ vs 12.3k stars on the same page, 2023 launch press quotes, and logos that may be Arize-wide rather than Phoenix customers.
- **ELv2 licence** (stated in the OSS tile), not OSI open source. An MIT/Apache single binary can claim "truly open source".

## Steal for cetra
- A copyable run command directly in the hero, with a "What does this run?" disclosure link. Ours: one-line install plus `cetra serve`, then "open localhost:…".
- A "Deploy anywhere" strip (Local / Docker / Kubernetes cards), each with a docs link. Ours can add "a single static binary" as the first card.
- A big-number stats row right after the hero (downloads, stars, OTel downloads). Ours should use engine numbers until we have adoption numbers.

## Scores
- Beauty 4/5: thin display type, wired-grid constellation, polished dark theme.
- Selling 4/5: one-command start, heavy proof, clear OSS stance.
- Clarity (5-second test) 5/5: "Open-source AI observability & evaluation" plus a run command.
- Proof 4/5: stars, downloads, famous quotes, big logos; no benchmarks.
- Product-showing 3/5: real screenshots, but late and soft; the hero is abstract.

## Best screenshots
- `caps/phoenix-hero.jpg`: thin-type OSS headline, Self-host / AX Cloud buttons, `uvx arize-phoenix serve` command, integration constellation.
- `caps/phoenix-part01.jpg`: stats row, testimonial carousel and Fortune-500 logo marquee directly after the hero.
- `caps/phoenix-part03.jpg`: improvement-loop legend, "Own your AI Stack" tiles and the Local/Docker/Kubernetes → AX deploy strip.
