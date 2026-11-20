# M7: launch

Status: planned 2026-11-20. Two phases, L1–L2, done in order, one commit each.

**Summary.** Everything needed for the launch, in code:
- the homepage replaces the holding page at `/`;
- `LAUNCHED` flips to `true`, which removes `noindex` from every marketing page except `/styleguide/`;
- the sitemap leaves out the `noindex` pages;
- every marketing page gets an Open Graph image;
- the docs' FAQ stops saying cetra Cloud does not exist.

Merging this milestone's PR **is** the launch. The PR waits for the user's yes, like every PR of this track.

**Prerequisites**
- M5 done on `claude/website-m5`.
- The branch `claude/website-m7`, cut from `claude/website-m5`.

**Read first**
- `docs/website/00-design.md` §10 (M7).
- `docs/plans/website/README.md`.
- `website/src/marketing/routes.mjs`.
- `website/src/pages/home.astro`.
- `website/src/pages/index.astro` (the holding page).
- `website/astro.config.mjs`.
- `website/scripts/check-marketing.mjs`.
- `website/src/content/docs/docs/help/faq.mdx`.

## What it is, and what it is not

When M7 is done:
- `https://cetra.sh/` is the homepage, and `/home/` is gone.
- Marketing pages are indexable, while `/styleguide/` keeps `noindex` and is not in the sitemap.
- Every marketing page has `og:image` and a large Twitter card.

M7 does **not** do the manual launch steps. These are the user's (listed at the end):
- attaching `cetra.sh` to the Worker;
- turning on Cloudflare Web Analytics;
- the first release.

`RELEASED` stays `false`: the banner "cetra is in development and not released yet" stays until the first release.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-W-2, D-W-3 | The site is `cetra.sh`, one Worker `cetra-site` with static assets |
| D-W-10 | Cloudflare Web Analytics, set up in the dashboard, nothing in the repo |
| Design §10 M7 | Holding page replaced, `noindex` removed, sitemap, robots, OG images, canonical URLs, analytics. The 404 page is Starlight's `dist/404.html`, already built: nothing to do |

## Versions

| Package | Version | Published | Where | Why |
|---|---|---|---|---|
| `@astrojs/sitemap` | 3.7.4 | 2026-08-31 | `website` dependencies | to pass a `filter`; the version Starlight already resolves (`pnpm-lock.yaml`), so no second copy |

The implementer checks `pnpm-lock.yaml` for the `@astrojs/sitemap` version Starlight resolves, uses that exact version, and reports it.

---

## L1: the homepage at /, and launch on

**Goal:** `/` is the homepage; `LAUNCHED` is true.

**Edit:**
- `website/src/pages/index.astro` (rewritten)
- `website/src/pages/home.astro` (deleted)
- `website/src/marketing/routes.mjs`
- `website/src/content/docs/docs/help/faq.mdx`
- `website/README.md`

**Steps**
1. Replace `website/src/pages/index.astro` with the content of `home.astro`, changing only the layout's `route` from `"/home/"` to `"/"`. Then `git rm website/src/pages/home.astro`.
2. `routes.mjs`:
   - `LAUNCHED = true`;
   - remove `"/home/"` from `MARKETING_ROUTES` and from `ALWAYS_NOINDEX`;
   - add `"/"` as the first entry of `MARKETING_ROUTES`;
   - update the comment on `LAUNCHED` to `True since M7: marketing pages are indexable and / is the homepage.`
3. `check-marketing.mjs` needs no change: with `LAUNCHED` true it checks `href="/"` and `/#…` links, and `listedRoutes` includes `/`. Confirm that the header's `Product` link (`/#product`) passes, i.e. that `dist/index.html` exists.
4. `faq.mdx`, section "Does cetra run in the cloud?": replace its paragraph with `Yes, both ways. Run cetra yourself as one binary or one container on your own machine or cluster, or use cetra Cloud, hosted in the EU. See [Self-hosting](/docs/self-hosting/), and the cetra website for cloud plans.` (no link to a marketing page: the docs' link validator only knows docs pages).
5. `website/README.md`:
   - In "Where pages live", replace the line saying `src/pages/index.astro` is the landing page with `src/pages/index.astro` is the homepage (the marketing site); its sections are in `src/marketing/home/`.
   - In "Marketing pages", replace the sentence about `LAUNCHED` staying false with `LAUNCHED is true since M7: marketing pages are indexable, except the routes in ALWAYS_NOINDEX.`
6. Build, run the checks, and run `pageshots.mjs` on `/`. Read the PNGs.

**Tests:** `check-marketing.mjs` (16 pages: `/` replaces `/home/`).

**Done when**
- `just check-docs` passes and prints `check-marketing: 16 pages ok`.
- `dist/index.html` has no `noindex` and has the canonical `https://cetra.sh/`.
- `dist/styleguide/index.html` still has `noindex`.

**Commit:** `Serve the homepage at / and launch the marketing site`

---

## L2: sitemap filter and Open Graph image

**Goal:** the sitemap leaves out `noindex` pages, and every marketing page shares one Open Graph image.

**Edit:**
- `website/package.json`
- `pnpm-lock.yaml` (repository root)
- `website/astro.config.mjs`
- new `website/scripts/og-image.mjs`
- new `website/public/og.png`
- `website/src/marketing/layouts/MarketingLayout.astro`
- `website/scripts/check-marketing.mjs` and `website/scripts/check-marketing.test.mjs`
- `website/README.md`
- `docs/plans/website/README.md`

**Steps**

1. Add `@astrojs/sitemap` (the version from the Versions table) to `website` dependencies; `pnpm install` from the repo root.

2. `astro.config.mjs`:
   - `import sitemap from "@astrojs/sitemap";` and `import { ALWAYS_NOINDEX } from "./src/marketing/routes.mjs";`;
   - in `integrations`, **before** `starlight(…)`, add:

```js
    // Starlight adds its own sitemap unless one is present; this one leaves out noindex pages.
    sitemap({
      filter: (page) => !ALWAYS_NOINDEX.some((route) => page === `https://cetra.sh${route}`),
    }),
```

   Starlight skips its own sitemap when the config already has `@astrojs/sitemap` (`@astrojs/starlight` `dist/index.js`). After the build, check that `dist/sitemap-0.xml` lists `https://cetra.sh/`, `https://cetra.sh/pricing/` and the docs pages, and does not list `/styleguide/`. Check that `dist/sitemap-index.xml` still exists, since `public/robots.txt` points to it.

3. `website/scripts/og-image.mjs` renders `website/public/og.png` (1200×630) with the website's Playwright (`import { chromium } from "playwright"`, as `scripts/screenshots.mjs` does).
   - `SITE = dirname(dirname(fileURLToPath(import.meta.url)))` is `website/`.
   - Fonts are inlined as base64 `data:` URIs, because Chromium blocks `file://` fonts in a `setContent` page. Read these two files, which exist:
     - `node_modules/@fontsource-variable/space-grotesk/files/space-grotesk-latin-wght-normal.woff2`
     - `node_modules/@fontsource-variable/jetbrains-mono/files/jetbrains-mono-latin-wght-normal.woff2`

     Resolve them from `SITE` with `join(SITE, "node_modules", …)`. Declare them as `@font-face { font-family: "Space Grotesk"; src: url(data:font/woff2;base64,…) format("woff2"); font-weight: 300 700; }` and the same for `"JetBrains Mono"` with `font-weight: 100 800`.
   - The logo is the markup of `src/marketing/assets/cetra-logo-on-dark.svg`, read as text and inserted with `style="height:56px;width:auto"`.
   - Launch Chromium and open a page with viewport 1200×630 and `deviceScaleFactor: 1`. Call `page.setContent(html)`, then `await page.evaluate(() => document.fonts.ready)`. Check that `document.fonts.check('600 68px "Space Grotesk"')` is true; otherwise throw `Error("font did not load")`. Then `page.screenshot({ path: join(SITE, "public/og.png"), type: "png" })` and close the browser.
   - The HTML: `html, body { margin: 0; width: 1200px; height: 630px; background: #141414; }`; the body is a flex column with `justify-content: space-between; padding: 72px 80px; box-sizing: border-box`. It holds:
     - the logo;
     - a block with an `h1` `LLM observability you run yourself.` (`font: 600 68px/1.05 "Space Grotesk"; letter-spacing: -0.03em; color: #f5f5f4; margin: 0; max-width: 900px`) and a `p` `One binary on your server, or cetra Cloud.` (`font: 400 30px/1.3 "Space Grotesk"; color: #a8a29e; margin: 20px 0 0`);
     - a `p` `cetra.sh` (`font: 500 24px "JetBrains Mono"; color: #2dd4bf; margin: 0`).
   - It prints `wrote public/og.png`. Run it (`pnpm --dir website exec node scripts/og-image.mjs`), Read the PNG, check that it uses the fonts and not a fallback, and commit it.

4. `MarketingLayout.astro`: after the `og:url` meta, add:

```astro
    <meta property="og:image" content={new URL("/og.png", SITE_URL).href} />
    <meta property="og:image:width" content="1200" />
    <meta property="og:image:height" content="630" />
    <meta property="og:image:alt" content="cetra: LLM observability you run yourself." />
    <meta name="twitter:card" content="summary_large_image" />
```

5. `check-marketing.mjs`: in `checkPage`, add a check (after the existing ones) reporting `og:image missing` when no `meta` tag has `property="og:image"` with a `content` starting with `https://cetra.sh/`. In `website/scripts/check-marketing.test.mjs`, give the `page()` helper an `ogImage` option whose default is `<meta property="og:image" content="https://cetra.sh/og.png">`, so the existing cases stay valid, and add **"og:image missing"** (`ogImage: ""`).

6. `website/README.md`, "Marketing pages": add `public/og.png is the Open Graph image of every marketing page; regenerate it with pnpm --dir website exec node scripts/og-image.mjs after a brand change.`

7. `docs/plans/website/README.md`: set M7's status to `done (L1–L2)`, and M6's to `not planned: waits for the benchmark`.

**Tests:** the new check case; `check-marketing.mjs` on 16 pages.

**Done when**
- `just check-docs` passes.
- The sitemap check of step 2 holds.
- `og.png` is 1200×630 and readable.

**Commit:** `Filter the sitemap and add the Open Graph image`

---

## Manual launch steps (the user, after merging)

1. In Cloudflare, attach `cetra.sh` as a custom domain of the Worker `cetra-site` (D-W-3).
2. Turn on Cloudflare Web Analytics for the site (D-W-10).
3. Check `https://cetra.sh/`, `/pricing/` and `/compare/langfuse/` on a phone.
4. Before the first release, re-check the comparison facts (`website/src/marketing/data/compare/`): the build fails 183 days after their date anyway.
