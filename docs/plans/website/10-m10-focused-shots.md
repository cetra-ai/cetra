# M10: Focused screenshots on the homepage

The homepage's seven screenshots are whole-app captures at 1440×900, shown about 1,170 px wide, so their text reads at about 9 px. This plan makes each one a crop of the panel its section talks about, taken in a smaller window, and lays the six product chapters out as text beside the picture, alternating sides. The hero puts its picture beside the text on wide screens.

**Prerequisites:** `main` at or after `cbd9748`. Playwright's Chromium for `playwright@1.65.1` is installed (`pnpm --dir website exec playwright install chromium`, done on 2026-11-24).

**Read first:**
- `docs/website/00-design.md` §4 (visual direction) and §9.10.
- `docs/plans/website/README.md` § Conventions for every phase. They apply to both phases.
- `website/README.md` § Screenshots.
- `website/scripts/screenshots.mjs`, `website/screenshots.json`.
- `website/src/marketing/components/Frame.astro`, `Chapter.astro`, `website/src/marketing/home/Hero.astro`, `Chapters.astro`.

## What it is, and what it is not

When it's done:
- `screenshots.json` entries can set `viewport` (the window size of that shot) and `clip` (a CSS selector: only that element is captured).
- The seven `marketing-*` PNGs are crops of one panel each.
- At 960 px and wider, each chapter shows its text in one column and its picture in the other, alternating sides. The hero does the same at 1200 px and wider. A picture is never shown larger than its CSS size.
- Phones keep scrollable crops (design §4).

Out of scope:
- The docs' own screenshots (the entries without `marketing-` in their name): they keep the default window and no clip.
- New screenshots, and any other homepage section.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-W-12 | Screenshots come from the web UI on its MSW mocks, by `just docs-screenshots` |
| D-W-23 | Marketing shots are crops of one panel in a smaller window; product chapters put text beside the picture, alternating sides (the user, 2026-11-24) |

The user's answer (2026-11-24): do all three steps: crop the shots, recapture them, and change the layout.

## Versions

No new dependency.

## Phases

| Phase | Edit | Depends on |
|---|---|---|
| Z1 Crop the marketing shots | `website/scripts/screenshots.mjs`, `website/screenshots.json`, `website/README.md`, the seven `website/src/assets/screenshots/marketing-*.png` | — |
| Z2 Text beside the picture | `website/src/marketing/components/Frame.astro`, `website/src/marketing/components/Chapter.astro`, `website/src/marketing/home/Chapters.astro`, `website/src/marketing/home/Hero.astro` | Z1 (its PNGs set the sizes) |

They run one after the other.

### Z1 Crop the marketing shots

**Goal:** each `marketing-*` shot is a crop of one panel, captured in its own window size.

**Edit:** `website/scripts/screenshots.mjs`, `website/screenshots.json`, `website/README.md`, and the seven PNGs that `just docs-screenshots` rewrites. No other file. In particular, the other PNGs must not change: run the recipe with the seven names only.

**Steps:**
1. In `shoot(context, shot)`, right after `const page = await context.newPage();`, add:
   ```js
   // A marketing shot sets its own window size; the docs' shots keep the context's 1440×900.
   if (shot.viewport) await page.setViewportSize(shot.viewport);
   ```
2. Replace the line `await page.screenshot({ path, fullPage: false, animations: "disabled", caret: "hide" });` with:
   ```js
   const options = { path, animations: "disabled", caret: "hide" };
   // `clip` captures one element (the panel a section talks about) instead of the window.
   if (shot.clip) await page.locator(shot.clip).first().screenshot(options);
   else await page.screenshot({ ...options, fullPage: false });
   ```
3. In `website/screenshots.json`, replace the seven `marketing-*` entries, in place and in the same order, with these (Biome may reformat them):
   ```json
   { "name": "marketing-trace-view", "path": "/acme/support-bot/traces", "waitFor": "100 loaded", "theme": "dark", "viewport": { "width": 1200, "height": 760 }, "actions": [{ "click": "text=How can I export my traces to a CSV file?" }], "waitAfter": "Input / Output", "clip": "aside[aria-label=\"Trace\"]" },
   { "name": "marketing-trace-list", "path": "/acme/support-bot/traces", "waitFor": "100 loaded", "theme": "dark", "viewport": { "width": 1000, "height": 680 }, "actions": [{ "click": "button[aria-pressed]:has-text(\"Filters\")" }], "clip": "main" },
   { "name": "marketing-alerts", "path": "/acme/support-bot/alerts", "waitFor": "High error rate", "now": "2026-01-01T11:00:00Z", "theme": "dark", "viewport": { "width": 1160, "height": 400 }, "clip": "main" },
   { "name": "marketing-assistant", "path": "/acme/support-bot/traces", "waitFor": "100 loaded", "actions": [{ "click": "button[aria-label=\"Ask AI\"]" }, { "click": "textarea[aria-label=\"Message the assistant\"]", "type": "Which traces failed in the last day?" }, { "click": "button[aria-label=\"Send\"]" }], "waitAfter": "Open in traces", "theme": "dark", "viewport": { "width": 1200, "height": 760 }, "clip": "[data-testid=\"assistant-window\"]" },
   { "name": "marketing-playground", "path": "/acme/support-bot/playground?prompt=support-reply&version=2", "waitFor": "Run all", "now": "2026-01-01T11:00:00Z", "actions": [{ "click": "button:has-text('Run all')" }], "waitAfter": "Open trace", "theme": "dark", "viewport": { "width": 1000, "height": 640 }, "clip": "main" },
   { "name": "marketing-queue-review", "path": "/acme/support-bot/annotation-queues/01900000-0000-7000-8000-000000000a01/review/01900000-0000-7000-8000-000000000a11", "waitFor": "Complete and next", "now": "2026-01-01T11:00:00Z", "theme": "dark", "viewport": { "width": 1100, "height": 600 }, "clip": "main" },
   { "name": "marketing-issue", "path": "/acme/support-bot/issues/01900000-0000-7000-8000-000000001c01", "waitFor": "ValueError: could not parse the reply", "now": "2026-01-03T20:00:00Z", "theme": "dark", "viewport": { "width": 960, "height": 600 }, "clip": "main" }
   ```
   What each keeps: the hero, the trace panel (span tree and input/output); the trace list without the filter column; the assistant window; the playground; the review page; the issue page; the alerts page. Below about 1024 px the app collapses its own nav sidebar, so `main` starts at the left edge on the narrower shots.
4. In `website/README.md` § Screenshots, change the "To add a shot" bullet's parenthesis to `(name, path, waitFor, and optionally actions, waitAfter, now, theme, viewport and clip)`, and add this bullet after it:
   `- A marketing shot shows one panel, readable at its size on the page: \`viewport\` (\`{ "width", "height" }\`) sets that shot's window, and \`clip\` (a CSS selector) captures only the first element it matches (D-W-23).`
5. Run `just docs-screenshots marketing-trace-view marketing-trace-list marketing-alerts marketing-assistant marketing-playground marketing-queue-review marketing-issue`. If port 5199 is taken, set `CETRA_SCREENSHOTS_PORT` to a free port (for example 5299) instead of stopping anything.

**Tests:** no unit test covers `screenshots.mjs` (it drives a browser). The checks are by hand:
- `git status` shows exactly the seven `marketing-*.png` changed among the PNGs.
- `file website/src/assets/screenshots/marketing-*.png` gives these sizes (2× the CSS size; ±2 px from font rounding is fine): trace-view about 1388×1440, trace-list 1922×1280, alerts 1904×720, assistant 840×1200, playground 1922×1200, queue-review 1784×1120, issue 1842×1120. A crop that is the full window (2400×1520, 2000×1360…) means the clip did not apply: stop and report.
- Open each PNG: it shows only its panel, with no nav sidebar (the alerts and review shots may show none either, since `main` excludes it).

**Done when:** `just check-docs` passes (its screenshot check fails if a listed shot has no PNG), and the hand checks above hold.

**Commit:** `Crop the homepage screenshots to one panel each`

### Z2 Text beside the picture

**Goal:** the chapters and the hero show text and picture side by side on wide screens, with each picture at most its CSS size.

**Edit:** `website/src/marketing/components/Frame.astro`, `website/src/marketing/components/Chapter.astro`, `website/src/marketing/home/Chapters.astro`, `website/src/marketing/home/Hero.astro`. No other file.

**Steps:**
1. **`Frame.astro`:**
   1. Add a prop to `Props`: `/** The \`sizes\` attribute of the image. */ sizes?: string;`. Destructure it with the default `"(max-width: 720px) 760px, (min-width: 1200px) 1200px, 100vw"`, which is today's value, and pass `sizes={sizes}` to `<Image>`.
   2. The screenshots are captured at 2× (`deviceScaleFactor: 2`), so a shot's CSS width is `src.width / 2`. In the frontmatter add `const cssWidth = Math.round(src.width / 2);` and put `style={\`--shot-width: ${cssWidth}px\`}` on the `<figure>`.
   3. CSS: in `.frame` add `width: fit-content; max-width: 100%;`. Replace the `.frame :global(img)` rule with:
      ```css
      .frame :global(img) {
        display: block;
        width: var(--shot-width);
        max-width: 100%;
        height: auto;
      }
      ```
      In the `@media (max-width: 720px)` block, replace `width: 760px;` in `.scroll :global(img)` with `width: var(--shot-width);`. Keep `max-width: none;` there, so a phone scrolls the crop at its CSS size.
   4. Remove `widths={[720, 1080, 1440, 2160]}` and use `widths={[cssWidth, cssWidth * 2]}` instead, so the browser gets a 1× and a 2× file and never an upscaled one.
2. **`Chapter.astro`:**
   1. Add a prop: `/** Put the picture on the left at 960 px and wider. */ flip?: boolean;` (default `false`). Put `flip && "chapter--flip"` in the root `div`'s `class:list` (the root becomes `<div class:list={["chapter", flip && "chapter--flip"]}>`).
   2. CSS:
      - Delete the `@media (min-width: 960px) { .text { … } }` block, so the text is one column: head, then body.
      - Add:
        ```css
        @media (min-width: 960px) {
          .chapter {
            grid-template-columns: minmax(0, 5fr) minmax(0, 7fr);
            gap: var(--space-8);
            align-items: center;
          }
          .chapter--flip {
            grid-template-columns: minmax(0, 7fr) minmax(0, 5fr);
          }
          .chapter--flip .text {
            order: 2;
          }
        }
        ```
      - `.features`: change `grid-template-columns: repeat(2, minmax(0, 1fr));` to `grid-template-columns: minmax(0, 1fr);`, and delete the now-identical `@media (max-width: 720px) { .features { … } }` block.
      - Leave the `.visual` rules as they are.
3. **`Chapters.astro`:**
   1. Add `flip` to the 2nd, 4th and 6th `<Chapter>` (`ch-explore`, `ch-evaluate`, `ch-alert`).
   2. Every `<Frame>` gets `sizes="(max-width: 720px) 100vw, (min-width: 960px) 700px, 100vw"`.
   3. Only the trace-list frame keeps `fade` (its table runs on). Change `fade` to `fade={false}` on the other five.
   4. Update the trace list's `alt` to: `"The trace list: a histogram of traces over time, and rows with start time, name, input and output."` (the filter column is no longer in the picture). Leave the other alts.
4. **`Hero.astro`:**
   1. The `<Frame>` gets `sizes="(max-width: 720px) 100vw, (min-width: 1200px) 700px, 100vw"` and `fade={false}`. Keep `bleed` and `eager`.
   2. CSS: add
      ```css
      @media (min-width: 1200px) {
        .hero {
          grid-template-columns: minmax(0, 5fr) minmax(0, 6fr);
          gap: var(--space-8);
          align-items: center;
        }
        .visual {
          margin-top: 0;
        }
        .hero-title {
          font-size: clamp(2.75rem, 1rem + 3.2vw, 4rem);
        }
      }
      ```
      Place it after the existing `.visual` rule, so it wins.

**Tests:** by hand, on `pnpm --dir website exec astro dev` (or the build's `dist/`), with the browser at 1440×900, 1024×768 and 390×844:
- 1440: the hero has text on the left and the trace panel on the right. The title is on two lines plus "In a minute." and doesn't overflow its column. The chapters alternate picture right, left, right, left, right, left. No picture is wider than its CSS size, so none is blurry.
- 1024: the hero is one column, with the picture under the text. Chapters are two columns.
- 390: everything stacks. Each picture scrolls sideways inside its frame. Nothing scrolls the page sideways.
- `pnpm --dir website exec astro check` reports no new error.

**Done when:** `just check-docs` passes (it ends with `check-marketing: 16 pages ok`) and the hand checks hold.

**Commit:** `Put the homepage text beside its screenshots`
