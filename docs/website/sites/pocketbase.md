# PocketBase (https://pocketbase.io)

## Hero
- **Headline:** "Open Source backend / in 1 file". The second line is bold. It is not marked up as an h1 (the json has `h1: null`).
- **Subhead:** none. Instead, a pill holds four checkmarked features: "Realtime database · Authentication · File storage · Admin dashboard".
- **Primary CTA:** "Live demo" (black button, goes to https://pocketbase.io/_/, the real admin UI running as a demo).
- **Secondary CTA:** "Read the documentation" (outlined, goes to /docs). The nav also has a "Documentation" button.
- **Hero visual:** a large, real screenshot of the admin dashboard (Collections › posts table with records, "API preview" and "New Record" buttons). It is framed in a dark window bar and fills the lower half of the viewport. A Gopher mascot peeks over its top-left corner. The two CTAs float over the middle of the screenshot. The background is pale blue (#B3D5DF) with faint topographic contour lines. A small pink "v0.42.2" version badge sits next to the logo.

## Structure
1. Hero: two-line claim, feature pill, real admin screenshot with the CTAs overlaid.
2. "Ready to use out of the box": four feature tabs on the left (Realtime database, Authentication, File storage, Extendable) and a JavaScript/Dart SDK code sample on the right, plus "Explore all features →".
3. "Integrate nicely with your favorite frontend stack": five framework icons (Flutter, Svelte, Vue, React, Angular).
4. Footer: FAQ, Discussions, Documentation, JS SDK, Dart SDK, and "Crafted by Gani".

The whole page is about 2,300px tall.

## Proof
- Nothing on the page: no logos, stars, numbers, testimonials or benchmarks. The GitHub icon in the nav is the only hint of community.
- The proof is the product itself: a live demo you can click into, and a 12MB binary.

## Product shown how
- A real UI screenshot fills the first viewport, which is the most prominent element.
- A one-click live demo of the actual admin dashboard.
- One code block (the SDK CRUD and realtime subscription example), shown right after the hero.
- The docs intro (pages/pocketbase-docs) gives per-OS download links ("Download v0.42.2 for Linux x64 (~12MB zip)") and "./pocketbase serve … And that's it!". It lists the three default routes (/, /_/ dashboard, /api/).

## Visual style
- **Type:** Source Sans Pro throughout. The headline is light/regular on line 1 and bold on line 2, set fairly small (about 36px judging from the hero image).
- **Colours:** white #FFFFFF, a pale-blue hero band (#B3D5DF, sampled), charcoal #2A2C32 for text and buttons, and green checkmarks. The UI screenshot is grey and white.
- **Mode and density:** light, sparse and calm. No video and no canvas; 8 images.
- **Signature devices:** the contour-line background, the Gopher mascot peeking over the screenshot, the version badge on the logo, and plain black/white outlined buttons.

## Pricing / comparison pages
- No pricing page (free and open source) and no comparison pages.
- The docs are the main destination: "Documentation" is in the nav and the secondary CTA.
- The docs intro does three things very well:
  - **Honesty:** a red banner warns it is "NOT recommended for production critical applications yet" before v1.0.
  - **Direct downloads** with sizes, x64/ARM64 tabs and a Releases link.
  - **Zero to running in three lines.**
- The docs sidebar uses tree glyphs (├ └) and "Extend with Go / Extend with JavaScript" tabs.

## What makes it sell
- "in 1 file" is the whole pitch, in four words. It is instantly understood and memorable.
- The first viewport is mostly the real product UI. You see exactly what you get.
- "Live demo" as the primary CTA means you touch the real admin with no signup.
- The minimalism itself signals "simple to run": the site is as small as the product.
- The docs quick-start proves the claim (download 12MB, run `serve`, done).

## Weaknesses
- No proof at all: no stars, users or performance numbers. It relies entirely on word of mouth.
- The CTA buttons overlaid on the screenshot look accidental and hide part of the UI.
- No install command or download on the homepage. You must go to the docs to find the binary.

## Steal for cetra
- A one-phrase architectural claim as the headline (e.g. the "one binary" idea), with a checkmark pill listing the bundled parts (ingest, storage, evals, UI).
- A real UI screenshot dominating the first viewport, and a "Live demo" CTA to a read-only hosted instance with seeded traces.
- A docs quick-start that names the binary size, gives per-OS downloads and states the default routes/ports after `serve`.
- An honest maturity banner in the docs while pre-1.0. It builds trust with developers.

## Scores
- Beauty 3/5 — clean and pleasant, but plain and slightly dated.
- Selling 3/5 — strong claim and demo, nothing else persuades.
- Clarity 5/5 — "Open Source backend in 1 file" lands instantly.
- Proof 1/5 — no logos, stars, numbers or quotes.
- Product-showing 5/5 — real UI hero plus a clickable live demo.

## Best screenshots
- caps/pocketbase-hero.jpg — "in 1 file" claim, feature pill, real admin UI as the hero.
- caps/pocketbase-part01.jpg — the whole homepage: hero, feature tabs with the SDK code, framework icons.
- pages/pocketbase-docs-hero.jpg — docs intro with the pre-1.0 warning, per-OS 12MB downloads and default routes.
