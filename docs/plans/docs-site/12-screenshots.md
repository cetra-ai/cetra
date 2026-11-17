# W12 — Reproducible screenshots

**Commit subject:** `Add reproducible UI screenshots to the docs`

**Goal:** `just docs-screenshots` produces the site's UI screenshots from the web UI running on its MSW mocks, deterministically (D-W-12). The PNGs are committed and used on the pages that describe a screen. CI checks every listed screenshot exists.

## Read first

- README § Decisions D-W-12; § Screenshots.
- `web/vite.config.ts`, `web/src/mocks/browser.ts`, `web/src/mocks/handlers.ts`, `fixtures/api/README.md` (the org `acme`, the project `support-bot`, the fixture times on 2026-01-01), `justfile` § `web-dev`.
- The pages of W7–W10 that describe a screen.
- After install: `website/node_modules/playwright/README.md`; Playwright's `page.clock.setFixedTime` and `browser.newContext` options.

**Check first:** `pnpm --dir web run dev` serves the UI with the mocks (MSW) at `/acme/support-bot/traces` (start it, open the URL, stop it). If the mock routes or the org and project slugs differ, use the real ones and say so.

## Files

| File | Change |
|---|---|
| `website/package.json`, `pnpm-lock.yaml` | `playwright` 1.65.0 (devDependency) |
| `website/screenshots.json` (new) | the shot list |
| `website/scripts/screenshots.mjs` (new) | the Playwright script |
| `website/scripts/check-screenshots.mjs`, `check-screenshots.test.mjs` (new) | the CI check |
| `website/src/assets/screenshots/*.png` (new) | the images |
| the pages that show them | `<Image>` |
| `justfile` | `docs-screenshots` |
| `website/README.md` | § Screenshots |

## Steps

### 1. Install

Add `"playwright": "1.65.0"` to `website/package.json` `devDependencies`; `pnpm install`. The package downloads no browser on install. Never add a browser download to CI or to `just js-install`.

### 2. `screenshots.json`

```json
[
  { "name": "trace-list", "path": "/acme/support-bot/traces", "waitFor": "chat claude-sonnet-4-5" },
  { "name": "trace-view", "path": "/acme/support-bot/traces", "waitFor": "chat claude-sonnet-4-5", "actions": [{ "click": "text=chat claude-sonnet-4-5" }], "waitAfter": "Timeline" },
  { "name": "sessions", "path": "/acme/support-bot/sessions", "waitFor": "Sessions" },
  { "name": "users", "path": "/acme/support-bot/users", "waitFor": "Users" },
  { "name": "dashboard", "path": "/acme/support-bot/dashboard", "waitFor": "Cost" },
  { "name": "sql-console", "path": "/acme/support-bot/sql", "waitFor": "Run" },
  { "name": "evaluators", "path": "/acme/support-bot/evaluators", "waitFor": "Evaluators" },
  { "name": "online-rules", "path": "/acme/support-bot/online-rules", "waitFor": "Online rules" },
  { "name": "datasets", "path": "/acme/support-bot/datasets", "waitFor": "Datasets" },
  { "name": "experiments", "path": "/acme/support-bot/experiments", "waitFor": "Experiments" },
  { "name": "alerts", "path": "/acme/support-bot/alerts", "waitFor": "Alerts" }
]
```

The `waitFor` texts are guesses from the fixtures: open each route in the dev server and replace each with a text that appears only once the page's data has loaded. Fix `waitAfter` the same way.

### 3. `screenshots.mjs`

Plain Node ESM using `playwright`'s `chromium`:

1. Spawn `pnpm --dir ../web exec vite --port 5199 --strictPort` (cwd the repository root resolved from `import.meta.url`), and poll `http://localhost:5199/` until it answers (60 s timeout, then fail with the dev server's output).
2. `chromium.launch()`, `browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2, colorScheme: "light", reducedMotion: "reduce", locale: "en-US", timezoneId: "UTC" })`.
3. For each shot (or only the names given as arguments): new page; `await page.clock.setFixedTime(new Date("2026-01-01T11:00:00Z"))` before `goto` (the fixtures are on 2026-01-01 between 10:00 and 10:59 UTC: check and pick a time just after the latest one); `goto("http://localhost:5199" + path)`; `await page.getByText(waitFor).first().waitFor()`; run `actions` (`click` with `page.locator(selector).first().click()`), then `waitAfter` if set; `await page.waitForLoadState("networkidle")`; `page.screenshot({ path: "src/assets/screenshots/<name>.png", fullPage: false })`.
4. Close the browser and kill the dev server (also on error: `try/finally`).
5. Print one line per shot written.

If MSW's service worker needs a first load before it intercepts, `goto` the root and wait for the worker (`navigator.serviceWorker.ready`) once before the first shot.

### 4. The recipe

```make
# Rewrite the docs' UI screenshots from the web UI on its mocks (D-W-12). Needs Chromium:
# `pnpm --dir website exec playwright install chromium` once. Optional: shot names.
docs-screenshots *names: js-install
    cd website && node scripts/screenshots.mjs {{names}}
```

Run `pnpm --dir website exec playwright install chromium` once, then `just docs-screenshots`. Run it twice: the second run must leave `git status` clean (deterministic output). If it does not, find the moving part (a relative time, an animation, a random ID) and fix it in the script (clock, reduced motion), never in the UI; report what it was.

### 5. `check-screenshots.mjs`

Fails when a `screenshots.json` entry has no PNG, or a PNG under `src/assets/screenshots/` has no entry. `check-screenshots.test.mjs` covers both. Add it to `check:content`.

### 6. Use them

On each page that describes the screen (`tracing/traces`, `tracing/sessions`, `tracing/users`, `tracing/dashboard`, `tracing/sql-console`, `evaluation/evaluators`, `evaluation/online-rules`, `evaluation/datasets`, `evaluation/experiments`, `platform/alerts`, and the quick start's "See it" step with `trace-view`):

```mdx
import { Image } from "astro:assets";
import traceView from "../../../../assets/screenshots/trace-view.png";

<Image src={traceView} alt="The trace view: the span tree on the left, the timeline and the selected span's input and output on the right." />
```

(Fix the relative depth per page.) Alt text says what the image shows, in one sentence.

### 7. `website/README.md` § Screenshots

When to run it (a PR that changes a screen shown in the docs), the one-time browser install, adding a shot (an entry in `screenshots.json`, then the run), and that a reviewer checks the PNG diff.

## Tests

- `check-screenshots.test.mjs`; `just check-docs` (the images optimise at build).
- Two runs of `just docs-screenshots` give identical files.

## Done when

- `just check` passes; eleven screenshots are committed and shown; the second run leaves the tree clean.
