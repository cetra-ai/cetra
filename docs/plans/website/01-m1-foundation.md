# M1: marketing site foundation

Status: planned 2026-11-20. Four phases, F1–F4, done in order, one commit each.

**Summary.** Add the marketing-site layer to the existing Astro project `website/`:
- design tokens and base styles for the dark cetra look;
- the self-hosted fonts;
- one layout (header, footer, release banner);
- the base components every later page uses (buttons, sections and colour bands, chapter layout, cards, the rim divider, a copyable command box, tabs);
- a style-guide page at `/styleguide/` to review them;
- the checks that keep marketing pages correct: page structure, metadata, `noindex`, internal links, page weight, colour contrast.

No real page content: M2 adds the homepage.

**Prerequisites**
- PR #161 (the docs site in `website/`) merged on `main`. This plan quotes its files as merged.
- The branch `claude/website-m1`, cut from `claude/website-design` after that branch was rebased on the `main` that contains #161.

**Read first**
- `docs/website/00-design.md`: §3 architecture, §4 visual direction, §7 checks, §10 M1.
- `docs/plans/website/README.md`: the conventions for every phase.
- `website/README.md`, `website/STYLE.md`, `website/package.json`, `website/astro.config.mjs`, `website/tsconfig.json`, `website/src/pages/index.astro` (the holding page, untouched in M1).
- `assets/brand/README.md` and `assets/brand/cetra-logo-on-dark.svg`.
- `biome.json` (formatting: 2 spaces, double quotes, semicolons, trailing commas, line width 100).

## What it is, and what it is not

When M1 is done:
- `pnpm --dir website run build` produces `/styleguide/`, a dark page that shows every component and the colour bands. It carries `noindex` and is linked from nowhere.
- `just check-docs` also runs the marketing checks and their unit tests.

M1 does **not** add:
- the homepage or any real page (M2–M5);
- `sitemap`, Open Graph images or the switch from the holding page (M7);
- any change to the Starlight docs, their styles or `src/pages/index.astro`. The docs already link back to the marketing site: Starlight's site title links to `/`.
- a sitemap filter. Starlight adds `@astrojs/sitemap`, so `/styleguide/` (and later the `noindex` pages) appear in `dist/sitemap-0.xml` until M7, which filters the sitemap at launch. A listed `noindex` page is harmless: search engines honour the `noindex`.
- the data-file checks of design §7 (prices, comparison facts, integrations). They come with their data in M3, M4 and M5.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-W-1, D-W-2 | One Astro project `website/`: marketing at `/`, docs at `/docs/` |
| D-W-3 | Deployed as the Cloudflare Worker `cetra-site` with static assets (`website/wrangler.jsonc`); M1 changes nothing there |
| D-W-10 | Cloudflare Web Analytics only, nothing in the repo |
| D189 | The Caetra c logo; teal `#0f766e`, light teal `#2dd4bf`, ink `#141414`, paper `#f5f5f4`; Space Grotesk |
| Design §2 | Dark theme; references Bun, Braintrust, Linear; the main hero action is the demo |
| Design §3 | Marketing pages are plain Astro pages with their own layout and styles; no Starlight, no client framework |
| Design §7 | Checks instead of a Lighthouse score: see "Deviation from the design" |

**User answers recorded in the design (2026-11-19):** dark, Cloudflare, in this repo, the demo first, launch everything at once except benchmarks.

**Deviation from the design (fixed in the design doc in F4).** Design §7 asked for "a Lighthouse-style budget (performance, accessibility ≥ 95)". CI never downloads a browser (`website/README.md` § Screenshots), so Lighthouse and axe cannot run there. M1 replaces them with deterministic checks on the built HTML:
- structure: one `h1`, no skipped heading level, `alt` on every image, a name on every link and button;
- metadata;
- a page-weight cap;
- a WCAG contrast test on the colour tokens.

F4 updates design §7 to say so.

## Versions

Checked with `npm view` on 2026-11-20. The repo installs only versions older than 7 days (`minimumReleaseAge: 10080`, D74).

| Package | Version | Published | Where | Why |
|---|---|---|---|---|
| `@fontsource-variable/space-grotesk` | 5.3.0 | 2026-07-19 | `website` dependencies | display face (the brand's) |
| `@fontsource-variable/inter` | 5.3.0 | 2026-07-19 | `website` dependencies | body face (the web UI's) |
| `@fontsource-variable/jetbrains-mono` | 5.3.0 | 2026-07-19 | `website` dependencies | code and labels (the web UI's) |

The implementer re-checks these with `npm view <package> version time --json` and reports the versions used. If a newer version is older than 7 days, still use the version in this table, and report the newer one.

## Phases

```
F1 tokens, fonts, site constants, small libraries ──► F2 layout, header, footer ──► F3 components ──► F4 style guide page and checks
```

Strictly sequential: each phase uses the previous one's files.

**Formatting rule for every phase.** After writing the files, run `pnpm --dir website exec biome check --write <the files you created or changed>`. Biome's formatting and import order win over the code quoted in this plan; behaviour must stay as quoted. After that, `biome check` must report no errors. Two warnings are expected and accepted:
- `noUnusedVariables` on Astro props used only in the template;
- `noImportantStyles` on the three `!important` in `base.css` (the reduced-motion override).

**What each phase's checks cover.** F2 and F3 add components that no page renders until F4, so `astro check` and Biome are their only checks; F4's build and `check-marketing.mjs` then exercise them all.

## Files this milestone creates

```
website/src/marketing/
  routes.mjs                 marketing routes and launch switch (F1)
  site.ts                    URLs, nav, install commands (F1)
  assets/cetra-logo-on-dark.svg   byte copy of assets/brand/cetra-logo-on-dark.svg (F1)
  assets/brand.test.mjs      the copy equals the brand file (F1)
  lib/github-stars.mjs       build-time star count (F1)
  lib/github-stars.test.mjs  (F1)
  styles/tokens.css          colour, type, spacing tokens (F1)
  styles/base.css            element styles for marketing pages (F1)
  styles/contrast.test.mjs   WCAG contrast of the token pairs (F1)
  layouts/MarketingLayout.astro   (F2)
  components/Logo.astro, ReleaseBanner.astro, Header.astro, Footer.astro, Button.astro   (F2)
  components/Section.astro, SectionHeading.astro, Chapter.astro, Card.astro,
             RimDivider.astro, CopyCommand.astro, Tabs.astro   (F3)
website/src/pages/styleguide.astro   (F4)
website/scripts/check-marketing.mjs, check-marketing.test.mjs   (F4)
```

---

## F1: tokens, fonts, site constants and small libraries

**Goal:** the data and styles every marketing page imports, with their unit tests.

**Edit:**
- `website/package.json`
- `pnpm-lock.yaml`
- new `website/src/marketing/routes.mjs`
- new `website/src/marketing/site.ts`
- new `website/src/marketing/assets/cetra-logo-on-dark.svg`
- new `website/src/marketing/assets/brand.test.mjs`
- new `website/src/marketing/lib/github-stars.mjs`
- new `website/src/marketing/lib/github-stars.test.mjs`
- new `website/src/marketing/styles/tokens.css`
- new `website/src/marketing/styles/base.css`
- new `website/src/marketing/styles/contrast.test.mjs`

**Steps**

1. Add the three fonts to `dependencies` in `website/package.json` with the exact versions from the Versions table, keeping keys sorted. Change the `test:scripts` script to `node --test scripts/ src/marketing/`. Run `pnpm install` from the repository root to update `pnpm-lock.yaml`.

2. `website/src/marketing/routes.mjs`, exactly:

```js
// The marketing pages (design §5) and the launch switch (design §10 M7).
// check-marketing.mjs and the layout read this file; each milestone adds its routes.

/** False until M7: every marketing page carries `noindex` and `/` is the holding page. */
export const LAUNCHED = false;

/** Routes that always carry `noindex`, launched or not. */
export const ALWAYS_NOINDEX = ["/styleguide/"];

/** Every marketing route, with a trailing slash. `/docs/` pages are not listed: Starlight owns them. */
export const MARKETING_ROUTES = ["/styleguide/"];

/** @param {string} route */
export function isNoindex(route) {
  return !LAUNCHED || ALWAYS_NOINDEX.includes(route);
}
```

3. `website/src/marketing/site.ts`, exactly:

```ts
// Every URL and repeated string of the marketing site, in one place (design §3 "Links out").

export const SITE_URL = "https://cetra.sh";
export const GITHUB_REPO = "cetra-ai/cetra";
export const GITHUB_URL = `https://github.com/${GITHUB_REPO}`;
export const LICENSE_URL = `${GITHUB_URL}/blob/main/LICENSE`;
export const DOCS_URL = "/docs/";
export const STATUS_URL = "https://status.cetra.sh";
export const CONTACT_EMAIL = "hello@cetra.sh";

/**
 * The cloud. Sources: the control plane's routes `/signup` and `/login` on `app.cetra.sh`
 * (llmobs-control `web/src/routes/`), the demo route `/demo` on the first cell `eu1.cetra.sh`
 * (docs/plans/demo-org/README.md, D187), the status page `status.cetra.sh` (llmobs-control docs).
 */
export const CLOUD = {
  signup: "https://app.cetra.sh/signup",
  login: "https://app.cetra.sh/login",
  /** The demo org (D187). A signed-out visitor is sent to log in first. */
  demo: "https://eu1.cetra.sh/demo",
  /** The regions the cloud offers today (design §9.4). */
  regions: ["EU (eu1)"],
} as const;

/** False until the first release; the layout then shows the release banner. */
export const RELEASED = false;

/** The self-host install commands shown in the copy box (docs: /docs/self-hosting/install/). */
export const INSTALL_COMMANDS = [
  {
    label: "macOS / Linux",
    command:
      "curl -fsSL https://github.com/cetra-ai/cetra/releases/latest/download/install.sh | sh",
  },
  {
    label: "Docker",
    command:
      "docker run -p 8080:8080 -p 4318:4318 -v cetra-data:/data -e CETRA_PUBLIC_URL=http://localhost:8080 ghcr.io/cetra-ai/cetra:latest",
  },
] as const;

export interface NavItem {
  label: string;
  href: string;
  /** Shown only when this marketing route exists (routes.mjs). Absent: always shown. */
  route?: string;
}

export const NAV: NavItem[] = [
  { label: "Product", href: "/#product", route: "/" },
  { label: "Pricing", href: "/pricing/", route: "/pricing/" },
  { label: "Integrations", href: "/integrations/", route: "/integrations/" },
  { label: "Compare", href: "/compare/", route: "/compare/" },
  { label: "Docs", href: DOCS_URL },
];

export interface FooterColumn {
  title: string;
  links: NavItem[];
}

export const FOOTER: FooterColumn[] = [
  {
    title: "Product",
    links: [
      { label: "Pricing", href: "/pricing/", route: "/pricing/" },
      { label: "Self-hosting", href: "/self-hosting/", route: "/self-hosting/" },
      { label: "Security", href: "/security/", route: "/security/" },
      { label: "Integrations", href: "/integrations/", route: "/integrations/" },
      { label: "Compare", href: "/compare/", route: "/compare/" },
    ],
  },
  {
    title: "Developers",
    links: [
      { label: "Docs", href: DOCS_URL },
      { label: "Quick start", href: "/docs/get-started/quickstart/" },
      { label: "Self-hosting guide", href: "/docs/self-hosting/" },
      { label: "API reference", href: "/docs/reference/api/overview/" },
      { label: "GitHub", href: GITHUB_URL },
    ],
  },
  {
    title: "Company",
    links: [
      { label: "Status", href: STATUS_URL },
      { label: "Licence", href: LICENSE_URL },
    ],
  },
];
```

Before writing, check that these docs pages exist: `website/src/content/docs/docs/get-started/quickstart.*`, `website/src/content/docs/docs/self-hosting/index.*`, and `/docs/reference/api/overview/` (linked from `astro.config.mjs`; starlight-openapi serves it). After the F4 build, confirm `website/dist/docs/reference/api/overview/index.html` exists. If one is missing, stop with `QUESTION:`.

The Docker command is a short form of the one in `website/src/content/docs/docs/self-hosting/install.md` § Docker (same image, volume and `CETRA_PUBLIC_URL`; the UI port and the OTLP/HTTP port; the `latest` tag, which the release workflow publishes per `docs/plans/n-packaging/08-versions.md`). Use it as written.

4. `website/src/marketing/assets/cetra-logo-on-dark.svg`: copy `assets/brand/cetra-logo-on-dark.svg` byte for byte (`cp`).

5. `website/src/marketing/assets/brand.test.mjs`: a `node:test` test, **"the logo copy equals the brand file"**. It reads both files with `readFileSync` (paths resolved from `import.meta.dirname`: the brand file is `../../../../assets/brand/cetra-logo-on-dark.svg`) and asserts `deepStrictEqual` on the buffers.

6. `website/src/marketing/lib/github-stars.mjs`, exactly:

```js
// The repository's star count, fetched once at build time (design §3 "Single sources").
// Any failure returns null and the header shows "GitHub" without a number.

/**
 * @param {string} repo `owner/name`
 * @param {{ fetchImpl?: typeof fetch, timeoutMs?: number }} [options]
 * @returns {Promise<number | null>}
 */
export async function fetchStars(repo, { fetchImpl = fetch, timeoutMs = 3000 } = {}) {
  try {
    const res = await fetchImpl(`https://api.github.com/repos/${repo}`, {
      headers: { accept: "application/vnd.github+json", "user-agent": "cetra-website-build" },
      signal: AbortSignal.timeout(timeoutMs),
    });
    if (!res.ok) return null;
    const body = await res.json();
    const n = body?.stargazers_count;
    return Number.isInteger(n) && n >= 0 ? n : null;
  } catch {
    return null;
  }
}

/**
 * 999 → "999", 1000 → "1k", 1250 → "1.3k", 12345 → "12.3k", 1000000 → "1M".
 * @param {number} n
 */
export function formatStars(n) {
  if (n < 1000) return String(n);
  if (Math.round(n / 100) < 10_000) return `${trim(n / 1000)}k`;
  return `${trim(n / 1_000_000)}M`;
}

/** @param {number} x */
function trim(x) {
  return (Math.round(x * 10) / 10).toFixed(1).replace(/\.0$/, "");
}

let cached;
/** One fetch per build, shared by every page. */
export function starsOnce(repo) {
  cached ??= fetchStars(repo);
  return cached;
}
```

7. `website/src/marketing/lib/github-stars.test.mjs`. Tests with `node:test` and `node:assert/strict`, using a fake `fetchImpl` (no network):
   - **"returns the count"**: the response is `{ ok: true, json: async () => ({ stargazers_count: 1234 }) }`, so the result is `1234`.
   - **"null on a non-ok response"**: `{ ok: false }` gives `null`.
   - **"null when the field is missing or not an integer"**: `{}` gives `null`; `{ stargazers_count: "12" }` gives `null`; `-1` gives `null`.
   - **"null when fetch throws"**: the fake throws, so the result is `null`.
   - **"sends the user agent and calls the right URL"**: assert the URL `https://api.github.com/repos/a/b` and the `user-agent` header.
   - **"formats counts"**:
     - 0 → `"0"`;
     - 999 → `"999"`;
     - 1000 → `"1k"`;
     - 1250 → `"1.3k"`;
     - 12345 → `"12.3k"`;
     - 999949 → `"999.9k"`;
     - 999950 → `"1M"` (rounds up to the next unit);
     - 1000000 → `"1M"`;
     - 2500000 → `"2.5M"`.

8. `website/src/marketing/styles/tokens.css`, exactly. The raw palette comes first, then semantic tokens. Components use only the semantic tokens, so a band can re-map them.

```css
/* cetra marketing tokens (design §4). Dark only. Brand colours: assets/brand/README.md (D189). */
:root {
  /* Raw palette */
  --c-ink: #141414;
  --c-ink-2: #1b1b1b;
  --c-ink-3: #232323;
  --c-line: #2e2e2e;
  --c-line-strong: #3d3d3d;
  --c-paper: #f5f5f4;
  --c-stone-300: #d6d3d1;
  --c-stone-400: #a8a29e;
  --c-stone-600: #57534e;
  --c-teal: #0f766e;
  --c-teal-light: #2dd4bf;
  --c-teal-deep: #0a2e2b;
  --c-teal-ink: #042f2e;

  /* Semantic tokens (re-mapped by .tone-* bands) */
  --bg: var(--c-ink);
  --surface: var(--c-ink-2);
  --surface-2: var(--c-ink-3);
  --line: var(--c-line);
  --line-strong: var(--c-line-strong);
  --fg: var(--c-paper);
  --fg-muted: var(--c-stone-400);
  --accent: var(--c-teal-light);
  --accent-fg: var(--c-teal-ink);
  --link: var(--c-teal-light);
  --focus: var(--c-teal-light);
  --band-bg: var(--c-ink);

  /* Type */
  --font-display: "Space Grotesk Variable", "Space Grotesk", ui-sans-serif, system-ui, sans-serif;
  --font-body: "Inter Variable", "Inter", ui-sans-serif, system-ui, sans-serif;
  --font-mono:
    "JetBrains Mono Variable", "JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, monospace;
  --text-xs: 0.8125rem;
  --text-sm: 0.9375rem;
  --text-base: 1.0625rem;
  --text-lg: 1.25rem;
  --text-xl: 1.5rem;
  --text-2xl: clamp(1.75rem, 1.2rem + 2vw, 2.5rem);
  --text-3xl: clamp(2.25rem, 1.4rem + 3.4vw, 3.5rem);
  --text-hero: clamp(2.75rem, 1.5rem + 5vw, 5rem);
  --leading-tight: 1.05;
  --leading-snug: 1.25;
  --leading-body: 1.6;
  --tracking-display: -0.03em;
  --tracking-label: 0.08em;

  /* Space and shape */
  --space-1: 0.25rem;
  --space-2: 0.5rem;
  --space-3: 0.75rem;
  --space-4: 1rem;
  --space-5: 1.5rem;
  --space-6: 2rem;
  --space-7: 3rem;
  --space-8: 4rem;
  --space-9: 6rem;
  --space-10: 8rem;
  --radius-sm: 6px;
  --radius: 10px;
  --radius-lg: 16px;
  --container: 1200px;
  --container-narrow: 760px;
  --container-wide: 1360px;
  --gutter: max(16px, env(safe-area-inset-left, 0px));
  --shadow-panel: 0 1px 0 rgb(255 255 255 / 0.04) inset, 0 24px 48px -24px rgb(0 0 0 / 0.6);
  color-scheme: dark;
}

/* Bands: each re-maps the semantic tokens for its content. */
.tone-ink {
  --band-bg: var(--c-ink);
}
.tone-raised {
  --band-bg: var(--c-ink-2);
  --surface: var(--c-ink-3);
}
.tone-teal {
  --band-bg: var(--c-teal-deep);
  --surface: #0f3b37;
  --surface-2: #134843;
  --line: #1d5650;
  --line-strong: #2a6c64;
  --fg-muted: #a7d8d1;
}
.tone-paper {
  --band-bg: var(--c-paper);
  --surface: #ffffff;
  --surface-2: #ecebe9;
  --line: #dddbd8;
  --line-strong: #c9c6c2;
  --fg: var(--c-ink);
  --fg-muted: var(--c-stone-600);
  --accent: var(--c-teal);
  --accent-fg: #ffffff;
  --link: var(--c-teal);
  --focus: var(--c-teal);
  color-scheme: light;
}
```

9. `website/src/marketing/styles/base.css`, exactly:

```css
/* Element styles for marketing pages only (imported by MarketingLayout). */
*,
*::before,
*::after {
  box-sizing: border-box;
}
html {
  -webkit-text-size-adjust: 100%;
  scroll-behavior: smooth;
  scroll-padding-top: 5rem;
}
body {
  margin: 0;
  background: var(--bg);
  color: var(--fg);
  font-family: var(--font-body);
  font-size: var(--text-base);
  line-height: var(--leading-body);
  -webkit-font-smoothing: antialiased;
  text-rendering: optimizeLegibility;
}
h1,
h2,
h3,
h4 {
  font-family: var(--font-display);
  font-weight: 600;
  letter-spacing: var(--tracking-display);
  line-height: var(--leading-tight);
  text-wrap: balance;
  margin: 0;
}
p {
  margin: 0;
  text-wrap: pretty;
}
a {
  color: var(--link);
  text-underline-offset: 0.2em;
}
img,
svg {
  display: block;
  max-width: 100%;
  height: auto;
}
code,
kbd,
pre {
  font-family: var(--font-mono);
  font-size: 0.9em;
}
:focus-visible {
  outline: 2px solid var(--focus);
  outline-offset: 3px;
  border-radius: 4px;
}
.container {
  width: 100%;
  max-width: var(--container);
  margin-inline: auto;
  padding-left: var(--gutter);
  padding-right: max(16px, env(safe-area-inset-right, 0px));
}
.container--narrow {
  max-width: var(--container-narrow);
}
.container--wide {
  max-width: var(--container-wide);
}
.eyebrow {
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  letter-spacing: var(--tracking-label);
  text-transform: uppercase;
  color: var(--accent);
}
.muted {
  color: var(--fg-muted);
}
.visually-hidden {
  position: absolute;
  width: 1px;
  height: 1px;
  padding: 0;
  margin: -1px;
  overflow: hidden;
  clip: rect(0, 0, 0, 0);
  white-space: nowrap;
  border: 0;
}
@media (prefers-reduced-motion: reduce) {
  html {
    scroll-behavior: auto;
  }
  *,
  *::before,
  *::after {
    animation-duration: 0.01ms !important;
    animation-iteration-count: 1 !important;
    transition-duration: 0.01ms !important;
  }
}
```

10. `website/src/marketing/styles/contrast.test.mjs`. It parses `tokens.css` and checks WCAG contrast.
    - Export nothing. Implement `parseTokens(css)`: return a `Map` from each `--c-*` name to its hex, taken from lines matching `/^\s*(--c-[a-z0-9-]+):\s*(#[0-9a-f]{6});/i`. Only raw palette tokens are parsed. In the table below, a `--c-*` name is looked up in that map and a `#rrggbb` literal is used as it is (the bands in `tokens.css` use those literals).
    - Implement `luminance(hex)` per WCAG 2.2 (sRGB channel `c/255`; `c <= 0.04045 ? c/12.92 : ((c+0.055)/1.055)**2.4`; `0.2126 R + 0.7152 G + 0.0722 B`) and `ratio(a, b) = (L1 + 0.05) / (L2 + 0.05)` with `L1 >= L2`.
    - Test **"text pairs meet 4.5:1"** over this exact table (foreground, background):

| fg | bg |
|---|---|
| `--c-paper` | `--c-ink` |
| `--c-stone-400` | `--c-ink` |
| `--c-paper` | `--c-ink-2` |
| `--c-stone-400` | `--c-ink-2` |
| `--c-stone-400` | `--c-ink-3` |
| `--c-teal-light` | `--c-ink` |
| `--c-teal-ink` | `--c-teal-light` |
| `--c-paper` | `--c-teal-deep` |
| `#a7d8d1` | `--c-teal-deep` |
| `--c-ink` | `--c-paper` |
| `--c-stone-600` | `--c-paper` |
| `--c-teal` | `--c-paper` |
| `#ffffff` | `--c-teal` |
| `--c-teal-light` | `--c-ink-2` |
| `--c-teal-light` | `--c-ink-3` |
| `--c-teal-light` | `--c-teal-deep` |
| `--c-teal-light` | `#0f3b37` |
| `--c-paper` | `#0f3b37` |
| `#a7d8d1` | `#0f3b37` |
| `#a7d8d1` | `#134843` |
| `--c-stone-600` | `#ecebe9` |
| `--c-teal` | `#ffffff` |

    - Test **"every palette name in the table exists"**: each `--c-*` name above is in the parsed map, so a renamed token fails loudly.
    - Test **"luminance of black and white"**: `luminance("#000000") === 0`, `luminance("#ffffff") === 1`, and `ratio("#000000", "#ffffff")` is 21.

    If a pair fails, stop with `QUESTION:` and the computed ratios. Never change a brand colour (D189).

**Tests**
- `brand.test.mjs`: the logo copy equals the brand file.
- `github-stars.test.mjs`: the 6 cases in step 7.
- `contrast.test.mjs`: the 3 cases in step 10.

**Done when**
- `pnpm --dir website exec node --test src/marketing/` passes.
- `pnpm --dir website exec biome check src/marketing package.json` passes.
- `just check-docs` passes.

**Commit:** `Add the marketing site tokens, fonts and site constants`

---

## F2: layout, header, footer

**Goal:** `MarketingLayout.astro` renders a full page: head metadata, release banner, header, main slot, footer.

**Edit:** new files in `website/src/marketing/`: `layouts/MarketingLayout.astro`, `components/Logo.astro`, `components/ReleaseBanner.astro`, `components/Header.astro`, `components/Footer.astro`, `components/Button.astro`.

**Steps**

1. `components/Button.astro`:

```astro
---
interface Props {
  href: string;
  variant?: "primary" | "secondary" | "ghost";
  size?: "md" | "lg";
  /** Opens in the same tab; true adds rel="noopener" for links off cetra.sh. */
  external?: boolean;
  /** Extra text for screen readers, e.g. "(free account)". */
  srHint?: string;
}
const { href, variant = "primary", size = "md", external = false, srHint } = Astro.props;
---
<a class:list={["btn", `btn--${variant}`, `btn--${size}`]} href={href} rel={external ? "noopener" : undefined}>
  <slot />{srHint && <span class="visually-hidden"> {srHint}</span>}
</a>
<style>
  .btn {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    font-family: var(--font-body);
    font-weight: 600;
    text-decoration: none;
    border-radius: 999px;
    border: 1px solid transparent;
    transition: background-color 0.15s ease, border-color 0.15s ease, color 0.15s ease;
    white-space: nowrap;
  }
  .btn--md {
    font-size: var(--text-sm);
    padding: 0.55rem 1.05rem;
  }
  .btn--lg {
    font-size: var(--text-base);
    padding: 0.8rem 1.4rem;
  }
  .btn--primary {
    background: var(--accent);
    color: var(--accent-fg);
  }
  .btn--primary:hover {
    background: color-mix(in srgb, var(--accent) 85%, white);
  }
  .btn--secondary {
    background: transparent;
    color: var(--fg);
    border-color: var(--line-strong);
  }
  .btn--secondary:hover {
    border-color: var(--fg-muted);
  }
  .btn--ghost {
    background: transparent;
    color: var(--fg);
    padding-inline: 0.4rem;
  }
  .btn--ghost:hover {
    color: var(--accent);
  }
</style>
```

2. `components/Logo.astro`. It renders the copied logo as an `<img>` linking to `/`:

```astro
---
import logo from "../assets/cetra-logo-on-dark.svg";
interface Props {
  height?: number;
}
const { height = 28 } = Astro.props;
const width = Math.round((logo.width / logo.height) * height);
---
<a class="logo" href="/" aria-label="cetra home">
  <img src={logo.src} width={width} height={height} alt="cetra" />
</a>
<style>
  .logo {
    display: inline-flex;
    align-items: center;
  }
</style>
```

If `astro check` reports that an SVG import has no `width`/`height` in this Astro version, stop with `QUESTION:` and the exact error.

3. `components/ReleaseBanner.astro`: renders only when `RELEASED` (from `site.ts`) is false.
   - Markup: `<div class="release-banner" role="note">` inside a `.container` holding the text `cetra is in development and not released yet. ` followed by `<a href="/docs/">Read the docs</a>`.
   - Style: `background: var(--c-teal-deep); color: var(--fg); font-size: var(--text-xs); text-align: center; padding-block: var(--space-2); border-bottom: 1px solid var(--line);`.

4. `components/Header.astro`:
   - Frontmatter: import `NAV`, `GITHUB_URL`, `GITHUB_REPO`, `CLOUD` from `../site`; `MARKETING_ROUTES` from `../routes.mjs`; `starsOnce`, `formatStars` from `../lib/github-stars.mjs`; `Logo` and `Button`.
   - `const items = NAV.filter((i) => !i.route || MARKETING_ROUTES.includes(i.route));`
   - `const stars = await starsOnce(GITHUB_REPO);`
   - Markup:
     - A `<header class="site-header">` (sticky: `position: sticky; top: 0; z-index: 20; background: color-mix(in srgb, var(--c-ink) 85%, transparent); backdrop-filter: blur(12px); border-bottom: 1px solid var(--line);`). It contains a `.container.header-inner` laid out as a grid, `grid-template-columns: auto 1fr auto; align-items: center; height: 64px; gap: var(--space-6)`.
     - Left: `<Logo />`.
     - Middle: `<nav aria-label="Main">` with a `<ul>` of `items`. Links use `color: var(--fg-muted); text-decoration: none; font-size: var(--text-sm)`; hover `color: var(--fg)`. List: `display: flex; gap: var(--space-5); list-style: none; margin: 0; padding: 0`.
     - Right, `display: flex; gap: var(--space-3); align-items: center`:
       - a GitHub link `<a class="gh" href={GITHUB_URL}>` showing a GitHub mark SVG (16 px, `aria-hidden="true"`, `viewBox="0 0 16 16"`, `fill="currentColor"`, this exact path (GitHub's Octicon `mark-github`, MIT licence): `M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z`), the text `GitHub` and, when `stars !== null`, `<span class="stars">{formatStars(stars)}</span>`. Give it an `aria-label`: "cetra on GitHub" plus `, ${stars} stars` when known.
       - `<a class="login" href={CLOUD.login}>Log in</a>`.
       - `<Button href={CLOUD.demo} external srHint="(needs a free account)">Try the demo</Button>`.

       The GitHub link also gets `rel="noopener"`.
   - Mobile, below 860 px:
     - hide the `nav` and the login link;
     - show a `<details class="menu">` with `<summary aria-label="Menu">`. The summary holds three `<span class="bar"></span>` (each `display: block; width: 18px; height: 2px; background: var(--fg); border-radius: 1px`, stacked with `gap: 4px` in a flex column; `list-style: none` and `summary::-webkit-details-marker { display: none }`). The panel is a `ul` listing the same `items` plus "Log in" and "GitHub", with `position: absolute; right: max(16px, env(safe-area-inset-right, 0px)); top: 64px; z-index: 30; min-width: 220px; background: var(--surface); border: 1px solid var(--line); border-radius: var(--radius); padding: var(--space-4)`. The `header-inner` gets `position: relative`.

     Above 860 px, hide `details.menu`. No JavaScript.

5. `components/Footer.astro`:
   - Import `FOOTER`, `CONTACT_EMAIL` from `../site`; `MARKETING_ROUTES` from `../routes.mjs`; `Logo`.
   - Filter every column's links like the header. Drop a column whose links are all filtered out.
   - Markup:
     - `<footer class="site-footer">` (`border-top: 1px solid var(--line); background: var(--c-ink); padding-block: var(--space-8) var(--space-6)`) with `.container` and a grid `grid-template-columns: 1.4fr repeat(3, 1fr); gap: var(--space-6)`. It becomes 2 columns below 860 px and 1 below 520 px.
     - First cell: `<Logo height={24} />`, then a `p.muted` reading "Open-source LLM observability and evaluation. One binary, on your machine or in our cloud."
     - Each column: an `h2` styled as `.eyebrow` (`font-size: var(--text-xs)`; it is an `h2` for heading order) and a `ul` of links (`color: var(--fg-muted)`, no underline, hover `var(--fg)`).
     - Bottom row, above a `border-top: 1px solid var(--line)` separator (the copyright holder is the company sebastiencs, D190): `© {new Date().getFullYear()} sebastiencs. The cetra core is MIT; enterprise features are under the Elastic License 2.0.` and the contact link `<a href={`mailto:${CONTACT_EMAIL}`}>{CONTACT_EMAIL}</a>`.

6. `layouts/MarketingLayout.astro`:

```astro
---
import "@fontsource-variable/space-grotesk";
import "@fontsource-variable/inter";
import "@fontsource-variable/jetbrains-mono";
import "../styles/tokens.css";
import "../styles/base.css";
import Header from "../components/Header.astro";
import Footer from "../components/Footer.astro";
import ReleaseBanner from "../components/ReleaseBanner.astro";
import { SITE_URL } from "../site";
import { isNoindex } from "../routes.mjs";

interface Props {
  /** The page title without the site name; the home page passes "cetra". */
  title: string;
  /** One sentence, at most 160 characters (checked by check-marketing.mjs). */
  description: string;
  /** The route with a trailing slash, e.g. "/pricing/". */
  route: string;
}
const { title, description, route } = Astro.props;
const fullTitle = title === "cetra" ? "cetra: open-source LLM observability" : `${title} · cetra`;
const canonical = new URL(route, SITE_URL).href;
---
<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover" />
    <title>{fullTitle}</title>
    <meta name="description" content={description} />
    <link rel="canonical" href={canonical} />
    {isNoindex(route) && <meta name="robots" content="noindex" />}
    <link rel="icon" href="/favicon.svg" type="image/svg+xml" />
    <meta name="theme-color" content="#141414" />
    <meta property="og:type" content="website" />
    <meta property="og:site_name" content="cetra" />
    <meta property="og:title" content={fullTitle} />
    <meta property="og:description" content={description} />
    <meta property="og:url" content={canonical} />
    <slot name="head" />
  </head>
  <body>
    <a class="skip-link" href="#main">Skip to content</a>
    <ReleaseBanner />
    <Header />
    <main id="main">
      <slot />
    </main>
    <Footer />
  </body>
</html>
<style>
  .skip-link {
    position: absolute;
    left: var(--space-4);
    top: -100px;
    z-index: 50;
    background: var(--accent);
    color: var(--accent-fg);
    padding: var(--space-2) var(--space-4);
    border-radius: var(--radius-sm);
  }
  .skip-link:focus {
    top: var(--space-4);
  }
</style>
```

**Tests:** none new (Astro components). F4's checks cover the rendered output. `astro check` must report 0 errors and 0 warnings for the new files.

**Done when**
- `pnpm --dir website exec astro check` passes.
- `pnpm --dir website exec biome check src/marketing` passes.
- `just check-docs` passes.

**Commit:** `Add the marketing layout, header and footer`

---

## F3: base components

**Goal:** the building blocks of every page, styled from the tokens.

**Edit:** new files in `website/src/marketing/components/`: `Section.astro`, `SectionHeading.astro`, `Chapter.astro`, `Card.astro`, `RimDivider.astro`, `CopyCommand.astro`, `Tabs.astro`.

**Steps**

1. `RimDivider.astro`: the design §4 motif, the logo's rim arc and boss dot, as a section seam.

```astro
---
interface Props {
  /** Where the mark sits on the line. */
  align?: "start" | "center";
}
const { align = "start" } = Astro.props;
---
<div class:list={["rim", `rim--${align}`]} aria-hidden="true">
  <span class="line"></span>
  <svg viewBox="1.8 4.5 55 55" width="22" height="22">
    <path d="M49.62 17.22A23 23 0 1 0 49.62 46.78" fill="none" stroke="currentColor" stroke-width="7.5" stroke-linecap="round" />
    <circle cx="32" cy="32" r="8.5" fill="currentColor" />
  </svg>
  <span class="line"></span>
</div>
<style>
  .rim {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    color: var(--accent);
  }
  .rim .line {
    height: 1px;
    background: var(--line);
    flex: 1;
  }
  .rim--start .line:first-child {
    flex: 0 0 var(--space-6);
  }
  svg {
    flex: none;
    opacity: 0.9;
  }
</style>
```

   The path and circle are the mark's own geometry (`assets/brand/cetra-mark-on-dark.svg`), coloured by `currentColor`.

2. `Section.astro`:

```astro
---
interface Props {
  tone?: "ink" | "raised" | "teal" | "paper";
  id?: string;
  width?: "default" | "narrow" | "wide";
  /** Vertical padding. */
  pad?: "md" | "lg";
  /** A rim divider at the top of the section. */
  divider?: boolean;
  /** The id of the heading that names this section (aria-labelledby). */
  labelledby?: string;
}
import RimDivider from "./RimDivider.astro";
const { tone = "ink", id, width = "default", pad = "lg", divider = false, labelledby } = Astro.props;
---
<section id={id} class:list={["section", `tone-${tone}`, `pad-${pad}`]} aria-labelledby={labelledby}>
  <div class:list={["container", width !== "default" && `container--${width}`]}>
    {divider && <div class="divider"><RimDivider /></div>}
    <slot />
  </div>
</section>
<style>
  .section {
    background: var(--band-bg);
    color: var(--fg);
    overflow: hidden;
  }
  .pad-md {
    padding-block: var(--space-8);
  }
  .pad-lg {
    padding-block: var(--space-10);
  }
  @media (max-width: 720px) {
    .pad-lg {
      padding-block: var(--space-8);
    }
  }
  .divider {
    margin-bottom: var(--space-8);
  }
</style>
```

   The `tone-*` classes come from `tokens.css` (global), so the semantic tokens re-map inside the band.

3. `SectionHeading.astro`:
   - Props `{ eyebrow?: string; title: string; lede?: string; align?: "start" | "center"; level?: 1 | 2; id?: string }`, defaults `align = "start"`, `level = 2`.
   - Renders a `div.heading` (`max-width: 46rem`, plus `margin-inline: auto; text-align: center` when centred). Inside:
     - the eyebrow as `p.eyebrow`;
     - the title as `h1` or `h2` (by `level`) with `id`. Font size `var(--text-hero)` for level 1 and `var(--text-3xl)` for level 2, `margin-top: var(--space-3)`;
     - the lede as `p.lede` (`font-size: var(--text-lg); color: var(--fg-muted); margin-top: var(--space-4)`).

4. `Chapter.astro`: the Linear-style repeated chapter, design §4.
   - Props `{ eyebrow?: string; title: string; lede: string; features?: string[]; id?: string }`.
   - Default slot: the visual (a scene or screenshot).
   - Markup:
     - `div.chapter` grid with two rows: the text block, then the visual in `div.visual`.
     - The text block is a grid of `1fr 1fr` above 960 px: left the eyebrow and the title, right the lede and the feature list. The title is an `h2` with `id`, `font-size: var(--text-2xl)`.
     - Features: a `ul.features` with `list-style: none; padding: 0; display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: var(--space-2) var(--space-5); font-size: var(--text-sm); color: var(--fg-muted)`. Each `li` has a 6 px teal dot before it (`::before`, `background: var(--accent); border-radius: 50%`).
   - `div.visual`: `margin-top: var(--space-7); border: 1px solid var(--line); border-radius: var(--radius-lg); background: var(--surface); box-shadow: var(--shadow-panel); overflow: hidden; position: relative`. Below 720 px it scrolls horizontally (`overflow-x: auto`).
   - A bottom fade over the visual: `::after` with `inset: auto 0 0 0; height: 30%; background: linear-gradient(to bottom, transparent, var(--band-bg)); pointer-events: none`.

5. `Card.astro`:
   - Props `{ title?: string; href?: string; tone?: "surface" | "outline" }`, default `"surface"`.
   - Renders an `article.card`. With `href`, the title is a link whose `::after` covers the card (`position: absolute; inset: 0`) so the whole card is clickable, and the card gets `position: relative`. The title is an `h3` (`font-size: var(--text-lg)`).
   - Styles:
     - `.card`: `padding: var(--space-6); border-radius: var(--radius-lg); border: 1px solid var(--line)`;
     - surface: `background: var(--surface)`;
     - outline: `background: transparent`;
     - hover with `href`: `border-color: var(--line-strong)`.
   - Default slot below the title, with `color: var(--fg-muted); margin-top: var(--space-2)`.

6. `CopyCommand.astro`: the copyable shell command box (Bun's install CTA).
   - Props `{ commands: readonly { label: string; command: string }[]; note?: string; noteHref?: string; id: string }`. The `id` is unique per page.
   - Markup:
     - `div.copy` with `data-copy`;
     - when `commands.length > 1`, a `div.tabs[role=tablist]` of `button[role=tab][type=button]`, each with `id={`${id}-tab-${i}`}`, `aria-controls={`${id}-panel-${i}`}`, `aria-selected` (`"true"` for index 0) and `tabindex` (`0` for the selected tab, `-1` otherwise);
     - one `div.panel[role=tabpanel]` per command, with `id`, `aria-labelledby` and `hidden` for all but the first. Each panel holds `<pre><code><span class="prompt" aria-hidden="true">$ </span><span class="cmd">{command}</span></code></pre>` and a `button.copy-btn` with `type="button"`, `aria-label="Copy command"`, the text `Copy`, and `data-copy-target` set to the panel id;
     - when `note`, a `p.note` (with a link when `noteHref`).
   - Styles:
     - `.copy`: `border: 1px solid var(--line); border-radius: var(--radius); background: var(--c-ink-2)`;
     - tabs: `display: flex; gap: var(--space-1); padding: var(--space-2); border-bottom: 1px solid var(--line)`;
     - tab buttons: `font: inherit; font-size: var(--text-xs); background: none; border: 0; color: var(--fg-muted); padding: var(--space-1) var(--space-3); border-radius: var(--radius-sm); cursor: pointer`, `[aria-selected=true]` with `background: var(--surface-2); color: var(--fg)`;
     - panel: `display: flex; align-items: center; gap: var(--space-3); padding: var(--space-3) var(--space-4)`;
     - `pre`: `margin: 0; overflow-x: auto; flex: 1; font-size: var(--text-sm); color: var(--c-paper)`;
     - `.prompt`: `color: var(--c-teal-light); user-select: none`;
     - copy button: like a `.btn--secondary` at `--text-xs` (write the CSS here; no class sharing with Button).
   - One `<script>` (Astro bundles and dedupes it):
     - For every `[data-copy]`, wire the tabs. A click selects the tab: set `aria-selected`, `tabindex` and `hidden` on the panels. ArrowLeft and ArrowRight move to the previous and next tab, wrapping around at the ends; Home and End go to the first and last tab. Moving selects the tab and focuses it.
     - For every `.copy-btn`, a click copies the text of `#<target> .cmd` with `navigator.clipboard.writeText`. On success the label becomes "Copied" for 1.5 s. On failure (rejected promise or no `navigator.clipboard`) the label becomes "Press Ctrl+C" and the command's text is selected with `getSelection().selectAllChildren`.
     - The `$ ` prompt is never copied, because only `.cmd` is read.

7. `Tabs.astro`: a generic tab switcher for scenes (used from M2).
   - Props `{ id: string; labels: string[] }`.
   - Markup: `div.tabs-root[data-tabs][data-tabs-id={id}]`; a `div[role=tablist]` of `button[role=tab][type=button]` with ids `${id}-tab-${i}`, `aria-selected` and `tabindex` as in `CopyCommand`, and **no** `aria-controls` in the markup; then `<slot />`. The children are the panels, each authored by the page as `<div role="tabpanel" data-tab-panel>`.
   - The script:
     - on load, for each `[data-tabs]`, takes its direct-descendant `[data-tab-panel]` elements in order and sets `id` (`${id}-panel-${i}`, with the root's id prefix stored as `data-tabs-id`), `aria-labelledby` and `hidden` (not the first) on each, and sets `aria-controls` on the matching tab button;
     - wires clicks and keys exactly like `CopyCommand` (wrap-around arrows, Home, End);
     - if the panel count differs from the label count, logs `console.error("Tabs <id>: N labels, M panels")` and leaves every panel visible.
   - Styles for the tab buttons as in `CopyCommand`, at `--text-sm`.

**Tests:** none new here (F4's style guide renders every component, and its checks run on that page).

**Done when**
- `pnpm --dir website exec astro check` passes.
- `pnpm --dir website exec biome check src/marketing` passes.
- `just check-docs` passes.

**Commit:** `Add the marketing base components`

---

## F4: style guide page and marketing checks

**Goal:** `/styleguide/` shows every component in every band. `check-marketing.mjs` checks every marketing route after the build and runs in `just check-docs`.

**Edit:**
- new `website/src/pages/styleguide.astro`
- new `website/scripts/check-marketing.mjs`
- new `website/scripts/check-marketing.test.mjs`
- `website/package.json` (the `check` script)
- `website/README.md`
- `docs/website/00-design.md` (§7)
- `docs/plans/website/README.md` (status)
- `docs/website/00-design.md` (§10, M1 row)

**Steps**

1. `website/src/pages/styleguide.astro`: `<MarketingLayout title="Style guide" description="The cetra marketing components and colour bands, for review." route="/styleguide/">`. Its content, in order:
   1. A `Section tone="ink"` with a `SectionHeading level={1} eyebrow="Style guide" title="cetra marketing components" lede="Every component and band of the marketing site."`. Then a row of `Button`s: primary `lg`, secondary `lg`, ghost; all `href="/docs/"`.
   2. A `Section tone="raised" divider` with `SectionHeading title="Type scale"` and one `<p>` in each size token (`--text-hero` to `--text-xs`, set with an inline `style`), each preceded by a `<p class="eyebrow">` with the token name. No heading tags in this list.
   3. A `Section tone="ink"` with a `CopyCommand id="install" commands={INSTALL_COMMANDS} note="What does this run?" noteHref="/docs/self-hosting/install/"`.
   4. A `Section tone="teal" divider` with a `Chapter eyebrow="Chapter" title="A chapter title" lede="One sentence that says what this part of the product does." features={["First feature", "Second feature", "Third feature", "Fourth feature"]}`. Its visual is a `div` 320 px high showing the text "Scene area".
   5. A `Section tone="paper"` with `SectionHeading title="Cards"` (an `h2`) and three `Card`s with titles in a 3-column grid (`grid-template-columns: repeat(auto-fit, minmax(240px, 1fr)); gap: var(--space-5)`): one with `href="/docs/"`, one without, one `tone="outline"`.
   6. A `Section tone="ink"` with a `Tabs id="demo-tabs" labels={["One", "Two"]}` and two panels with one paragraph each.
   7. A colour swatch grid of every raw `--c-*` token, each square labelled with its name and hex in `.eyebrow`. The list is hard-coded in the page, matching `tokens.css`.

   Every heading level must follow on from the one before (one `h1`, then `h2`s, then `h3`s). The footer's column titles are `h2`s, which is valid after any heading.

2. `website/src/marketing/routes.mjs` already lists `/styleguide/` (F1). Nothing to add.

3. `website/scripts/check-marketing.mjs`. It checks the built marketing pages, prints `route: message` per finding, and exits 1 when there is any. Before writing the regular expressions, build once (`pnpm --dir website exec astro build`) and read `website/dist/styleguide/index.html`. Astro adds `data-astro-cid-*` attributes and may order attributes differently from this plan, and the checks must work on that real output.
   - **Parsing helpers** (exported, unit-tested):
     - `stripBlocks(html)` removes `<script…>…</script>`, `<style…>…</style>` and `<!-- … -->`.
     - `tags(html, name)` returns, for every opening tag `<name …>` (case-insensitive, `/<name\b([^>]*)>/gi`), a `Map` of its attributes. Attributes are parsed with `/([^\s=/>]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+)))?/g`; names are lower-cased, a bare attribute has the value `""`, and values are entity-decoded.
     - `decode(text)` decodes `&amp; &lt; &gt; &quot; &#34; &#39; &#x27;` and any `&#NNN;` / `&#xHH;`.
     - `elements(html, name)` returns `{ attrs, inner }` for every `<name …>inner</name>` (non-greedy, case-insensitive; used for `a`, `button`, `h1`–`h6`).
     - `textOf(inner)` removes tags, decodes, and collapses whitespace.
   - **`checkPage(route, html, { noindex, distFiles = new Set() })`** returns an array of strings. It runs these checks on `stripBlocks(html)`, except check 13, which uses the full HTML:
     1. `missing lang="en"` unless the `html` tag has `lang="en"`.
     2. `title missing` when the `<title>` text is empty.
     3. `description missing`, or `description is N characters, the limit is 160`. The value is the decoded `content` of the `meta` tag whose `name` is `description`.
     4. `canonical must be https://cetra.sh<route>` unless the `link` tag with `rel="canonical"` has exactly that `href`.
     5. `noindex expected` when `noindex` is true and no `meta` tag has `name="robots"` with a `content` containing `noindex`; `noindex not allowed` when false and one has.
     6. `expected one h1, found N`.
     7. `heading level skipped: hA then hB` when a heading is more than one level deeper than the previous heading, in document order. Going back up any number of levels is fine.
     8. `image without alt` for each `img` tag without an `alt` attribute (`alt=""` is fine).
     9. `link without a name: <href>` for each `a` element whose `textOf(inner)` is empty, that has no `aria-label`, and contains no `img` with a non-empty `alt`.
     10. `button without a name` likewise for `button` elements (text or `aria-label`).
     11. `broken internal link: <href>`, for the `href` of `a` elements only (not `link`), when it starts with `/` and not `//`. Strip any `#…` and `?…`. A path ending in `/` must have `<path>index.html` in `distFiles`; any other path must be in `distFiles` itself. While `LAUNCHED` is false, skip `href="/"` and hrefs starting with `/#`: the holding page is at `/` and has no anchors.
     12. `href="#" not allowed` for an `a` element whose `href` is exactly `#`.
     13. `banned word: <word>` for each match in the visible text (`textOf` of the stripped `body`) of `/\b(seamless(ly)?|blazing(ly)?|effortless(ly)?|powerful|simply)\b/gi`, and `write cetra in lower case` for each match of `/\b(Cetra|CETRA)\b/g` (website/STYLE.md).
   - **`pageWeight(html, readAsset)`** returns the bytes of the HTML plus every `link` tag with `rel="stylesheet"` and an `href` starting with `/_astro/`, and every `script` tag with a `src` starting with `/_astro/`, each read through `readAsset(path)` (returns a `Buffer`). Fonts are not counted: CSS loads them, nothing links them.
   - **`listedRoutes(distFiles, launched)`** returns the routes of every `index.html` in `distFiles` that is not under `/docs/`, minus `/` while `launched` is false (the holding page). The route of `/x/y/index.html` is `/x/y/`.
   - **`main`** runs when `import.meta.main` is true (Node 26):
     1. Build `distFiles` by walking `website/dist` (relative paths with `/` separators and a leading `/`).
     2. For each route in `listedRoutes(distFiles, LAUNCHED)` that is not in `MARKETING_ROUTES`: finding `unlisted marketing page`.
     3. For each route in `MARKETING_ROUTES`: read `dist<route>index.html` (missing → finding `page not built`), run `checkPage` with `noindex: isNoindex(route)` and `distFiles`, and check the weight against `MAX_PAGE_BYTES = 350_000` (finding `page weighs N bytes, the cap is 350000`).
     4. Print all findings as `route: message` and exit 1 if there are any. Otherwise print `check-marketing: N pages ok`.

4. `website/scripts/check-marketing.test.mjs` (node:test). Use a helper `page(overrides)` building a valid page from parts (lang, title, description, canonical, robots, body), by default `<html lang="en"><head><title>T</title><meta name="description" content="D"><link rel="canonical" href="https://cetra.sh/x/"><meta name="robots" content="noindex"></head><body><h1>A</h1></body></html>`. Call `checkPage("/x/", html, { noindex: true, distFiles })`. Cases:
   - **"a valid page has no findings"**;
   - **"attributes in another order and data-astro-cid attributes are fine"**: `<html data-astro-cid-x lang="en">`, `<meta content="D" name="description">`, `<link href="https://cetra.sh/x/" rel="canonical">`;
   - **"missing lang"**, **"empty title"**, **"missing description"**;
   - **"description of 161 characters"**; 160 passes; `&amp;` and `&#x27;` count as one character each;
   - **"wrong canonical"**;
   - **"noindex expected"** and **"noindex not allowed"**;
   - **"two h1"** and **"no h1"**;
   - **"h2 then h4 is a skip"**, and **"h3 then h2 is fine"**;
   - **"img without alt"**, and **"img with empty alt is fine"**;
   - **"empty link"**, **"link named by aria-label"**, **"link named by an img alt"**;
   - **"empty button"**;
   - **"broken internal link"**: `/nope/` with `distFiles` lacking `/nope/index.html`;
   - **"internal link ok"**: `/docs/` with `/docs/index.html` present, and `/docs/#x` also passes;
   - **"holding page links skipped before launch"**: `/` and `/#product` give no finding;
   - **"external, mailto and in-page links not checked"**: `https://example.com/`, `//cdn.example.com/x`, `mailto:a@b.c` and `#main` give no finding;
   - **"href # not allowed"**;
   - **"banned word"**: `<p>A powerful tool</p>` gives `banned word: powerful`; **"capitalised name"**: `<p>Cetra runs</p>` gives `write cetra in lower case`;
   - **"script and style content ignored"**: an `<h1>` inside `<script>` does not count, and `powerful` inside `<style>` is not reported;
   - **"page weight sums html, css and js"**: a fake `readAsset` returns 100- and 200-byte buffers for one stylesheet and one script; the weight is the HTML's byte length plus 300;
   - **"listed routes"**: `listedRoutes(new Set(["/index.html", "/styleguide/index.html", "/docs/index.html", "/docs/a/index.html", "/404.html"]), false)` deep-equals `["/styleguide/"]`, and with `true` deep-equals `["/", "/styleguide/"]`.

5. `website/package.json`: append ` && node scripts/check-marketing.mjs` to the end of the `check` script, after `astro build`.

6. `website/README.md`: add a section **"Marketing pages"** after "Where pages live", covering:
   - Marketing pages are plain Astro pages under `src/pages/` using `src/marketing/layouts/MarketingLayout.astro`, never Starlight. Their components, styles and constants are in `src/marketing/`.
   - Every marketing route is listed in `src/marketing/routes.mjs`. `check-marketing.mjs` checks each one after the build (structure, metadata, `noindex`, internal links, a 350 KB page-weight cap without fonts).
   - `LAUNCHED` stays false until M7: every marketing page has `noindex` and `/` is the holding page.
   - URLs and repeated strings live in `src/marketing/site.ts`.
   - `/styleguide/` shows every component.
   - The design is `docs/website/00-design.md`.

7. `docs/website/00-design.md` §7: replace the bullet "**Accessibility and weight:** a Lighthouse-style budget (performance, accessibility ≥ 95; page weight cap) on the built pages." with:

   > **Accessibility and weight:** `check-marketing.mjs` checks every built marketing page: one `h1`, no skipped heading level, `alt` on images, a name on every link and button, metadata, `noindex`, internal links, and a 350 KB cap without fonts. `contrast.test.mjs` checks WCAG AA contrast for every text colour pair of the tokens. CI never downloads a browser (`website/README.md`), so Lighthouse and axe are not used (M1 plan).

8. `docs/website/00-design.md` §10, the M1 row, "Delivers" cell: replace it with "Marketing layout (header, footer, release banner), design tokens, fonts, the bands and the rim motif, base components (buttons, copy-command box, tabs, section and chapter layouts, card), the marketing page checks (structure, metadata, `noindex`, internal links, weight, contrast). A style page at `/styleguide/` (noindex) to review the look. The docs already link back to `/` through Starlight's site title; the data-file checks come with their data in M3–M5."

9. `docs/plans/website/README.md`: set M1's status to `done (F1–F4)`.

**Tests**
- `check-marketing.test.mjs`: every case in step 4.
- The real run: `check-marketing: 1 pages ok` after the build.

**Done when**
- `pnpm --dir website exec node --test scripts/check-marketing.test.mjs` passes.
- `just check-docs` passes and prints `check-marketing: 1 pages ok`.
- Opened in a browser (`just docs-dev`, then `http://localhost:4321/styleguide/`), the page shows the dark bands, the teal band and the light paper band with readable text, the copy box copies the command without the `$`, and the tabs switch with the mouse and the arrow keys. Report what you checked; if you cannot open a browser, say so.

**Commit:** `Add the marketing style guide and page checks`
