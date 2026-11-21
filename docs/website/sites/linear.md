# Linear (https://linear.app)

## Hero
- Headline: "The product development system for teams and agents"
- Subhead: "Purpose-built for planning and building products. Designed for the AI era."
- CTAs: no button in the hero itself. The only conversion points above the fold are "Sign up" (white pill, top-right nav) and "Log in". Right of the subhead is a small "New Loops →" link to the latest launch.
- Visual: a large, realistic product UI that fills the lower half of the first viewport and bleeds off the bottom: sidebar (Inbox, My issues, Reviews, Initiatives), an issue "Faster app launch" with an activity feed, and a floating agent chat panel ("Opus 5.5") that opened a draft PR with a +22 −10 diff. It's built from HTML, not a bitmap (the text is in innerText). It shows a real workflow rather than a decorative mockup.

## Structure
1. Nav, then the headline and product UI described above.
2. Logo strip (OpenAI, Vercel, Salesforce, Figma, Cursor, Coinbase, Ramp) with a tiny mono caption "Powering the companies building the future".
3. Manifesto paragraph ("A new species of product tool…"), first sentence white and the rest grey, followed by three "FIG 0.1/0.2/0.3" line-art illustrations (Purpose-built / Powered by agents / Designed for speed).
4. Four numbered feature chapters, each laid out as: big two-line title on the left, one paragraph and "Learn more →" on the right, a wide UI composition, then a "Features" row of expandable sub-features (+):
   - Intake & integrations: Slack thread → kanban.
   - Planning & monitoring: roadmap timeline plus a "Cycle time by agent" scatter chart.
   - AI & automations: four agent chat windows side by side.
   - Build, review & ship: review queue plus a code diff.
5. Changelog: four dated entries on a timeline, "View all →".
6. Two large testimonial cards (OpenAI engineer on a pale gradient, Ramp head of eng on acid yellow), followed by "Linear powers over 50,000 product teams" and "Customer stories →".
7. Closing CTA "Built for the future. Available today." with Get started and Contact sales.
8. Six-column footer.

## Proof
- Logo strip directly under the hero.
- "50,000 product teams" appears under the testimonials and again on pricing ("Trusted by more than 50,000 companies").
- Two named testimonials with title and company, near the bottom.
- The dated changelog is used as proof that the team ships fast.
- No benchmarks or GitHub stars (it isn't open source).

## Product shown how
Product UI is the main visual of every section, starting in the first viewport. It's static HTML/CSS (0 videos, 0 canvases; 7 code blocks for the diff). The UI uses believable data (issue IDs, timestamps, PR numbers, a real code diff). There are no illustrations of the product, only the product itself plus three abstract FIG drawings.

## Visual style
- Type: Inter Variable at 64px, weight 510, letter-spacing −1.4px (tight and slightly heavier than regular). Mono for small labels ("FIG 0.1", dates).
- Colour: near-black page rgb(8,9,10) #08090A, grey-on-grey UI panels, white text with mid-grey secondary text. Linear indigo #5E6AD2 appears only on the skip link and the send button. Colour is held back until the testimonial cards (pale lavender and #E4F222-ish yellow).
- Dark and very airy: about 10,000px of page for four chapters.
- Signature devices: UI panels that fade into the dark at their edges, a mono "FIG" numbering, the two-tone headline paragraph (white lead sentence, grey continuation), and thin hairline dividers.

## Pricing / comparison pages
- Pricing (pages/linear-pricing): four columns (Free $0, Basic $10, Business $16, Enterprise Custom) with a "Billed yearly" toggle, the same logo strip again, and a long feature matrix grouped by Core / AI and agent workflows / Integrations, using checks and crosses. Clean and scannable. The free tier's limits are concrete (250 issues, 2 teams).
- Docs: in the footer ("Documentation", "Developers"); there's no top-nav Docs link in the screenshot.
- No "vs" page captured. A "Switch" link sits in the footer under Resources.

## What makes it sell
- The hero is the product doing the new thing: an agent opens a PR from an issue. It shows the pitch instead of claiming it.
- Every chapter repeats the same layout (title, one-line value, product UI, sub-feature list), so the page is easy to skim.
- The dated changelog on the homepage signals momentum.
- The logo strip is chosen for an AI/dev audience (OpenAI, Cursor, Vercel).
- Craft itself is the selling point: the restraint and typographic precision signal that the tool is high quality.

## Weaknesses
- No CTA button in the hero, so a first-time visitor has to find "Sign up" in the nav.
- The page is heavy on mood and thin on concrete outcomes (no numbers about speed and no setup story).

## Steal for cetra
- Put a realistic trace view (span tree, LLM I/O, eval score, cost) built in HTML in the hero, bleeding off the fold, with plausible data instead of lorem ipsum.
- Use repeating chapter blocks (big title, one line, UI composition, sub-feature chips) for Ingest → Explore → Evaluate → Alert.
- Add a dated changelog strip on the homepage to show the project ships.
- Write a two-tone manifesto paragraph (white claim, grey elaboration) for the "one binary, one machine" positioning.

## Scores
- Beauty 5/5: the benchmark for dark, restrained, precise SaaS craft.
- Selling 3/5: shows the product beautifully but has no hero CTA or hard numbers.
- Clarity 4/5: headline plus agent UI explain it within seconds.
- Proof 4/5: strong logos, 50k teams, two named quotes.
- Product-showing 5/5: real UI in every section, starting in the hero.

## Best screenshots
- caps/linear-hero.jpg: headline over a full product UI with an agent opening a PR.
- caps/linear-part01.jpg: logo strip, two-tone manifesto, FIG line-art triad.
- caps/linear-part03.jpg: code-diff chapter, changelog timeline, colourful testimonial cards.
- pages/linear-pricing-part01.jpg: four-tier pricing and grouped feature matrix.
