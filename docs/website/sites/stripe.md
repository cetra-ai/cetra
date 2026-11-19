# Stripe (https://stripe.com)

## Hero
- Headline (the H1 is one long sentence): "Financial infrastructure to grow your revenue." The rest of the H1 works as the subhead, in a lighter blue-grey: "Accept payments, handle business banking, and implement custom revenue models—from your first transaction to your billionth."
- Above the headline is a live ticker: "Global GDP running on Stripe: 1.74853613%" (the digits roll).
- Primary CTA: "Get started" (indigo #533AFD, links to dashboard.stripe.com/register). Secondary: "Sign up with Google" (outlined, OAuth one-click). The nav has "Sign in" and "Contact sales".
- Above the nav: a dark announcement bar ("OpenRouter is now part of Stripe…").
- Visual: the famous animated mesh gradient (orange/pink/violet/blue ribbons) sweeping in from the top-right. There are 13 canvases on the page.
- A logo strip (Amazon, NVIDIA, Ford, Coinbase, Google…) sits at the bottom of the first viewport.

## Structure
1. Announcement bar, nav, hero with GDP ticker, mesh gradient and logo strip.
2. "Flexible solutions for every business model.": a bento of six product cards, each a live, cycling UI mini-demo:
   - Payments: terminal plus checkout, cycling between USD/EUR/JPY.
   - Billing: usage meter of 1.5B tokens.
   - Agentic commerce: chat that leads to a buy button.
   - Card issuing: a card render.
   - Stablecoins: a dotted globe.
   - Connect: a platform dashboard.
3. "Stripe Sessions" keynote video banner ("Building the economic infrastructure for AI").
4. "The backbone of global commerce": a dark indigo band with four big numbers (135+ currencies, $1.9T volume in 2025, 99.999% uptime, 200M+ subscriptions) over a particle burst.
5. "Powering businesses of all sizes" in three audience blocks:
   - Enterprise: a Hertz story with stats, plus an accordion of URBN, Instacart and Le Monde.
   - Startups: a carousel of Lovable, Gamma "$100M ARR", Runway and Supabase, plus Startups program and Atlas.
   - Platforms: a dashboard mock and a Mindbody quote.
6. Developer band (dark navy): "View developer docs / View Stripe's GitHub", an integration diagram, scale stats (500M+ API requests/day, 10K+ req/s, 150K+ tx/min), and three integration paths (no-code, pre-integrated platforms, code with a terminal snippet).
7. "What's happening": news carousel (Annual letter: $1.9T).
8. "Book of the week" (Stripe Press), a brand-culture flourish.
9. "Ready to get started?" CTA with pricing and integration links, then the footer.

## Proof
Proof is dense and spread through the whole page:
- Live GDP ticker and logo strip, both above the fold.
- Four headline numbers in the middle of the page.
- Named enterprise and startup stories with numbers ($5B, $100M ARR, 160 countries).
- API scale stats in the developer band.
- One named quote.
- "50% of Fortune 100" and "88% of the Forbes AI 50".

## Product shown how
- Product UI mini-demos start right after the hero (second section), animated and localised; they look like real checkout and dashboard UI.
- Code appears only in the developer band (one terminal snippet).
- There's also a keynote video.
- Illustration is reserved for gradients and particle effects.

## Visual style
- Type: Söhne (sohne-var) at 48px, weight 300 (light), letter-spacing −0.96px. Uses the two-tone headline device (dark navy lead, lighter slate continuation).
- Colour:
  - White background.
  - Navy text #0A2540 (rgb(10,37,64), also the chat button).
  - Indigo CTA #533AFD (rgb(83,58,253)).
  - Slate-blue secondary text.
  - Mesh gradient of orange #FF9A3C, pink and violet.
  - Dark navy and indigo bands for the stats and developer sections.
- Light with dark interludes. Dense but orderly, with thin vertical guide lines framing the content column.
- Signature devices: the mesh gradient, the live counter, product micro-demos in a bento, faint grid guide lines, and the two-tone sentence-as-headline.

## Pricing / comparison pages
- Not captured. "Pricing" is in the top nav. The closing block summarises pricing as "Integrated per-transaction pricing with no hidden fees" and the integration time as "as little as 10 minutes".
- Docs: "Developers" dropdown in the nav, plus a dedicated "View developer docs" button in the dark developer band.
- Capture note: the cookie, locale and sales-chat popups were not hidden and cover the right side of the hero.

## What makes it sell
- The live "% of global GDP" ticker in the first line is audacious, specific and moving.
- Numbers everywhere, each with a unit and a scope ($1.9T in 2025, 99.999% uptime, 10K+ req/s), so every claim is checkable.
- Audience routing (enterprise / startup / platform), each block with its own CTA and its own customer story.
- A developer band with integration-path choice: no-code, platform, or code.
- Two paths to start: email, or one-click Google sign-up.

## Weaknesses
- The H1 is a 30-word sentence, which is heavy for a 5-second scan.
- The page is enormous (about 15,000px) and covers so much that a developer has to scroll far to reach the code.

## Steal for cetra
- Put a live counter in the hero (e.g. spans ingested per second in the demo instance, or a real benchmark number that animates).
- Add a dark "numbers band" with four big stats, each with a unit and scope (spans/s on one core, p95 query ms, binary size MB, RAM idle).
- Build a bento of animated micro-demos, one card per capability (trace tree, eval run, cost chart, alert), cycling real-looking data.
- Add a developer band that offers integration paths (OTLP endpoint / Python SDK / TS SDK / framework auto-instrumentation) with docs and GitHub buttons side by side.

## Scores
- Beauty 5/5: iconic gradient, impeccable type, lively micro-demos.
- Selling 5/5: relentless quantified proof and clear audience routing.
- Clarity 3/5: the long, abstract H1 is slow to parse.
- Proof 5/5: live ticker, $1.9T, logos, ARR stories, API stats.
- Product-showing 4/5: animated UI demos early, but code arrives late.

## Best screenshots
- caps/stripe-hero.jpg: mesh gradient, GDP ticker, two-tone headline, logo strip.
- caps/stripe-part01.jpg: bento of animated product micro-demos.
- caps/stripe-part02.jpg: "Backbone of global commerce" numbers band and enterprise story.
- caps/stripe-part04.jpg: dark developer band with API scale stats and integration paths.
