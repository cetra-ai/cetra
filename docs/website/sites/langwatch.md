# LangWatch (https://langwatch.ai)

## Hero
- **Headline (verbatim):** "Agent Observability & Evals for AI Platform teams". "AI Platform teams" is in rust-orange.
- **Subhead (verbatim, short):** "Trace, test, route and govern every LLM call in your company…"
- **Pills:** LLM OPS / CODING AGENTS / AI GATEWAY / AI GOVERNANCE.
- **Primary CTA:** "Book a demo" (near-black #141417 pill) → `/get-a-demo`.
- **Secondary CTA:** "Self-host in 15 min" (outlined) → `/docs/self-hosting/overview`.
- **Hero visual:** a tabbed, animated split mock: a Claude Code terminal on the left and a "simulation — qualified senior candidate" chat on the right, with tabs Chat/Code Assistant and Simulations/Experiments/Traces. In the capture it is mid-animation and mostly empty ("waiting for the assistant…"). The background is a warm peach gradient.

## Structure
1. Nav (Platform, Enterprise, Customers, Blog, **Benchmarks**, Docs, Pricing).
2. Hero with animated product mock.
3. "TRUSTED IN PRODUCTION BY" logos (Backbase, PagBank, Visma, Deloitte, Altura, vinny, Freeday).
4. Problem statement ("AI agents are still tested by hand…") and a Specs → Simulations → Agent Improvement loop.
5. Simulate, Evaluate and Observe: three card carousels of coded mini-UIs.
6. "Every LLM call has a way in": SDK/OTel, `npx langwatch claude` for coding agents, gateway ("11 microseconds of overhead").
7. Dark band "Our AI tests your AI" (Langy agent: PM goal → PR, "median PM-to-PR 14 minutes").
8. "Where it runs. Who controls it. What certifies it.": self-hosted/hybrid/cloud, security controls, ISO 27001 and GDPR.
9. Photo testimonial bento ("Moved from Langfuse to LangWatch…").
10. "Ship agents with confidence." → "Talk to an engineer" / "or start free in 5 minutes". The footer has "Switch from Langfuse/Braintrust/LangSmith/Arize/Humanloop" links and a "Feature comparison" page.

## Proof
Seven enterprise and fintech logos under the hero (Deloitte, Visma, PagBank). A photo-portrait testimonial bento near the end, including a Langfuse switcher. ISO 27001 and GDPR. Numbers are micro-claims (11 µs gateway overhead, 14-minute PM-to-PR) with no methodology on the page. A Benchmarks nav item exists (not captured). No GitHub star count is shown, only an icon; the footer says "Open Source · Apache2".

## Product shown how
Coded, animated mocks start in the hero and continue through three carousels (simulation audio chat, Claude Code scenario writing, CI run list, eval table, production monitors, trace list, ⌘K palette, cost per coding agent). Rich but small. No video and no live demo.

## Visual style
Serif display "Sentient" (52px, weight 400, -1.56px tracking) over Inter. Off-white background with warm peach/orange gradient blooms and a rust-orange accent (≈ #D2561C), near-black #141417 buttons, and a navy/dark band mid-page. Editorial and warm, distinct from the dev-tool norm. Italic serif emphasis ("runs", "controls", "certifies", "mission-critical AI").

## Pricing / self-host / comparison pages
Pricing was not captured. Self-host is the hero's secondary CTA ("in 15 min"), and the homepage lists Docker, Kubernetes/Helm, VPC and a hybrid data plane. No infra footprint is shown; competitors.md lists Postgres + ClickHouse + Redis + S3 plus Go/Python services. "Switch from X" migration pages exist for five competitors.

## What makes it sell
- Sharp audience targeting ("for AI Platform teams") and a broad "every LLM call in your company" scope, including coding-agent spend.
- An owned differentiator: agent simulations and red-teaming, not just tracing.
- A warm editorial serif look that stands out in a sea of dark dev sites.
- Enterprise readiness (hybrid, SCIM, SIEM, ISO) plus "Switch from…" pages.

## Weaknesses we can exploit
- **"Self-host in 15 min"** is a claim, not a visible artifact. No command, no footprint. "Running in 15 seconds, one binary" beats it with a demo.
- **The primary CTA is "Book a demo",** which is sales-led. Developers who want to try it right away hit friction.
- **The hero animation can render empty;** busy carousels dilute the message.

## Steal for cetra
- A "Switch from Langfuse / Phoenix / …" link row in the footer, with migration guides behind it.
- The self-host CTA sitting next to the primary CTA in the hero, with a time claim, but backed by a visible command.

## Scores
- Beauty 4/5: warm serif editorial look, distinctive in the category.
- Selling 3/5: strong targeting, but demo-first and busy.
- Clarity (5-second test) 4/5: headline names the product and the buyer.
- Proof 3/5: decent logos and portraits, unsourced micro-numbers.
- Product-showing 4/5: coded mocks everywhere, hero animation fragile.

## Best screenshots
- `caps/langwatch-hero.jpg`: serif headline, capability pills, Book a demo / Self-host in 15 min, split terminal and simulation mock. A cookie banner is visible because the capture's hiding failed.
- `caps/langwatch-part02.jpg`: Evaluate/Observe card carousels and the "Every LLM call has a way in" integration rows.
- `caps/langwatch-part03.jpg`: "Where it runs / controls / certifies" deployment band and photo testimonial bento.
