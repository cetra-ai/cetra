# Helicone (https://www.helicone.ai)

## Hero
- **Badges above the headline:** "Backed by Y Combinator" and a "Product of the day 1st" laurel.
- **Headline (verbatim):** "Build Reliable AI Apps". "AI Apps" is set in brand sky-blue.
- **Subhead (verbatim):** "The world's fastest-growing AI companies rely on Helicone to route, debug, and analyze their applications."
- **Primary CTA:** "Try for free >", a sky-blue rgb(12,165,234) / #0CA5EA button → `us.helicone.ai/signup`, with "No credit card required, 7-day free trial" underneath.
- **Secondary CTA:** none in the hero. "Contact us" (grey) and "Log In" (blue) sit in the nav, next to a GitHub 6.3K counter.
- **Hero visual:** right side is a cloud of tilted, softly shadowed provider-logo tiles (OpenAI, Gemini, DeepSeek, Mistral, Groq, Anthropic, Together…), signalling "gateway to every model". Directly below, entering the first viewport, is a large, clean light-theme **dashboard screenshot** ("Xpedia AI": Requests 3,310,278, Errors donut, Top Models, Costs $93,128.22, Top Countries, Latency 6.058 s/req).
- **Top banner:** "Helicone is now part of Experiential Labs. Details to come soon." An acquisition notice with no details, the first thing a visitor reads.

## Structure
1. Acquisition banner and nav (Docs, Pricing, Models, Stats, Resources, Tools, GitHub 6.3K, Contact us, Log In).
2. Hero: YC and Product-of-the-day badges, headline, subhead, one CTA, logo-tile cloud.
3. Large dashboard screenshot in a rounded frame.
4. "1000+ AI teams use Helicone to build reliable products": one row of logos (together.ai, QA Wolf, Clay, Singapore Airlines, Duolingo, Sunrun, Filevine, PodPitch).
5. "One SDK for 100+ Models": model tabs (GPT-4o, Claude, Gemini, DeepSeek) and a dark code block (TypeScript/Python/cURL tabs) showing an OpenAI client with `baseURL: "https://ai-gateway.helicone.ai"`, then CTA and "View all 100+ models".
6. Big pull-quote: "The most impactful one-line change I've seen applied to our codebase." (Nishant Shukla, QA Wolf).
7. "Become an AI Engineer in 7 days": email-course signup.
8. 01 Route, "Smart & Speedy LLM Routing": isometric illustration of provider blocks wired into a Helicone cube, plus a "What is LLM routing?" accordion.
9. 02 Debug, "Trace and debug your agent with ease": trace/Gantt screenshot.
10. 03 Monitor, "Get complete visibility into your AI apps": the same dashboard, tilted in 3D perspective.
11. Big-number sentence: "Today, 11.6 billion requests processed, 3.2 Trillion tokens a month, 84.5 million users tracked", with numbers in blue and one in a hand-drawn-style box.
12. "Proudly open-source": community card (contributor avatars), "Deploy your own AI Gateway" (Helm, AWS/GCP/Azure cube), API Cost Calculator card.
13. FAQ (6 questions: latency, differentiation, integration time, compliance, self-hosting, "what if I don't want Helicone in my critical path?").
14. Second pull-quote (Soohoon Choi, CTO, Greptile).
15. Closing CTA ("Reliable AI applications starting today", "Try Helicone for free") with the logo tiles again, SOC2/HIPAA badges, footer (which links blog posts such as "Helicone vs. LangSmith").

## Proof
- **Hero:** YC badge, Product Hunt #1 laurel, and the "fastest-growing AI companies" claim.
- **Below the dashboard:** "1000+ AI teams" and 8 logos (mid-market and startup, plus Singapore Airlines and Duolingo).
- **Mid-page:** the big usage sentence (11.6B requests, 3.2T tokens/month, 84.5M users tracked) and two pull quotes with photo, title and company logo.
- **GitHub:** 6.3K stars in the nav and the OSS section. SOC2 and HIPAA badges at the bottom.
- **Missing:** no latency benchmark on the homepage, even though "Will Helicone add latency?" is the first FAQ (the answer is collapsed). No case studies.

## Product shown how
- A **real dashboard screenshot is in the first viewport**: big, light-theme, believable numbers. It is the clearest above-the-fold product shot among the competitors.
- A code sample appears at about 1.5 viewports: the gateway one-liner (change `baseURL`) with language tabs. It sells the "one-line integration" mechanism directly.
- Then a trace screenshot (Debug) and the dashboard again in 3D tilt (Monitor). Routing uses an isometric illustration.
- No video, no live demo (0 videos).

## Visual style
- **Type:** Inter, 84px, weight 600, normal tracking. A big bold geometric sans; two-tone headings (black plus sky-blue phrase). System UI sans for body.
- **Palette:** white, with pale-blue gradient washes and faint contour/wave line patterns. Sky blue #0CA5EA is the only accent (buttons, highlights, logo). Grey-blue body text, dark navy code block. Light only.
- **Density:** medium-low. Classic SaaS template rhythm with generous spacing; feels friendly and approachable rather than technical-premium.
- **Motion:** little visible; floating logo tiles may animate.
- **Signature devices:** floating tilted provider-logo tiles with soft shadows; the hand-drawn-style outline box around a key word ("11.6 billion", "Reliable"); numbered sections "01 Route / 02 Debug / 03 Monitor".

## Pricing / self-host / comparison pages
- **Pricing (`/pricing`):** "Simple, predictable pricing". Four tiers: Hobby (free, 10k requests, 1 GB, 1 seat), Pro ($79/month, "POPULAR"), Team ($799/month, "BEST VALUE"), Enterprise (contact; "On-prem deployment", SAML SSO). Then a calculator (requests/month × tokens/request → storage GB → $), a long plan matrix (retention 7 days / 1 month / 3 months / forever; ingestion 10 to 30,000 logs/min), discounts (startups 50% off the first year, OSS $100 credit, students free), and a **"Compare to similar products"** checkmark table against LangSmith, Braintrust, Arize AI and Langfuse. Helicone is the only column with all green ticks; others get ✗ on "Built for scale" and "1 line integration", and the UI row reads "Confusing / Average / Intuitive". Self-serving and not credible.
- **Self-host:** the homepage only offers "Deploy your own AI Gateway" (single-click or Helm), and the FAQ asks "Can I deploy Helicone in my own infrastructure?" (collapsed). The pricing page lists on-prem only in Enterprise. **The infra footprint is admitted on the vs page:** "Distributed (Cloudflare Workers, ClickHouse, Kafka)" and, as Helicone's own drawback, "More complex self-hosting setup due to distributed architecture". competitors.md adds Postgres, S3 and Redis.
- **Vs pages:** blog-style comparisons (`/blog/best-langfuse-alternatives`, dated August 25, 2024, by Cole Gottdank). It contains **outdated claims**: Langfuse on "a single PostgreSQL database", "$59/month", "no free trial", whereas Langfuse's own site now shows ClickHouse plus a $29 Core tier and a free Hobby tier. The footer also links "Helicone vs. LangSmith".
- **Docs entry:** "Docs" is the first nav item. No docs CTA in the hero.

## What makes it sell
- Instant comprehension: big headline, one CTA, and a real dashboard in the first viewport, so you see what you get.
- Its mechanism is concrete and cheap to try: "change `baseURL`" code block plus a "most impactful one-line change" testimonial that repeats the point.
- Big usage numbers, written as a sentence ("11.6 billion requests processed…"), are memorable.
- Friendly, low-anxiety aesthetic and a 7-day trial with no card.
- The FAQ answers proxy-specific fears (latency, critical path) up front.

## Weaknesses we can exploit
- **Uncertain future:** the first line on the page is "now part of Experiential Labs. Details to come soon.", and the footer still reads © 2025. An independent, actively shipped project contrasts well.
- **The self-host stack is admittedly complex** (Cloudflare Workers, ClickHouse, Kafka, plus Postgres/S3/Redis), and the homepage only offers the gateway for self-hosting. "The whole platform, one binary" beats "self-host our gateway via Helm".
- **Comparison content is self-serving and stale** (all-✗ competitor columns, "Confusing" UI rows, a 2024 blog with wrong facts about Langfuse). Honest, dated, sourced comparisons would stand out.
- **Proxy-in-the-critical-path is their core design;** the latency question is asked but no number is shown. OTLP ingestion out of the request path, with a published overhead number, is a cleaner claim.

## Steal for cetra
- A real product screenshot entering the first viewport directly under a short headline and a single CTA.
- A "the integration is literally this" code block with language tabs right after the logos. Ours would show the OTLP endpoint / SDK init.
- A FAQ that answers the scary operational questions up front (latency, critical path, self-hosting), but with numbers.

## Scores
- Beauty 3/5: clean but generic SaaS template, logo tiles a bit cliché.
- Selling 4/5: one CTA, real dashboard, concrete one-line integration.
- Clarity (5-second test) 4/5: gateway and observability are clear from screenshot and code.
- Proof 3/5: big usage numbers, YC, mid-tier logos; acquisition banner undermines trust.
- Product-showing 4/5: real dashboard above the fold, code early.

## Best screenshots
- `caps/helicone-hero.jpg`: bold two-tone headline, logo-tile cloud, real dashboard entering the first viewport.
- `caps/helicone-part01.jpg`: dashboard, logo row, "One SDK for 100+ Models" `baseURL` code block, one-line-change quote.
- `pages/helicone-pricing-part02.jpg`: self-serving "Compare to similar products" checkmark table and discounts grid.
