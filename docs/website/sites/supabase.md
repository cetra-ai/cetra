# Supabase (https://supabase.com)

## Hero
- Headline: "Build in a weekend / Scale to millions" (the second line is in brand green).
- Subhead: "Start your project with a Postgres database. Add Authentication, Data APIs, Edge Functions, Realtime Data, Storage, and Vector embeddings."
- Primary CTA: "Start your project" (green, links to supabase.com/dashboard/sign-up). Secondary: "Request a demo" (white, links to /contact/sales).
- The nav shows a GitHub icon with "114.9K" stars, plus Pricing, Docs and Blog as top-level links.
- Visual: there's no single hero image. The product bento starts right under the CTAs, still in the first viewport.

## Structure
1. Hero: two-line headline on the left, subhead on the right, CTAs.
2. Product bento of seven cards (Postgres Database, Authentication, Edge Functions, Storage, Realtime, Vector, Data APIs), each with a small animated illustration (CLI pill "$ supabase functions deploy", email grid, globe, cube). Tagline: "Use one or all. Best of breed products. Integrated as a platform."
3. Two-row logo grid: Lovable, Mozilla, PwC, Figma, v0, Bolt, GitHub, LangChain, 1Password, and others.
4. "Stay productive… without leaving the dashboard": tabbed real dashboard UI (Table Editor / SQL Editor / RLS Policies).
5. "Use Supabase with React": code sample with framework tabs and "Read docs for React".
6. Templates grid (Stripe starter, Next.js, AI Chatbot, LangChain, and others).
7. Customer stories carousel: a big Lovable card, plus eXp, Phoenix Energy, Chatbase "$10M ARR" and Rally.
8. "Join the community": a wall of about 16 real tweets (@shadcn, @patrickc…) and "Join us on Discord".
9. "Open source from day one" with a giant dot-matrix "114.9K" star count and "View on GitHub".
10. Repeat CTA, a compliance strip (SOC2 Type 2, HIPAA, ISO 27001), and a mega footer with "Switch from Firebase / Neon".

## Proof
- GitHub stars in the nav (above the fold) and again as a huge graphic near the bottom.
- Logo grid right after the bento.
- Customer stories with a quantified headline each ("$10M ARR", "zero downtime", "2,000+ employees").
- A wall of tweets.
- Compliance badges above the footer.

## Product shown how
- Illustrated feature cards in the first viewport.
- Real dashboard UI (tabbed) around 1/3 of the way down.
- Copy-pasteable client code with framework tabs.
- 1 video and 4 code blocks. Everything is concrete and developer-facing.

## Visual style
- Type: Manrope at 46px, weight 500, for display; Inter for body text; mono for CLI snippets and pricing numbers.
- Colour: near-white background oklch(0.995) (light theme here; a dark theme toggle exists). The primary button is dark green oklch(0.525 0.12 157.5) ≈ #1F7A4D, and the brand mint #3ECF8E is used for highlights. Grey hairline card borders.
- Light, medium density, card/bento-heavy.
- Signature devices: green emphasis on the second headline line, the dot-matrix star counter, and the tweet wall with a fade at its edges.

## Pricing / comparison pages
- Pricing (pages/supabase-pricing): Free / Pro ($25, "Most Popular", outlined) / Team ($599) / Enterprise. Every limit shows its overage price ("then $0.125 per GB"), and the free-tier pause rule is stated plainly. There's an interactive compute cost estimator ("Add Project", monthly total), an advanced disk table, "Spend Cap On" cost control, a giant comparison matrix with tooltips, and an FAQ. AI tools sit at the top: "Copy as Markdown / Ask ChatGPT / Ask Claude".
- Vs page (pages/supabase-vs, Supabase vs Firebase): a long article-style comparison (36-minute read) with a feature table per area (architecture, auth, functions, storage, open source, pricing, migration). It's balanced in tone. There are also footer links "Switch from Firebase / Neon".
- Docs: a top-nav "Docs" link.

## What makes it sell
- The headline promises both ends of the journey (fast start, and scale) in six words.
- Open source is used as a weapon: stars in the nav, a "never locked in, self-host" section and a giant star count.
- Community proof at volume: real tweets from recognisable developers.
- The pricing page is radically transparent (overage rates, a calculator, a spend cap), which defuses fear of a surprise bill.
- Templates and a code sample turn "interested" into "running in 5 minutes".

## Weaknesses
- The hero has no single strong visual; the bento is busy and generic.
- The page is long and the sections feel generic (templates, tweets), so there's less craft than Linear.
- The cookie banner isn't hidden by the capture CSS and overlaps the screenshots.

## Steal for cetra
- Put the GitHub star count in the nav, and add an "Open source / self-host / no lock-in" section with a big star graphic.
- Make the pricing page transparent: show the overage rates and a resource calculator. For cetra, a "what size box do I need" calculator (spans/day, then RAM/disk) fits the one-binary story.
- Write long, table-driven "cetra vs Langfuse/Phoenix/…" pages by area (ingestion, storage, evals, self-hosting, cost).
- Add copy/ask-an-AI buttons ("Copy as Markdown", "Ask Claude") on docs and pricing.

## Scores
- Beauty 3/5: clean but generic bento-and-cards look.
- Selling 5/5: OSS, community, transparent pricing and templates all push hard.
- Clarity 5/5: headline plus product list are instantly clear.
- Proof 5/5: stars, logos, ARR stories, tweet wall, compliance.
- Product-showing 4/5: real dashboard tabs and code, but not in the hero.

## Best screenshots
- caps/supabase-part01.jpg: hero, product bento, logo grid, tabbed dashboard.
- caps/supabase-part02.jpg: customer story carousel and community tweet wall.
- pages/supabase-pricing-part01.jpg: four tiers with overage prices and a compute calculator.
- pages/supabase-vs-part01.jpg: table-driven Supabase vs Firebase comparison.
