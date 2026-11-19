# Clerk (https://clerk.com)

## Hero
- **Headline:** "Everything you need for authentication".
- **Subhead:** "More than APIs. The complete user layer for modern apps."
- **Primary CTA:** not a button. It is a pill on the right with agent logos (Claude, VS Code/Copilot, Cursor…) and "Copy prompt", which copies "Add Clerk auth to my app: clerk.com/SKILL.md". The nav has "Sign in" and "Sign up", and the dark top bar says "Configure Clerk with your agent | Install the MCP ▸".
- **Secondary CTA:** none in the hero. The cookie banner (Decline/Accept) was not hidden by the capture and covers part of the hero.
- **Hero visual:** a large framed panel with four tabs (User Authentication / Multi-tenancy / Enterprise Authentication / Agent Authentication). Each tab shows Clerk's real, live UI components (Create your account with Google/GitHub and a "Last used" badge, user profile, billing, an OAuth consent screen "Cursor would like to access your Acme account"…). The components are rendered as HTML, not images. Sub-tabs read "Sign In & Sign Up / User Button / User Profile". Side components are faded to focus on the centre one.

## Structure
1. Agent-setup bar, floating pill nav, hero headline and the copy-prompt pill.
2. Tabbed live-component showcase (product as hero).
3. A small logo row: Inngest, Braintrust, OpenRouter, Upstash, Consensus.
4. Dark section, "Auth that keeps evolving": a bento grid of 12 feature tiles (MFA, session management, OTP, fraud prevention, social SSO, magic links, bot detection, passwords, API keys, Clerk CLI). Each tile has a small monochrome micro-illustration or animation.
5. Light section, "Everything your B2B SaaS needs": multi-tenancy bento (roles grid with avatars, auto-join, invitations, org switcher component).
6. "Agents build faster and more securely with Clerk": scrolling marquee rows of CLI commands and natural-language prompts (`clerk doctor`, `clerk env pull`, "Set my app up for B2B…"), then three links (MCP, CLI, works with your agent).
7. Dark split: "Build with SDKs for modern frameworks" and "Integrate with the tools you love", as icon grids.
8. "Trusted around the world": a free-tier statement ("Free for your first 50,000 monthly retained users…") and a masonry wall of tweets/testimonials (Supabase CEO quote, developer tweets). Then the footer.

## Proof
- A thin 5-logo row under the hero showcase (developer-tool brands, including Braintrust).
- An end-of-page testimonial wall, mostly developer tweets with handles ("I rolled my own auth and this is the amount of code I needed…"), plus a featured quote from Supabase's CEO.
- SOC 2 Type 2 and CCPA are mentioned in the "Advanced security" tile.
- No usage numbers, stars or benchmarks on the homepage.

## Product shown how
- The actual product (drop-in UI components) is the hero, rendered as real interactive HTML across four tabs and several sub-tabs. That is the ideal case for a UI-component product.
- Later sections use stylised micro-illustrations rather than screenshots.
- Code is shown only as CLI command chips (2 code blocks).

## Visual style
- **Type:** "suisse" (Suisse Int'l) with geistNumbers. The h1 is 56px/700 with tight -1.96px tracking.
- **Colours:** light grey page #F7F7F8, white cards, near-black #131315 for the alternating dark bands, charcoal #2B2B31 buttons, and a single violet accent #6541FF (sampled on the tab dot and the eyebrow labels). Frameworks/integrations eyebrows use a cyan/violet tint.
- **Mode and density:** alternating light/dark bands joined by chamfered (angled-corner) section edges, a signature shape. Restrained, premium, low-saturation. 1 canvas; motion comes from the marquees and the micro-animations in tiles.
- **Signature devices:** the floating pill nav, faded neighbouring components around a focused one, the bento grid of grey 3D-ish micro-illustrations, the chamfered section transitions, marquee command chips.

## Pricing / comparison pages
- Not captured. The homepage carries the key pricing hook in the closing section: free for the first 50,000 monthly retained users and 100 monthly retained orgs.
- No vs pages seen.
- Docs: "Docs ▾" is the second nav item (a dropdown). The footer links Documentation and an "LLM Leaderboard" resource.

## What makes it sell
- The product is the hero: you see the exact sign-in, profile and billing components you would ship, polished to a pixel.
- "Copy prompt" as the primary CTA meets developers where they now work (inside the coding agent), with almost no friction.
- The bento grid makes a long feature list feel light and explorable.
- Tweets from real developers read as authentic peer proof.
- The visual rhythm (light → dark → light, chamfered edges) keeps a long page feeling premium.

## Weaknesses
- No visible "Start building" or sign-up button in the hero. The primary action is ambiguous for non-agent users.
- Proof is light near the top (5 logos), with no numbers.
- Later sections use abstract illustrations that show little real behaviour.

## Steal for cetra
- Live UI-as-hero: a tabbed panel rendering real cetra views as HTML with seeded data (Trace tree / Eval run / Prompt diff / Cost dashboard), focusing one view and fading the rest.
- A "Copy prompt" / "Install with your agent" pill alongside the install command (e.g. "Instrument my app with cetra: …/SKILL.md").
- A bento feature grid with tiny monochrome micro-animations, in alternating dark/light bands.
- A marquee of real CLI commands and natural-language agent prompts to show agent-readiness.

## Scores
- Beauty 5/5 — restrained, premium, crafted components and transitions.
- Selling 4/5 — product-first and agent-native; weak CTA hierarchy.
- Clarity 4/5 — headline plus visible components are instantly clear.
- Proof 3/5 — few logos, tweets only at the end.
- Product-showing 5/5 — real interactive components are the hero.

## Best screenshots
- caps/clerk-hero.jpg — headline, copy-prompt pill and tabbed live-component hero (cookie banner visible).
- caps/clerk-part01.jpg — component showcase, logo row and the dark bento feature grid.
- caps/clerk-part02.jpg — multi-tenancy bento, agent command marquee, chamfered dark/light transitions.
- caps/clerk-part03.jpg — "Trusted around the world" tweet wall with the free-tier line.
