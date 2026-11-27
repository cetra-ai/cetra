# M15: Story screenshots and the story switcher

Every homepage picture follows one of four stories (Support agent, Coding agent, Voice agent, Document extraction; design §12, D-W-24, D-W-25). M14 gave each story its own prompt, dashboard, annotation queue and assistant chat in the web mocks. M15 takes 24 chapter screenshots (six chapters times four stories) and builds a page-wide switcher: the hero's tabs and a chip row above every chapter picture are one picker, so picking a story anywhere changes every picture, and `?story=` keeps the choice in the URL. It is three phases:
- Q0 lets a screenshot entry set `localStorage` keys, so the assistant shot can show a taller window.
- Q1 gives `website/screenshots.json` the 24 chapter entries and commits their PNGs.
- Q2 adds the stories data module, the `StoryPicture` component, rewires the hero and the chapters, and corrects every alt against its PNG.

**Prerequisites:** branch `claude/focused-screenshots` at or after the commit that adds this plan; M14 done (`docs/plans/website/14-m14-prompts-dashboards-queues-assistant.md`; the story mocks are registered in `web/src/mocks/browser.ts`, commit `90465e1` and its review `093af2f`).

**Read first:**
- `docs/website/00-design.md` §12.
- `docs/plans/website/README.md` § Conventions for every phase. They apply here.
- `docs/plans/website/14-m14-prompts-dashboards-queues-assistant.md`: the story objects, their ids and the playground variables (Planner's readings, P1).
- `website/STYLE.md`: the style of every alt and caption.
- `website/README.md` § Screenshots, `website/scripts/screenshots.mjs`, `website/scripts/check-screenshots.mjs`, `website/screenshots.json`.
- `web/src/lib/assistant/panel.ts` (`WINDOW_KEY`, `readWindowRect`, `fitWindow`) and `web/src/components/assistant/assistant-window.tsx`: what Q0's `storage` map sets for the assistant window.
- `website/src/marketing/home/Hero.astro`, `Chapters.astro`, `website/src/marketing/components/Frame.astro`, `Tabs.astro`, `website/src/marketing/lib/tabs.ts`, `website/src/marketing/home/Chapter.astro`.

## What it is, and what it is not

When it's done:
- **Screenshots.** `screenshots.json` has 24 chapter entries (`marketing-{trace-list,assistant,issue,queue-review,playground,alerts}` and the same with `-coding`, `-voice`, `-extraction`), each with its PNG. All run at the default clock, dark theme, large mock. The assistant shots show a tall window with both turns.
- **Switcher.** The homepage's hero and six chapters each show a picker of the four stories; picking one anywhere selects it everywhere; `?story=<id>` selects on load and is written with `history.replaceState` on change; unknown values fall back to support; only the visible pictures load; without JavaScript the support story shows everywhere and the tabs are inert but visible.
- **Alts.** Every one of the 28 alts was checked against its PNG.

Out of scope:
- Any change under `web/` or `crates/`, to `docs/` other than this plan and the README row, or to any page but the homepage. The four hero shots `marketing-trace-view*` are not retaken.
- `Tabs.astro`, `lib/tabs.ts`, `Chapter.astro`, `Frame.astro` and `base.css` (reused as they are).
- A `CHANGELOG.md` line: the product does not change. **No new dependency.**

While phases run, nobody runs full test suites: each phase runs only what it names. Never `just check`. Q0, Q1 and Q2 run one after the other (Q1's shots need Q0's `storage`; Q2's build needs Q1's PNGs).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-W-12 | Screenshots come from the web UI on its MSW mocks |
| D-W-23 | A marketing shot shows one panel, readable at its size: `viewport` and `clip` |
| D-W-24 | The hero shows four example traces behind tabs; the voice agent replaced the research agent |
| D-W-25 | Every homepage picture follows one of the four stories, through mock objects tied to the story's hero trace |

The user's answers: the story pickers are synced chips per picture (one choice for the whole page); captions state the outcome; the four stories are support, coding, voice, extraction (in that order, support is the default).

### Planner's readings

Choices that the fixed content below does not state. They come from the phase drafts and are collected here so you can see them.

Q0:
- `storage` values are written in the same init script that clears storage and sets the theme, after both, so a story may override `cetra.theme` but no shot does. The init script runs again on every navigation of the page, which is what the per-shot clear already does.
- Validation lives in `screenshots.mjs` itself (it has no importable parts: it runs on import), runs for every selected shot before the dev server starts, and throws.

Q1:
- The four trace-list entries add `cols=start,name,input,duration,cost` (the URL lists the visible columns), so the Duration and Cost columns fit the 800 px clip.
- The voice trace-list entry waits for the filter chip text, not for a trace name: the hero `voice-agent` (2.8 s) is not among the top rows, voice traces last up to 4 s.
- The coding trace list shows the `coding-agent` row second (a generated `memory-update` error is newer); it is not the top row.
- The old action that clicks the "Filters" toggle closes the facet panel (open by default with empty storage) so the filter chip shows; all four trace-list entries keep it.
- Queue-review shots clip `main`, showing the trace beside the score form (the old clip showed the form alone).
- The four playground shots use one window, 1280x640, so the pictures have the same shape in the switcher. The alerts shots use 1200x560, clip `main`; `History` is the `waitAfter` (the text exists although the panel does not show it at this height).
- The assistant shot does not set `cetra.assistant.open`: its entry clicks "Ask AI", which toggles the panel open; storing `"1"` would make that click close it. It sets only `cetra.assistant.window`, to a rect that fits the 1200x760 viewport (`fitWindow` keeps a window within the viewport less a 16 px margin on each side, so at most 1168x728). The starting value is `{"x":544,"y":16,"width":640,"height":728}`; the implementer tunes it so the window shows the first question, its answer, the build request and the draft card, and reports the value used.

Q2:
- `stories.mjs` holds names, alts, captions and Frame labels only (plain strings, testable by `node --test`); `StoryPicture.astro` turns a shot name into an image with `import.meta.glob`, because only a bundler import gives Astro's image pipeline an `ImageMetadata`.
- `Tabs.astro` stays the markup and the keyboard/ARIA layer; `StoryPicture` renders one `Tabs` per picture and a script keeps the pickers in step through a `MutationObserver` on `aria-selected`.
- The chips reuse the `.tablist` style of the hero's tabs; there is no separate chip style.
- Chapter Frame labels stay the same for every story (`Traces`, `Assistant`, `Issues`, `Annotation queue`, `Playground`, `Alerts`); only the hero's four differ, as today.
- Alignment: a chapter's picture sits at the start (text on the left) or the end (`flip`) at 960 px and wider, so its chip row and caption follow it; the hero and phones are centred.
- Names in the alts: the coding hero trace is `coding-agent`; its service and its alert's name are `code-agent`.

## Versions

No new dependency. Playwright (`playwright` in `website/package.json`) and Astro are already pinned; Q0 to Q2 use the installed versions. The implementer reports them.

## The fixed content

Decided by the planner. Every phase uses exactly this; "the content" in the phases means this section.

Stories, in order: support, coding, voice, extraction (ids used in `?story=`): labels "Support agent", "Coding agent", "Voice agent", "Document extraction". Default (no JS, or no/unknown `?story=`): support.

### Shots (one per story per chapter; support keeps today's names, others add -coding, -voice, -extraction)

| Chapter (Chapters.astro) | Shot base name | Support | Coding | Voice | Extraction |
|---|---|---|---|---|---|
| Trace | marketing-trace-list | `/acme/support-bot/traces?filter=tags @> ['email']&cols=start,name,input,duration,cost` | `…?filter=status = 'error'&cols=…` | `…?filter=tags @> ['voice']&sort=duration&order=desc&cols=…` | `…?filter=tags @> ['billing']&cols=…` |
| Explore (assistant) | marketing-assistant | dashboard `…570401` behind, assistant window open on stored conversation `…570801` | `…570402` / `…570802` | `…570403` / `…570803` | `…570404` / `…570804` |
| Issues | marketing-issue | `/acme/support-bot/issues/01900000-0000-7000-8000-000000570101` | `…570102` | `…570103` | `…570104` |
| Evaluate (queue) | marketing-queue-review | review of queue `…570501` item `…570511` | `…570502`/`…570512` | `…570503`/`…570513` | `…570504`/`…570514` |
| Prompts (playground) | marketing-playground | `?prompt=support-reply&version=2`, variables typed, Run all | `?prompt=write-patch&version=3` | `?prompt=voice-reply&version=4` | `?prompt=extract-invoice&version=5`, a second column with model claude-sonnet-4-5, Run all |
| Alert | marketing-alerts | `/acme/support-bot/alerts?rule=01900000-0000-7000-8000-000000570201` | `…570202` | `…570203` | `…570204` |

`cols=…` stands for `cols=start,name,input,duration,cost`. All chapter shots run at the default clock (2026-11-19T12:00:00Z, no `now`), dark theme, large mock. The existing marketing-* shots that used `now: 2026-01-01…` (alerts, playground, queue-review, issue) drop `now` because the story objects are dated against the story clock. Viewport and clip per chapter as today unless the page needs more room (the assistant shot shows the dashboard behind the window, so its clip is the whole viewport, 1200x760).

Variables to type in the playground (one value per variable; the template's variable names are in `web/src/mocks/stories-prompts.ts` and the support-reply fixture):
- support-reply v2: product "cetra", language "English", question "How can I export my traces to a CSV file?"
- write-patch v3: issue "Fields that contain a comma break the CSV columns (#412).", path "src/export/csv.py", code "def escape_field(value): return value.replace('\"', '\"\"')"
- voice-reply v4: caller "Hi, can I move my dentist appointment to Friday afternoon?", slots "14:00, 16:30"
- extract-invoice v5: ocr_text "NORTHWIND SUPPLIES · Invoice INV-2026-0412 · Total 1,284.00 EUR"

### Captions (one line under each picture)

| Chapter | Support | Coding | Voice | Extraction |
|---|---|---|---|---|
| Hero | See what each step of a support answer cost and how long it took. | Find the failed test run, and the fix that followed, in one trace. | Find the one slow tool call behind a reply that started 2.8 s late. | See the cheap model fail the check and the strong one fix it, with the cost of each. |
| Trace | Every support conversation, with its cost and latency. | Only the failed runs, so nothing broken hides in the traffic. | The slowest calls first, so you fix what callers feel. | Every invoice the pipeline read, with what each one cost. |
| Explore | Ask which answers scored low, then get a quality dashboard built for you. | Ask what failed, then get a dashboard for the coding agent built for you. | Ask which calls were slow, then get a latency dashboard built for you. | Ask which invoices needed a retry, then get a dashboard built for you. |
| Issues | What customers ask most, grouped for you, with real examples. | The same test failure across runs becomes one issue to fix. | Slow replies grouped into one issue, with the trace that shows why. | Wrong totals grouped into one issue, before they reach your books. |
| Evaluate | Reviewers score real answers in a queue, next to the trace. | Review the agent's patches the way you review a pull request. | Check whether each caller got what they asked for. | Check the extracted fields against the invoice, one by one. |
| Prompts | Try a prompt change on a real question before you ship it. | Run the patch prompt on a real issue and see the diff. | Tune the receptionist prompt for short spoken answers. | Compare a cheap and a strong model on the same invoice, with the cost of each. |
| Alert | Know when answer quality drops below your bar. | Know when the coding agent's error rate climbs, in Slack. | Know when callers start waiting, before they hang up. | Know when the schema check pass rate slips. |

Alts: one sentence each, "The <page> …: <what the picture shows for this story>." They describe the picture, not sell, in the style of `website/STYLE.md`.

### Switcher

- One data module `website/src/marketing/data/stories.mjs` (`.mjs`, like the site's other tested data modules) listing the stories and, per story and per picture (hero + 6 chapters), the screenshot name, alt, caption and the Frame label.
- The hero's Tabs become the story picker; each chapter shows a row of story chips (same four labels) above its picture. Picking a story anywhere selects it everywhere: every hero tab and chapter chip row updates, every picture switches.
- `?story=<id>` in the URL selects on load and is written with `history.replaceState` on change (no new history entries). Unknown values fall back to support.
- Only the visible pictures load: pictures of other stories are in hidden panels with lazy images (the hero's first picture stays eager).
- Keyboard and ARIA as the existing Tabs (`lib/tabs.ts`) — reuse it; chips are a tablist per chapter.
- No JS: the support story shows everywhere and the chips/tabs are inert but visible.

## Phases

| Phase | Edit | Depends on |
|---|---|---|
| Q0 A storage map for screenshot entries | `website/scripts/screenshots.mjs`, `website/README.md` | — |
| Q1 Story screenshots | `website/screenshots.json`; `website/src/assets/screenshots/marketing-{trace-list,assistant,issue,queue-review,playground,alerts}{,-coding,-voice,-extraction}.png` (24 files) | Q0 |
| Q2 The story switcher | `website/src/marketing/data/stories.mjs` (new), `website/src/marketing/data/stories.test.mjs` (new), `website/src/marketing/components/StoryPicture.astro` (new), `website/src/marketing/home/Hero.astro`, `website/src/marketing/home/Chapters.astro`, `docs/plans/website/README.md` (the M15 row) | Q1 |

The phases are sequential; no parallel runs, no shared files. Setup for every phase, from the worktree root: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`; in a fresh worktree `pnpm install --frozen-lockfile` and `pnpm --dir web run gen:routes` (`web/src/routeTree.gen.ts` is git-ignored); Chromium once: `pnpm --dir website exec playwright install chromium`. Biome for website files: `cd website && ../node_modules/.bin/biome check --write <paths>`, never from the root. Biome's formatting wins over code quoted in this plan; behaviour and strings stay as quoted. The screenshot port 5199 may be taken by another worktree: use `CETRA_SCREENSHOTS_PORT=5299`. `scripts/screenshots.mjs` needs the `target/debug/cetra` symlink only for e2e, not for screenshots.

---

## Q0 A storage map for screenshot entries

**Goal:** a screenshot entry may set `localStorage` keys before its page loads, so a shot can start from a chosen UI state.

**Edit:** `website/scripts/screenshots.mjs`, `website/README.md`. Nothing else. (No test file exists for `screenshots.mjs`: the script runs on import, reads `screenshots.json` and starts a server, so it has nothing a `node --test` file can import. `check-screenshots.mjs` and its test only compare shot names with PNG files and do not change.)

**Facts checked against the code.**
- `shoot()` in `screenshots.mjs` clears storage and sets the theme with one `page.addInitScript((value) => { localStorage.clear(); localStorage.setItem("cetra.theme", value); }, theme)`. An init script runs in every document the page loads, before the page's own scripts, so the storage is already set when the app first reads it. It runs again on each navigation of that page; each shot opens one page and navigates once.
- The assistant window reads `cetra.assistant.window` when `AssistantWindow` mounts (`readWindowRect` in `web/src/lib/assistant/panel.ts`): a JSON string `{"x":number,"y":number,"width":number,"height":number}`, all finite numbers, fitted to the viewport by `fitWindow` (at least 320x360, at most the viewport less 16 px on each side). A bad value falls back to the default 420x600 window. The panel's open state is `cetra.assistant.open`, `"1"` or `"0"`; the "Ask AI" button toggles it. The window writes its rect back to the key on mount.
- The key prefix is `STORAGE_PREFIX` (`web/src/product.ts`), which is `cetra.`.

**Steps**
1. In `screenshots.mjs`, add above `startServer`:
   ```js
   // A shot's `storage` is a map of localStorage keys to string values, written after the per-shot
   // clear and before the page loads. Throws on anything else, before the server starts.
   function checkStorage(shots) {
     for (const shot of shots) {
       if (shot.storage === undefined) continue;
       const map = shot.storage;
       if (typeof map !== "object" || map === null || Array.isArray(map)) {
         throw new Error(`screenshots.json: shot "${shot.name}": storage must be an object`);
       }
       for (const [key, value] of Object.entries(map)) {
         if (key === "" || typeof value !== "string") {
           throw new Error(
             `screenshots.json: shot "${shot.name}": storage "${key}" must be a non-empty key with a string value`,
           );
         }
       }
     }
   }
   ```
2. Call `checkStorage(selected)` right after the `selected.length` check and before `mkdirSync`/`startServer()`.
3. Replace the `addInitScript` call in `shoot()` by one that passes an object and writes the map after the theme:
   ```js
   await page.addInitScript(
     ({ theme, storage }) => {
       try {
         localStorage.clear();
         localStorage.setItem("cetra.theme", theme);
         for (const [key, value] of Object.entries(storage)) localStorage.setItem(key, value);
       } catch {
         // storage unavailable: the UI's defaults stay
       }
     },
     { theme, storage: shot.storage ?? {} },
   );
   ```
   Update the comment above it ("Each shot starts from empty storage ...; a shot's `storage` then sets its own keys").
4. Update the header comment of `screenshots.mjs` only if it lists entry fields (today it does not; leave it).
5. In `website/README.md` § Screenshots, extend the "To add a shot" bullet's field list with `storage`, and add one bullet after the `clip` bullet:
   ```md
   - A shot starts from empty storage. `storage` (an object of `localStorage` keys to string values) sets keys after that clear and before the page loads, for a state the UI keeps in the browser, for example `"storage": { "cetra.assistant.window": "{\"x\":544,\"y\":16,\"width\":640,\"height\":728}" }` for a taller assistant window. A value that is not a string fails the run before the server starts.
   ```

**Tests:** no unit test (see Edit). Verify with scratch entries that you remove before the commit:
1. Add to `screenshots.json` a temporary entry `scratch-storage`: `{"name":"scratch-storage","path":"/acme/support-bot/traces","waitFor":"100 loaded","theme":"dark","storage":{"cetra.theme":"light"}}` (the storage map is written after the theme, so the picture must come out light although `theme` is dark). Run `CETRA_SCREENSHOTS_PORT=5299 node scripts/screenshots.mjs scratch-storage` from `website/`, read the PNG with the Read tool and confirm it is light.
2. Change the entry's storage to `{"a": 1}` and run again: it must throw `storage "a" must be a non-empty key with a string value` before any server starts (`port 5299` stays free).
3. Remove the temporary entry and its PNG (`website/src/assets/screenshots/scratch-storage.png`); `git status` shows only the two Edit files.

**Done when:**
- `cd website && ../node_modules/.bin/biome check --write scripts/screenshots.mjs` then without `--write` is clean.
- The three scratch checks above passed and were cleaned up.
- `git status` shows only `website/scripts/screenshots.mjs` and `website/README.md`.

**Commit:** `Let a screenshot entry set localStorage`

---

## Q1 Story screenshots

**Goal:** give `website/screenshots.json` the 24 chapter entries (six chapters x four stories) and commit their PNGs, so the switcher phase can import them.

**Edit:** `website/screenshots.json`; `website/src/assets/screenshots/marketing-{trace-list,assistant,issue,queue-review,playground,alerts}{,-coding,-voice,-extraction}.png` (24 files, the six support ones rewritten). Nothing else.

**Prerequisites:** M14 done (the story mocks are registered in `web/src/mocks/browser.ts`, commit `90465e1`); Q0 committed (`storage`).

### Facts checked against the code (and by a trial run of all 24 on port 5299)

- `screenshots.mjs` supports per entry: `path`, `waitFor` (getByText, first match), `actions` (`click` selector, optional `type`: Ctrl+A then `insertText`, fine on `<input>`), `waitAfter`, `viewport`, `clip` (a selector; none = whole viewport), `now`, `theme`, and (Q0) `storage`. Then it waits for networkidle (so streamed playground runs finish) and fonts.
- No entry sets `now`: the default clock is 2026-11-19T12:00:00Z. The four old entries that pinned 2026-01-01 (alerts, playground, queue-review, issue) lose `now`.
- Trace list: the page reads `filter`, `sort`, `order` and `cols` from the URL. The facet panel is open by default (storage is cleared), so the old action (click the "Filters" toggle, `aria-pressed`) closes it, and the filter chip shows. The URL filter is percent-encoded here.
- The large mock applies `tags @>`, `status =` filters and `sort=duration`. In the voice list the hero `voice-agent` (2.8 s) is NOT among the top rows (voice traces last up to 4 s), so the voice entry waits for the filter chip text, not the trace name. In the coding list `coding-agent` is the second row (a generated `memory-update` error is newer), not the top one.
- Without `cols` the Duration and Cost columns fall outside the 800 px clip, so the old picture could not show what the captions promise ("cost and latency", "slowest first"). All four entries add `cols=start,name,input,duration,cost` (the URL lists the visible columns).
- Dashboard route: `/acme/support-bot/dashboard/<id>`. Assistant: button `Ask AI`; the chat menu is the header button `aria-label="Chats: <title>"` (the label is "New chat" when none is selected); items are `[role=menuitem]` whose text is the chat's title (the first question, e.g. "Which answers scored low today?"). Fresh storage selects no chat, so the entry opens the menu and picks the story's chat. The stored chat shows the draft card ("Open in dashboard editor"). The window defaults to 420x600 (`WINDOW_DEFAULT`), too small for both turns, so each assistant entry sets `cetra.assistant.window` (Q0).
- Issue pages: id in the URL; the title is on the page. Queue review: `/annotation-queues/<queue>/review/<item>`; the clip `main` shows the trace beside the score form (the old clip `section[aria-label="Review"]` showed the form alone).
- Playground: variable inputs are `input[aria-label="<variable name>"]`; "Add model" adds column "Model 2" (`section[aria-label="Model 2"]`) with the model input `input[aria-label="Model"]`; typed `claude-sonnet-4-5` makes the mock answer 1284 (column 1 keeps gemini-2.5-flash and answers 1248). Two columns need a wider window, so all four playground shots use 1280x640.
- Alerts: `?rule=<id>` opens the rule panel beside the list (the panel has the condition, last value and a preview chart; the list keeps "Recent events"). It needs a wide window: 1200x560, clip `main`. `History` is not shown by the panel at this height but the text exists, so it is the `waitAfter`.

### Steps

1. In `website/screenshots.json`, replace the six existing entries `marketing-trace-list`, `marketing-alerts`, `marketing-assistant`, `marketing-playground`, `marketing-queue-review`, `marketing-issue` by the 24 entries below, at the same place (after `marketing-trace-view-extraction`; keep every other entry untouched, including the four `marketing-trace-view*` hero shots). Order: by story (support, coding, voice, extraction), within a story: trace-list, assistant, issue, queue-review, playground, alerts. Run `cd website && ../node_modules/.bin/biome check --write screenshots.json`; keep its formatting.
2. Run `CETRA_SCREENSHOTS_PORT=5299 just docs-screenshots <the 24 names>`; if a shot times out on `waitFor`, the page printed nothing useful: reopen the shot's URL with a dev server (`just web-dev`-like) and fix the selector or wait, only in `screenshots.json`.
3. Tune the assistant window. The entries below start at `{"x":544,"y":16,"width":640,"height":728}` (the viewport is 1200x760; `fitWindow` allows at most 1168x728). Read each `marketing-assistant*.png`: the window must show the first question, its answer, the build request and the draft card with the story's four widget titles and "Open in dashboard editor", with the dashboard visible behind it. If something is cut off, change the rect (same value in all four entries) and recapture. Report the final value.
4. Look at every PNG with the Read tool against the checklist; fix and recapture the wrong ones.
5. Run `CETRA_SCREENSHOTS_PORT=5299 just docs-screenshots` once for all shots (dataset-wide shots can change); `git status` must show only the 24 marketing PNGs plus whichever other PNGs really changed; if another PNG changed, look at it and report why.
6. `just check-docs`.

### The 24 entries

```json
[
  {"name": "marketing-trace-list", "path": "/acme/support-bot/traces?filter=tags%20%40%3E%20%5B'email'%5D&cols=start,name,input,duration,cost", "waitFor": "support-agent", "theme": "dark", "viewport": {"width": 800, "height": 640}, "actions": [{"click": "button[aria-pressed]:has-text(\"Filters\")"}], "clip": "main"},
  {"name": "marketing-assistant", "path": "/acme/support-bot/dashboard/01900000-0000-7000-8000-000000570401", "waitFor": "Correctness by day", "actions": [{"click": "button[aria-label=\"Ask AI\"]"}, {"click": "button[aria-label^=\"Chats:\"]"}, {"click": "[role=\"menuitem\"]:has-text(\"Which answers scored low today?\")"}], "waitAfter": "Open in dashboard editor", "storage": {"cetra.assistant.window": "{\"x\":544,\"y\":16,\"width\":640,\"height\":728}"}, "theme": "dark", "viewport": {"width": 1200, "height": 760}},
  {"name": "marketing-issue", "path": "/acme/support-bot/issues/01900000-0000-7000-8000-000000570101", "waitFor": "Customers ask how to export traces to CSV", "theme": "dark", "viewport": {"width": 800, "height": 600}, "clip": "main"},
  {"name": "marketing-queue-review", "path": "/acme/support-bot/annotation-queues/01900000-0000-7000-8000-000000570501/review/01900000-0000-7000-8000-000000570511", "waitFor": "Complete and next", "theme": "dark", "viewport": {"width": 1200, "height": 640}, "clip": "main"},
  {"name": "marketing-playground", "path": "/acme/support-bot/playground?prompt=support-reply&version=2", "waitFor": "Run all", "actions": [{"click": "input[aria-label=\"product\"]", "type": "cetra"}, {"click": "input[aria-label=\"language\"]", "type": "English"}, {"click": "input[aria-label=\"question\"]", "type": "How can I export my traces to a CSV file?"}, {"click": "button:has-text('Run all')"}], "waitAfter": "Use the API for ranges", "theme": "dark", "viewport": {"width": 1280, "height": 640}, "clip": "main"},
  {"name": "marketing-alerts", "path": "/acme/support-bot/alerts?rule=01900000-0000-7000-8000-000000570201", "waitFor": "Answer correctness under 0.7 on support-bot", "waitAfter": "History", "theme": "dark", "viewport": {"width": 1200, "height": 560}, "clip": "main"},
  {"name": "marketing-trace-list-coding", "path": "/acme/support-bot/traces?filter=status%20%3D%20'error'&cols=start,name,input,duration,cost", "waitFor": "coding-agent", "theme": "dark", "viewport": {"width": 800, "height": 640}, "actions": [{"click": "button[aria-pressed]:has-text(\"Filters\")"}], "clip": "main"},
  {"name": "marketing-assistant-coding", "path": "/acme/support-bot/dashboard/01900000-0000-7000-8000-000000570402", "waitFor": "Failed runs per day", "actions": [{"click": "button[aria-label=\"Ask AI\"]"}, {"click": "button[aria-label^=\"Chats:\"]"}, {"click": "[role=\"menuitem\"]:has-text(\"Which traces failed in the last day?\")"}], "waitAfter": "Open in dashboard editor", "storage": {"cetra.assistant.window": "{\"x\":544,\"y\":16,\"width\":640,\"height\":728}"}, "theme": "dark", "viewport": {"width": 1200, "height": 760}},
  {"name": "marketing-issue-coding", "path": "/acme/support-bot/issues/01900000-0000-7000-8000-000000570102", "waitFor": "run_tests failed: 1 failed, 47 passed", "theme": "dark", "viewport": {"width": 800, "height": 600}, "clip": "main"},
  {"name": "marketing-queue-review-coding", "path": "/acme/support-bot/annotation-queues/01900000-0000-7000-8000-000000570502/review/01900000-0000-7000-8000-000000570512", "waitFor": "Complete and next", "theme": "dark", "viewport": {"width": 1200, "height": 640}, "clip": "main"},
  {"name": "marketing-playground-coding", "path": "/acme/support-bot/playground?prompt=write-patch&version=3", "waitFor": "Run all", "actions": [{"click": "input[aria-label=\"issue\"]", "type": "Fields that contain a comma break the CSV columns (#412)."}, {"click": "input[aria-label=\"path\"]", "type": "src/export/csv.py"}, {"click": "input[aria-label=\"code\"]", "type": "def escape_field(value): return value.replace('\"', '\"\"')"}, {"click": "button:has-text('Run all')"}], "waitAfter": "needs_quotes", "theme": "dark", "viewport": {"width": 1280, "height": 640}, "clip": "main"},
  {"name": "marketing-alerts-coding", "path": "/acme/support-bot/alerts?rule=01900000-0000-7000-8000-000000570202", "waitFor": "Error rate above 5% on code-agent", "waitAfter": "History", "theme": "dark", "viewport": {"width": 1200, "height": 560}, "clip": "main"},
  {"name": "marketing-trace-list-voice", "path": "/acme/support-bot/traces?filter=tags%20%40%3E%20%5B'voice'%5D&sort=duration&order=desc&cols=start,name,input,duration,cost", "waitFor": "tags @> ['voice']", "theme": "dark", "viewport": {"width": 800, "height": 640}, "actions": [{"click": "button[aria-pressed]:has-text(\"Filters\")"}], "clip": "main"},
  {"name": "marketing-assistant-voice", "path": "/acme/support-bot/dashboard/01900000-0000-7000-8000-000000570403", "waitFor": "p95 reply time", "actions": [{"click": "button[aria-label=\"Ask AI\"]"}, {"click": "button[aria-label^=\"Chats:\"]"}, {"click": "[role=\"menuitem\"]:has-text(\"Which calls were slow today?\")"}], "waitAfter": "Open in dashboard editor", "storage": {"cetra.assistant.window": "{\"x\":544,\"y\":16,\"width\":640,\"height\":728}"}, "theme": "dark", "viewport": {"width": 1200, "height": 760}},
  {"name": "marketing-issue-voice", "path": "/acme/support-bot/issues/01900000-0000-7000-8000-000000570103", "waitFor": "Reply started more than 800 ms after the caller", "theme": "dark", "viewport": {"width": 800, "height": 600}, "clip": "main"},
  {"name": "marketing-queue-review-voice", "path": "/acme/support-bot/annotation-queues/01900000-0000-7000-8000-000000570503/review/01900000-0000-7000-8000-000000570513", "waitFor": "Complete and next", "theme": "dark", "viewport": {"width": 1200, "height": 640}, "clip": "main"},
  {"name": "marketing-playground-voice", "path": "/acme/support-bot/playground?prompt=voice-reply&version=4", "waitFor": "Run all", "actions": [{"click": "input[aria-label=\"caller\"]", "type": "Hi, can I move my dentist appointment to Friday afternoon?"}, {"click": "input[aria-label=\"slots\"]", "type": "14:00, 16:30"}, {"click": "button:has-text('Run all')"}], "waitAfter": "Which one works for you?", "theme": "dark", "viewport": {"width": 1280, "height": 640}, "clip": "main"},
  {"name": "marketing-alerts-voice", "path": "/acme/support-bot/alerts?rule=01900000-0000-7000-8000-000000570203", "waitFor": "Slow replies on voice-agent", "waitAfter": "History", "theme": "dark", "viewport": {"width": 1200, "height": 560}, "clip": "main"},
  {"name": "marketing-trace-list-extraction", "path": "/acme/support-bot/traces?filter=tags%20%40%3E%20%5B'billing'%5D&cols=start,name,input,duration,cost", "waitFor": "extract-invoice", "theme": "dark", "viewport": {"width": 800, "height": 640}, "actions": [{"click": "button[aria-pressed]:has-text(\"Filters\")"}], "clip": "main"},
  {"name": "marketing-assistant-extraction", "path": "/acme/support-bot/dashboard/01900000-0000-7000-8000-000000570404", "waitFor": "Invoices per day", "actions": [{"click": "button[aria-label=\"Ask AI\"]"}, {"click": "button[aria-label^=\"Chats:\"]"}, {"click": "[role=\"menuitem\"]:has-text(\"Which invoices needed a retry?\")"}], "waitAfter": "Open in dashboard editor", "storage": {"cetra.assistant.window": "{\"x\":544,\"y\":16,\"width\":640,\"height\":728}"}, "theme": "dark", "viewport": {"width": 1200, "height": 760}},
  {"name": "marketing-issue-extraction", "path": "/acme/support-bot/issues/01900000-0000-7000-8000-000000570104", "waitFor": "Invoice total doesn't match its line items", "theme": "dark", "viewport": {"width": 800, "height": 600}, "clip": "main"},
  {"name": "marketing-queue-review-extraction", "path": "/acme/support-bot/annotation-queues/01900000-0000-7000-8000-000000570504/review/01900000-0000-7000-8000-000000570514", "waitFor": "Complete and next", "theme": "dark", "viewport": {"width": 1200, "height": 640}, "clip": "main"},
  {"name": "marketing-playground-extraction", "path": "/acme/support-bot/playground?prompt=extract-invoice&version=5", "waitFor": "Run all", "actions": [{"click": "input[aria-label=\"ocr_text\"]", "type": "NORTHWIND SUPPLIES · Invoice INV-2026-0412 · Total 1,284.00 EUR"}, {"click": "button:has-text('Add model')"}, {"click": "section[aria-label=\"Model 2\"] input[aria-label=\"Model\"]", "type": "claude-sonnet-4-5"}, {"click": "button:has-text('Run all')"}], "waitAfter": "1284", "theme": "dark", "viewport": {"width": 1280, "height": 640}, "clip": "main"},
  {"name": "marketing-alerts-extraction", "path": "/acme/support-bot/alerts?rule=01900000-0000-7000-8000-000000570204", "waitFor": "Schema check pass rate under 97%", "waitAfter": "History", "theme": "dark", "viewport": {"width": 1200, "height": 560}, "clip": "main"}
]
```

### Tests

No unit tests. `website/scripts/check-screenshots.mjs` checks each entry has a PNG and each PNG an entry; it does not reject PNGs the site does not import, so the new PNGs may be unused until the switcher lands.

### Per-shot checklist (what must be visible; no error or empty state, nothing cut off)

- trace-list (x4): filter chip with the story's filter; Duration and Cost columns with values; rows present. Support: `support-agent` first row, CSV question. Coding: red error rows and the `coding-agent` row in the top few (second). Voice: DUR column sorted descending with 3.9x s at the top. Extraction: `extract-invoice` rows.
- assistant (x4): the story's dashboard behind (its four widget titles, charts or numbers drawn, not skeletons or "Dashboard not found"); the assistant window open with the story's chat title, both turns (the first question and its answer, the build request, the "Drafted a dashboard" step), the draft card with the story's four widget titles and "Open in dashboard editor". The composer chips show "page: traces" and a raw dashboard id; that is the product's own text.
- issue (x4): the story's title (Customers ask how to export traces to CSV / run_tests failed: 1 failed, 47 passed / Reply started more than 800 ms after the caller / Invoice total doesn't match its line items), the counts, the chart, Examples.
- queue-review (x4): queue name in the breadcrumb, the hero trace (`support-agent`, `coding-agent`, `voice-agent`, `extract-invoice`) beside the form with the story's score fields, "Complete and next".
- playground (x4): "From prompt <name> v<n>"; typed variable values; the story's answer (support: export steps; coding: the diff; voice: "Sure. On Friday afternoon..."; extraction: two columns, `"total": 1248` under gemini-2.5-flash and `1284` under claude-sonnet-4-5); metrics and "Open trace".
- alerts (x4): the rule name highlighted in the list and in the panel header, condition, last value, preview chart with the threshold line.

Trial results, seen by the drafter (before Q0): trace-list support/coding/voice, assistant support/voice/extraction (window too small), issue support, queue-review support/voice, playground support?/coding/extraction, alerts support/coding were fine. Not looked at: the other issue, queue, coding/voice assistant, voice alerts, extraction trace list; the implementer looks at all 24.

**Done when:** `just check-docs` passes; the 24 PNGs exist and match the checklist; `git status` shows only `website/screenshots.json` and PNGs under `website/src/assets/screenshots/`; the report states the final assistant window value.

**Commit:** `Capture every chapter screenshot for each story`

---

## Q2 The story switcher

**Goal:** every homepage picture (the hero and the six chapters) is a four-story picker whose choice is shared across the page and kept in `?story=`.

**Prerequisite:** Q1 has added the 24 chapter shots to `website/screenshots.json` and their PNGs to `website/src/assets/screenshots/`. The four hero shots `marketing-trace-view`, `-coding`, `-voice`, `-extraction` exist today. Until the PNGs exist, the data test (case 3) and the build fail.

**Edit:** (nothing else may change)
- `website/src/marketing/data/stories.mjs` (new)
- `website/src/marketing/data/stories.test.mjs` (new)
- `website/src/marketing/components/StoryPicture.astro` (new)
- `website/src/marketing/home/Hero.astro`
- `website/src/marketing/home/Chapters.astro`
- `docs/plans/website/README.md` (the M15 row, last step)

Not edited, and why: `Tabs.astro` and `lib/tabs.ts` are reused as they are; `Chapter.astro` needs no change (its `.visual--bare` and `.visual :global(.frame)` rules already apply to a Frame inside a slot, at any depth); `Frame.astro` stays (eager only for the hero's first picture); `base.css` stays (the `.tablist` look is reused for the chips).

### How the work splits (decisions of this phase, none is a product decision)

- Astro's image pipeline needs an `ImageMetadata` object, which only a bundler import gives (Frame reads `src.width`). A `.mjs` read by `node --test` cannot import a PNG. So **`stories.mjs` holds names, alts, captions and Frame labels only** (plain strings, testable by `node --test`), and **`StoryPicture.astro` turns a shot name into an image** with `import.meta.glob("../../assets/screenshots/marketing-*.png", { eager: true, import: "default" })`. A missing shot throws at build time.
- The existing `Tabs.astro` stays the markup and the keyboard/ARIA layer (it calls `wireTabs` for every `[data-tabs]`, sets panel ids, `role=tabpanel`, `aria-labelledby`). `StoryPicture` renders one `Tabs` per picture, wrapped in `<div data-story-picker>`. Its one script only keeps the pickers in step. It never duplicates the ARIA code.
- The script learns of a change from a `MutationObserver` on every tab's `aria-selected`, not from `click`: `wireTabs` handles arrow keys, Home and End without a click event, and the observer catches clicks and keys alike. To switch the other pickers it calls `tab.click()` on their tab, which runs `wireTabs`' own handler. The script is idempotent (`current` guard), so the clicks it causes do nothing more.
- The script runs in a `DOMContentLoaded` handler, because it needs `Tabs.astro`'s listeners (from another bundled script) to exist before it clicks. Module scripts all run before `DOMContentLoaded`.
- `?story=` is written with `history.replaceState` (no history entry), keeping the other query parameters and the hash. Loading with `?story=` does not rewrite the URL. An unknown value changes nothing (support stays) and the URL is left alone.
- No JavaScript: the server renders support selected, panels 2 to 4 `hidden`, and the tabs inert but visible. Pictures of hidden panels are `loading="lazy"` (Frame's default); `display:none` images do not load, and when a switch shows them the browser loads the ones in or near the viewport.
- Alignment: a chapter's picture sits at the start (text on the left) or the end (`flip`) at 960 px and wider, so its chip row and caption follow it (`align` prop); the hero and phones are centred, as the hero's tabs are today.
- Page weight (`check-marketing`: 350 000 bytes of HTML, CSS and JS; images are not counted): the home page's HTML is 63.6 KB today, 28 `<img>` with srcset add about 15 KB, the script under 1 KB. No problem.
- Frame labels: hero keeps today's four (`Traces · Support bot`, `Traces · Coding agent`, `Traces · Voice agent`, `Traces · Document pipeline`); each chapter keeps today's one label (`Traces`, `Assistant`, `Issues`, `Annotation queue`, `Playground`, `Alerts`) for all stories.

### Steps

#### 1. `website/src/marketing/data/stories.mjs` (new), in full

The trace-list alts name the columns the shots now show (`cols=start,name,input,duration,cost`), and the coding one does not claim the failed run is on top (it is the second row). Step 7 corrects every alt against its PNG.

```js
// The homepage's four stories (design §12, D-W-24, D-W-25): for each of the seven pictures (the
// hero and the six product chapters), the screenshot's name, its Frame label, its alt and its
// one-line caption. The names are the shots of website/screenshots.json; StoryPicture.astro turns
// a name into an image, because only a bundler import gives Astro's image pipeline its metadata.
// stories.test.mjs checks the shapes, the names and the words.

/**
 * @typedef {object} Story
 * @property {string} id    the `?story=` value
 * @property {string} label the tab's and chip's text
 */

/** In the order they are shown. The first is the default (no JavaScript, or no or an unknown `?story=`). */
export const STORIES = /** @type {const} */ ([
  { id: "support", label: "Support agent" },
  { id: "coding", label: "Coding agent" },
  { id: "voice", label: "Voice agent" },
  { id: "extraction", label: "Document extraction" },
]);

export const DEFAULT_STORY = STORIES[0].id;

/** The suffix a story adds to a shot's base name; support keeps the bare name. */
const SUFFIX = { support: "", coding: "-coding", voice: "-voice", extraction: "-extraction" };

/**
 * @typedef {object} PictureContent
 * @property {string} alt     what the picture shows, one sentence, for screen readers
 * @property {string} caption one line under the picture
 */

/**
 * @typedef {object} Picture
 * @property {string} id      "hero" or the chapter's id without "ch-"
 * @property {string} section the page's name, for the picker's accessible name
 * @property {string} base    the shot's base name in screenshots.json
 * @property {string | Record<string, string>} label the Frame's title-bar text; one per story when a record
 * @property {Record<string, PictureContent>} stories keyed by story id
 */

/** @type {Picture[]} */
export const PICTURES = [
  {
    id: "hero",
    section: "Trace view",
    base: "marketing-trace-view",
    label: {
      support: "Traces · Support bot",
      coding: "Traces · Coding agent",
      voice: "Traces · Voice agent",
      extraction: "Traces · Document pipeline",
    },
    stories: {
      support: {
        alt: "The cetra trace view: a support agent's trace, with its span tree, timeline, models, tokens and costs, and the root span's input and output.",
        caption: "See what each step of a support answer cost and how long it took.",
      },
      coding: {
        alt: "The cetra trace view: a coding agent's trace, where the first test run fails and the second passes after a fix, with its timeline, models, tokens and costs.",
        caption: "Find the failed test run, and the fix that followed, in one trace.",
      },
      voice: {
        alt: "The cetra trace view: a voice agent's trace for one caller turn, where a slow availability check makes the reply start 2.8 s after the caller stops talking, with its timeline, models and costs.",
        caption: "Find the one slow tool call behind a reply that started 2.8 s late.",
      },
      extraction: {
        alt: "The cetra trace view: an invoice extraction pipeline, where a small model's answer fails the schema check and a larger model reads the invoice again.",
        caption: "See the cheap model fail the check and the strong one fix it, with the cost of each.",
      },
    },
  },
  {
    id: "trace",
    section: "Trace list",
    base: "marketing-trace-list",
    label: "Traces",
    stories: {
      support: {
        alt: "The trace list filtered to traces tagged email: rows of support-agent traces with start time, name, input, duration and cost.",
        caption: "Every support conversation, with its cost and latency.",
      },
      coding: {
        alt: "The trace list filtered to traces with status error: failed runs, among them a coding-agent trace, with start time, name, input, duration and cost.",
        caption: "Only the failed runs, so nothing broken hides in the traffic.",
      },
      voice: {
        alt: "The trace list filtered to voice traces and sorted by duration, longest first: voice-agent traces with start time, name, input, duration and cost.",
        caption: "The slowest calls first, so you fix what callers feel.",
      },
      extraction: {
        alt: "The trace list filtered to traces tagged billing: extract-invoice traces with start time, name, input, duration and cost.",
        caption: "Every invoice the pipeline read, with what each one cost.",
      },
    },
  },
  {
    id: "explore",
    section: "Assistant",
    base: "marketing-assistant",
    label: "Assistant",
    stories: {
      support: {
        alt: "The assistant's window open over a dashboard: the question about answers that scored low, its answer, and the drafted dashboard of answer quality.",
        caption: "Ask which answers scored low, then get a quality dashboard built for you.",
      },
      coding: {
        alt: "The assistant's window open over a dashboard: the question about failed traces, an answer that names coding-agent, and the drafted dashboard of failed runs, cost per run and p95 duration.",
        caption: "Ask what failed, then get a dashboard for the coding agent built for you.",
      },
      voice: {
        alt: "The assistant's window open over a dashboard: the question about slow calls, its answer, and the drafted dashboard of voice latency.",
        caption: "Ask which calls were slow, then get a latency dashboard built for you.",
      },
      extraction: {
        alt: "The assistant's window open over a dashboard: the question about invoices that needed a retry, its answer, and the drafted dashboard of invoices, schema-check failures and cost.",
        caption: "Ask which invoices needed a retry, then get a dashboard built for you.",
      },
    },
  },
  {
    id: "issues",
    section: "Issues",
    base: "marketing-issue",
    label: "Issues",
    stories: {
      support: {
        alt: "An issue page for the question customers ask most, how to export traces to CSV: its state and actions, a daily chart of occurrences, and the support trace as its first example.",
        caption: "What customers ask most, grouped for you, with real examples.",
      },
      coding: {
        alt: "An issue page for the failed run_tests step, one failed and 47 passed: its state and actions, a daily chart of occurrences, and the run_tests span of the coding-agent trace as its example.",
        caption: "The same test failure across runs becomes one issue to fix.",
      },
      voice: {
        alt: "An issue page for replies that started more than 800 ms after the caller: its state and actions, a daily chart of occurrences, and the check_availability span of the voice-agent trace as its example.",
        caption: "Slow replies grouped into one issue, with the trace that shows why.",
      },
      extraction: {
        alt: "An issue page for invoice totals that do not match their line items: its state and actions, a daily chart of occurrences, and the first schema check of the extraction trace as its example.",
        caption: "Wrong totals grouped into one issue, before they reach your books.",
      },
    },
  },
  {
    id: "evaluate",
    section: "Annotation queue",
    base: "marketing-queue-review",
    label: "Annotation queue",
    stories: {
      support: {
        alt: "An annotation queue's review form for support answers: the progress, the instructions and the correctness and relevance score fields, beside the trace.",
        caption: "Reviewers score real answers in a queue, next to the trace.",
      },
      coding: {
        alt: "An annotation queue's review form for patches: the progress, the instructions and the tests-pass and review-verdict fields, beside the coding-agent trace.",
        caption: "Review the agent's patches the way you review a pull request.",
      },
      voice: {
        alt: "An annotation queue's review form for calls: the progress, the instructions and the resolved and tone fields, beside the voice-agent trace.",
        caption: "Check whether each caller got what they asked for.",
      },
      extraction: {
        alt: "An annotation queue's review form for invoices: the progress, the instructions and the schema-valid and fields-correct fields, beside the extraction trace.",
        caption: "Check the extracted fields against the invoice, one by one.",
      },
    },
  },
  {
    id: "prompts",
    section: "Playground",
    base: "marketing-playground",
    label: "Playground",
    stories: {
      support: {
        alt: "The playground with support-reply version 2: its messages, the variables filled in for a question about exporting traces to CSV, and the model's answer with its tokens, cost and latency.",
        caption: "Try a prompt change on a real question before you ship it.",
      },
      coding: {
        alt: "The playground with write-patch version 3: its messages, the variables filled in for a CSV bug, and the model's answer, a diff, with its tokens, cost and latency.",
        caption: "Run the patch prompt on a real issue and see the diff.",
      },
      voice: {
        alt: "The playground with voice-reply version 4: its messages, the caller's request to move an appointment, and the model's short spoken answer with its tokens, cost and latency.",
        caption: "Tune the receptionist prompt for short spoken answers.",
      },
      extraction: {
        alt: "The playground with extract-invoice version 5 in two columns, a cheaper model and claude-sonnet-4-5 on the same invoice, each with its answer, tokens, cost and latency.",
        caption: "Compare a cheap and a strong model on the same invoice, with the cost of each.",
      },
    },
  },
  {
    id: "alert",
    section: "Alerts",
    base: "marketing-alerts",
    label: "Alerts",
    stories: {
      support: {
        alt: "The alerts page with the rule for an average correctness score under 0.7 on support-bot selected, and its recent alert events.",
        caption: "Know when answer quality drops below your bar.",
      },
      coding: {
        alt: "The alerts page with the rule for an error rate above 5% on code-agent selected and firing, and its recent alert events.",
        caption: "Know when the coding agent's error rate climbs, in Slack.",
      },
      voice: {
        alt: "The alerts page with the rule for a p95 latency above 1.5 s on voice-agent selected and firing, and its recent alert events.",
        caption: "Know when callers start waiting, before they hang up.",
      },
      extraction: {
        alt: "The alerts page with the rule for a schema-check pass rate under 97% on billing traces selected, and its recent alert events.",
        caption: "Know when the schema check pass rate slips.",
      },
    },
  },
];

/** The shot's name in screenshots.json, e.g. `shotName("hero", "voice")` is "marketing-trace-view-voice". */
export function shotName(pictureId, storyId) {
  const picture = pictureById(pictureId);
  const suffix = SUFFIX[/** @type {keyof typeof SUFFIX} */ (storyId)];
  if (suffix === undefined) throw new Error(`stories: unknown story "${storyId}"`);
  return `${picture.base}${suffix}`;
}

/** @returns {Picture} */
export function pictureById(pictureId) {
  const picture = PICTURES.find((p) => p.id === pictureId);
  if (!picture) throw new Error(`stories: unknown picture "${pictureId}"`);
  return picture;
}

/**
 * The four panels of one picture, in STORIES order.
 * @returns {{ story: string, shot: string, label: string, alt: string, caption: string }[]}
 */
export function panelsOf(pictureId) {
  const picture = pictureById(pictureId);
  return STORIES.map(({ id }) => {
    const content = picture.stories[id];
    if (!content) throw new Error(`stories: picture "${pictureId}" has no story "${id}"`);
    const label = typeof picture.label === "string" ? picture.label : picture.label[id];
    return { story: id, shot: shotName(pictureId, id), label, ...content };
  });
}
```

#### 2. `website/src/marketing/components/StoryPicture.astro` (new), in full

```astro
---
import type { ImageMetadata } from "astro";
import { panelsOf, pictureById, STORIES } from "../data/stories.mjs";
import Frame from "./Frame.astro";
import Tabs from "./Tabs.astro";

interface Props {
  /** A picture of data/stories.mjs: "hero", "trace", "explore", "issues", "evaluate", "prompts" or "alert". */
  picture: string;
  /** Fade the bottom of the frame into the band's background (Frame's prop). */
  fade?: boolean;
  /** The first story's picture loads with the page (only the hero). The others are always lazy. */
  eager?: boolean;
  /** Where the chip row and the caption sit at 960 px and wider; phones are always centred. */
  align?: "center" | "start" | "end";
}
const { picture, fade = true, eager = false, align = "center" } = Astro.props;

// Only a bundler import gives Astro's image pipeline an ImageMetadata, so the shots are
// imported here by name; data/stories.mjs holds the names (a missing PNG fails the build).
const shots = import.meta.glob<ImageMetadata>("../../assets/screenshots/marketing-*.png", {
  eager: true,
  import: "default",
});
function shotOf(name: string): ImageMetadata {
  const shot = shots[`../../assets/screenshots/${name}.png`];
  if (!shot) throw new Error(`StoryPicture: no src/assets/screenshots/${name}.png`);
  return shot;
}

const panels = panelsOf(picture);
const section = pictureById(picture).section;
---

<div
  class:list={["story", `story--${align}`]}
  data-story-picker
  data-stories={STORIES.map((story) => story.id).join(" ")}
>
  <Tabs id={`story-${picture}`} label={`Story: ${section}`} labels={STORIES.map((story) => story.label)}>
    {panels.map((panel, i) => (
      <div data-tab-panel hidden={i !== 0}>
        <Frame src={shotOf(panel.shot)} alt={panel.alt} label={panel.label} fade={fade} eager={eager && i === 0} />
        <p class="caption">{panel.caption}</p>
      </div>
    ))}
  </Tabs>
</div>

<style>
  .story :global(.tablist) {
    justify-content: safe center;
    border-bottom: 0;
    margin-bottom: var(--space-4);
  }
  .caption {
    margin-top: var(--space-3);
    text-align: center;
    font-size: var(--text-sm);
    color: var(--fg-muted);
  }
  @media (min-width: 960px) {
    .story--start :global(.tablist) {
      justify-content: flex-start;
    }
    .story--start .caption {
      text-align: left;
    }
    .story--end :global(.tablist) {
      justify-content: flex-end;
    }
    .story--end .caption {
      text-align: right;
    }
  }
</style>

<script>
  // Keeps every story picker on the page on the same story (D-W-25). Each picker is a Tabs
  // (lib/tabs.ts handles its clicks, arrow keys, Home and End); this script copies a change to the
  // others and to the URL, and reads `?story=` on load.
  document.addEventListener("DOMContentLoaded", () => {
    const pickers = Array.from(document.querySelectorAll<HTMLElement>("[data-story-picker]")).map(
      (root) =>
        Array.from(
          root.querySelectorAll<HTMLElement>(":scope > [data-tabs] > [role=tablist] > [role=tab]"),
        ),
    );
    const ids = (document.querySelector<HTMLElement>("[data-story-picker]")?.dataset.stories ?? "").split(" ");
    if (pickers.length === 0 || pickers.some((tabs) => tabs.length !== ids.length)) {
      console.error("Story pickers: the tabs and the stories differ");
      return;
    }

    let current = ids[0];
    // Selects `id` in every picker and writes it to the URL. Idempotent: the clicks it makes
    // change aria-selected again, which calls this again and returns at the first line.
    const show = (id: string) => {
      const i = ids.indexOf(id);
      if (i < 0 || id === current) return;
      current = id;
      for (const tabs of pickers) {
        if (tabs[i].getAttribute("aria-selected") !== "true") tabs[i].click();
      }
      const url = new URL(window.location.href);
      url.searchParams.set("story", id);
      history.replaceState(null, "", url);
    };

    // Clicks, arrow keys, Home and End all end in wireTabs' select(), which sets aria-selected.
    const observer = new MutationObserver((records) => {
      for (const record of records) {
        const tab = record.target as HTMLElement;
        if (tab.getAttribute("aria-selected") !== "true") continue;
        const tabs = pickers.find((list) => list.includes(tab));
        if (tabs) show(ids[tabs.indexOf(tab)]);
      }
    });
    for (const tab of pickers.flat()) {
      observer.observe(tab, { attributes: true, attributeFilter: ["aria-selected"] });
    }

    // On load: an unknown or missing value changes nothing, and the URL is not rewritten.
    const wanted = new URLSearchParams(window.location.search).get("story");
    if (wanted !== null) show(wanted);
  });
</script>
```

Notes for the implementer:
- `import.meta.glob` with a relative pattern from `src/marketing/components/` reaches `src/assets/screenshots/` (two levels up is `src/`), the same files `Hero.astro` imports today. The glob's keys keep that relative spelling; do not change it to `/src/...`.
- `Tabs.astro` renders the tablist with `aria-selected` on the first tab and `tabindex`, and its own script sets `id`, `role="tabpanel"` and `aria-labelledby` on every `[data-tab-panel]` and `hidden` on all but the first. Do not repeat that.
- Do not add `hashes` to these `Tabs`: they would write `#<hash>` and fight `?story=`.
- The chips are the `Tabs` tablist, so they reuse the `.tablist` style; add no chip-specific style.

#### 3. `website/src/marketing/home/Hero.astro`

1. Delete the four `import ... from "../../assets/screenshots/marketing-trace-view*.png"` lines, the `Frame` and `Tabs` imports, and the whole `SCENARIOS` constant. Add `import StoryPicture from "../components/StoryPicture.astro";` (Biome sorts the imports).
2. Replace the `<Tabs id="hero-scenarios" ...>...</Tabs>` block by:
   ```astro
   <StoryPicture picture="hero" fade={false} eager />
   ```
   inside `<div class="visual">`.
3. Delete the `.visual :global(.tablist)` and `.caption` rules from `<style>` (they moved to `StoryPicture.astro`). Keep `.visual { margin-top: var(--space-8); }`.

#### 4. `website/src/marketing/home/Chapters.astro`

1. Delete the six `import ... from "../../assets/screenshots/marketing-*.png"` lines and `import Frame ...`; add `import StoryPicture from "../components/StoryPicture.astro";`.
2. Replace each chapter's `<Frame .../>` by (every `fade` is today's choice: `true` for the trace list, `false` for the rest; `align="end"` exactly for the chapters that have `flip`, which are explore, evaluate and alert):

   | Chapter id | Replacement |
   |---|---|
   | `ch-trace` | `<StoryPicture picture="trace" align="start" fade />` |
   | `ch-explore` | `<StoryPicture picture="explore" align="end" fade={false} />` |
   | `ch-issues` | `<StoryPicture picture="issues" align="start" fade={false} />` |
   | `ch-evaluate` | `<StoryPicture picture="evaluate" align="end" fade={false} />` |
   | `ch-prompts` | `<StoryPicture picture="prompts" align="start" fade={false} />` |
   | `ch-alert` | `<StoryPicture picture="alert" align="end" fade={false} />` |

   Nothing else in the file changes (the `Chapter` props, the sections' tones, the texts).

#### 5. `website/src/marketing/data/stories.test.mjs` (new)

Run from the `website/` directory by `node --test scripts/ src/marketing/` (already in `test:scripts`). Resolve the files with `new URL("../../../screenshots.json", import.meta.url)` and `new URL("../../assets/screenshots/", import.meta.url)`.

Tests (names exact):
1. `the four stories are in the fixed order`: `STORIES.map(id)` equals `["support","coding","voice","extraction"]`; labels equal `["Support agent","Coding agent","Voice agent","Document extraction"]`; `DEFAULT_STORY === "support"`.
2. `there are seven pictures of four stories each`: `PICTURES.map(id)` equals `["hero","trace","explore","issues","evaluate","prompts","alert"]`; for each, `panelsOf(id)` has 4 entries whose `story` equals the STORIES ids in order (7 x 4 = 28 panels).
3. `every shot is in screenshots.json and has a PNG`: for every panel, `shot` is a `name` of `screenshots.json` and `<shot>.png` exists; the 28 shot names are distinct; `shotName("hero","voice") === "marketing-trace-view-voice"` and `shotName("trace","support") === "marketing-trace-list"`.
4. `alts are one sentence that describes the picture`: non-empty, at most 260 characters, ends with `.`, contains no `\n`, and is unique among the 28.
5. `captions are one line`: non-empty, at most 100 characters, ends with `.`, no `\n`, unique; the four hero captions and the 24 chapter captions equal the fixed content's captions table (copy the 28 strings into the test as an `EXPECTED` table keyed `[picture][story]` and compare, so a copy-edit has to touch two places). The coding Trace caption is "Only the failed runs, so nothing broken hides in the traffic."
6. `no copy uses a banned word`: over alts, captions and labels, none matches `/\b(seamless(ly)?|blazing(ly)?|effortless(ly)?|powerful|simply|easy|just)\b/i` (the STYLE.md list), `/!/`, `/Cetra|CETRA/`, or an emoji (`/\p{Extended_Pictographic}/u`).
7. `unknown pictures and stories throw`: `pictureById("nope")` and `shotName("hero","nope")` throw with `stories:` in the message.

#### 6. Scratch Playwright check (not committed)

Write `scratchpad/story-switcher.mjs` (the scratchpad directory, outside the repo) using `playwright` (`import { chromium } from "/home/sebastien/travaux/llm-observer/.claude/worktrees/website-research-c9eb27/website/node_modules/playwright/index.mjs"`) and `STORIES`, `panelsOf` imported from `website/src/marketing/data/stories.mjs`. Chromium: `pnpm --dir website exec playwright install chromium` once.

Run: `pnpm --dir website build` (or `astro build`), then `pnpm --dir website exec astro preview --port 4329 &` (check the port with `node -e` and `scripts/port-free.mjs`'s `portIsFree(4329)` first; never kill a process you did not start), and the script against `http://localhost:4329/`. The script exits 1 on the first failed check and prints which. Helpers: `pickers = page.locator("[data-story-picker]")`; `selected(page)` returns for each picker the index of its tab with `aria-selected="true"`; `visibleImgs(page)` returns the `img` elements in `[data-story-picker] [role=tabpanel]:not([hidden])`.

Checks, each in a fresh page unless said:
1. **Markup** (`/`, desktop 1280x800): `[data-story-picker]` count is 7; each has 4 `[role=tab]` with the four labels and `aria-selected` only on the first; each has 4 `[role=tabpanel]`, exactly one not hidden; there are 28 `img` in the pickers; the hero's first `img` has `loading="eager"` (or `fetchpriority="high"`), and the 27 others have `loading="lazy"`.
2. **Default shows support everywhere**: the 7 visible `img` `alt`s equal `panelsOf(picture)[0].alt` and the 7 visible captions equal the support captions.
3. **Only visible pictures load**: record every response whose URL contains `/_astro/marketing-`; after `load` and a scroll to the page bottom in steps of 600 px with 150 ms waits, none of the URLs contains `-coding`, `-voice` or `-extraction`; the set of distinct shot names requested is a subset of the 7 support shots.
4. **`?story=coding` on load**: `selected` is `[1,1,1,1,1,1,1]`; the 7 visible alts equal `panelsOf(p)[1].alt`; `location.search` is `?story=coding`; `history.length` is unchanged from a blank page's (1 more than before is the navigation itself; compare with the value read right after load and again after step 5).
5. **A chapter chip switches the page**: on `/`, scroll `#ch-prompts` into view, click the tab "Voice agent" inside the picker of `#ch-prompts`'s chapter; then `selected` is `[2,...,2]`; the hero's visible alt and the prompts picture's visible alt equal the voice ones; `location.search` is `?story=voice`; `history.length` equals its value before the click; scrolling the page to the bottom requests `-voice` images and none with `-coding` or `-extraction`.
6. **Keyboard**: focus the selected tab of the alert chapter's picker, press `ArrowLeft` (wraps from support to the last story): `selected` is `[3,...,3]` and the URL is `?story=extraction`; press `Home`: all `[0,...]` and `?story=support`; press `End` then `ArrowRight` (wraps to 0): all `[0,...]`.
7. **Hash and other parameters survive**: load `/?utm=1&story=voice#product`; click "Coding agent" in the hero: `location.search` is `?utm=1&story=coding` and `location.hash` is `#product`.
8. **Unknown value**: `/?story=bogus`: `selected` is all 0, `location.search` is still `?story=bogus`, no console error.
9. **No horizontal overflow at 390 px** (viewport 390x844, mobile UA not needed): for each of `/`, `/?story=coding`, `/?story=voice`, `/?story=extraction`: `document.documentElement.scrollWidth <= window.innerWidth`, and no picker's `[role=tablist]` is wider than its parent's `clientWidth` (its own `overflow-x:auto` scrolls instead); then click each chip once in the hero and re-check the overflow.
10. **No JavaScript** (`javaScriptEnabled: false`, `/`): the same 7 visible support pictures, panels 2 to 4 of each picker have the `hidden` attribute, and the tabs exist.
11. **Layout kept** (desktop): at 1280 px, the trace chapter's `.chapter` places its text left of the picture, the explore chapter's picture left of its text (compare `getBoundingClientRect().left`); the hero's picture is below its text (`top` greater than the lede's `bottom`); the chip row of a `start` chapter starts at the picture's left edge (within 2 px) and the `end` chapter's chips end at its right edge.
12. **Console and requests**: no `console.error`, no failed request, during all of the above.

#### 7. Check every alt against its picture

Read each of the 28 PNGs with the Read tool (the four hero shots `marketing-trace-view{,-coding,-voice,-extraction}.png` and the 24 chapter shots) next to its alt in `stories.mjs`. If the picture shows other names, counts or fields than the alt says (for example the score fields of a queue, the rule's condition or state on the alerts page, the trace names in a list, the models of the playground columns), correct the alt text, nothing else. Names: the coding trace is `coding-agent`; its service and its alert's name are `code-agent`. Keep each alt one sentence that describes the picture (rules of test 4) and rerun `node --test src/marketing/data/stories.test.mjs`. Report each alt you changed and why.

#### 8. The plan's README row

In `docs/plans/website/README.md`, replace the M15 row by:

```md
| M15 Story screenshots and switcher | [`15-m15-story-shots-and-switcher.md`](15-m15-story-shots-and-switcher.md) | done (Q0–Q2) |
```

### Tests

The cases of step 5 (node) and the twelve checks of step 6 (Playwright, scratch). The switcher touches only `website/`, so Q2 does not run `check-web`.

### Done when

Run from the worktree, with Node on the PATH:
- `cd website && ../node_modules/.bin/biome check --write src/marketing/data/stories.mjs src/marketing/data/stories.test.mjs src/marketing/components/StoryPicture.astro src/marketing/home/Hero.astro src/marketing/home/Chapters.astro` is clean (Biome may not lint `.astro` scripts; fix what it reports for the others).
- `cd website && node --test src/marketing/data/stories.test.mjs` passes.
- `pnpm --dir website exec astro check` reports 0 errors, 0 warnings in the touched files.
- `just check-docs` passes (it builds the site, runs `check-marketing`'s page weight and the screenshot list check).
- The Playwright script of step 6 prints no failure.
- By hand: `pnpm --dir website exec astro dev`, load `/`, `/?story=voice`, and look at the hero and the six chapters at 1280 and 390 px.
- `git status` shows only the Edit files.

**Commit:** `Switch every homepage picture by story`

---

## Settled questions

1. **Q0 gets a unit test** (AGENTS.md: every behaviour change comes with tests). Put `checkStorage` in its own module, `website/scripts/screenshot-storage.mjs`, exported, and import it from `screenshots.mjs`. Add `website/scripts/screenshot-storage.test.mjs`, run by the site's `node --test` setup (check how `just check-docs` finds the script tests and follow it). It covers: a missing map is fine; a valid map passes; a non-object, an empty key and a non-string value are each rejected, naming the shot. Both new files join Q0's Edit list. The scratch run that checks the storage is applied stays.
2. **The coding trace-list alt** is corrected against the PNG in Q2 step 7.

## Review fixes (2026-11-24), override the phases above

- **Q0 is done** (commit 095bcee). Skip it; Q1 uses its `storage` support.
- **Q1, readability (D-W-23).** A chapter picture is shown in a column about 715 CSS px wide, so a shot's CSS width (PNG width / 2) must be at most 900 px; aim for 760–900. Use the narrowest viewport where the page's content isn't cut, for every chapter (all four stories of a chapter share one viewport and clip, so the panels have equal heights). Report each chapter's viewport and the PNGs' widths.
  - Assistant shot: a 900×760 viewport, the dashboard behind; the assistant window stored at the left edge (`{"x":16,"y":16,"width":480,"height":728}`, tuned so both turns and the draft card show; report the value), so phones, which crop from the left, show the window first; clip the whole viewport.
  - Queue review, alerts and playground: narrow their viewports to fit the 900 px rule; if a page's content can't fit (for example the playground's two columns for extraction), stop and report with `QUESTION:` and the narrowest width that works.
- **Q1, hero shots.** The all-shots run rewrites `marketing-trace-view*.png`. Keep the new PNGs, and look at each one: the hero captions and alts (in `stories.mjs`, Q2) must still be true of them.
- **Q2, copy.** Step 7 checks the 28 captions too: each caption must be true of its picture (for example the coding alert's "in Slack": if the channel isn't visible, say "Know when the coding agent's error rate climbs." instead). Alts are one sentence describing the picture; they don't need to start with "The".
- **Q2, design.** Add `docs/website/00-design.md` to Q2's Edit list. In §12, change the coding trace list's "`coding-agent` on top" to "only the failed traces", replace "`-research`" with "`-voice`", and keep the shot count at 28 (4 hero + 24 chapter).
- **Q2, phones.** When the sync script selects a story in a picker the reader didn't click, it scrolls only that picker's tab list horizontally so the chosen chip is visible, never the page (`scrollIntoView` would scroll the page). In the `for (const tabs of pickers)` branch, after `tabs[i].click()`:
  ```ts
  const tab = tabs[i];
  const list = tab.parentElement as HTMLElement;
  const t = tab.getBoundingClientRect();
  const l = list.getBoundingClientRect();
  if (t.left < l.left) list.scrollLeft -= l.left - t.left;
  else if (t.right > l.right) list.scrollLeft += t.right - l.right;
  ```
  Add a Playwright check: at 390 px, click the hero chip "Document extraction"; `window.scrollY` is unchanged and the Alert picker's selected chip lies inside its tab list's visible rect.
- **Q2, first load.** With `?story=voice`, the support hero picture is still fetched (it is the eager one). Accepted (design §12: the first load is as today); check 4 doesn't assert otherwise.
- **Q2, checks.**
  - Check 3: a picture's shot name is the `/_astro/` file name cut at its first `.`.
  - Check 4: `history.length` is the same right after load and after a switch.
  - Check 11: compare the first tab's left edge with the tablist's content box (its left edge plus its padding), with 2 px tolerance.
- **Q2, conventions.** Build with `pnpm --dir website exec astro build` only (never `pnpm build`). Drop `DEFAULT_STORY` from the module; step 5's test 1 asserts `STORIES[0].id === "support"` instead.
- **Q2, URL on load.** Loading with `?story=` may rewrite the URL through `replaceState` (same parameters and hash, no new history entry); that is accepted, whatever the plan said about not rewriting it.
- **Q2, copy changes.** Any caption or alt changed in step 7 is changed in `stories.mjs` and in test 5's `EXPECTED` table together.

## Changes during implementation (Q1)

- **Assistant shots.** The floating window exists only at 1,024 px or wider; below that the assistant is a full-width sheet that ignores the stored rect. The shots use a 1,024×900 viewport, the window stored at the left edge, and a new optional `clipRect` entry field (`{x, y, width, height}` in CSS px, validated by `checkClipRect` in `website/scripts/screenshot-storage.mjs`, documented in `website/README.md`) cropping to 900×900. They show the window and part of the dashboard behind it.
- **Trace lists.** `cols=start,name,duration,tokens,cost`; the input column is dropped so every column fits in 900 px.
- **Queue review.** The shots clip only the review form (`section[aria-label="Review"]`), as before M15; the trace pane is left out.
- **Playground.** 960×640 for all four, so the extraction run's second model name shows whole (CSS 921 px, an accepted exception to the 900 px rule).
- **Alerts.** 900×900, with the rule panel in its bottom layout; the list's "Last evaluated" column is cut, which is accepted.

## Changes after the code review

- **The assistant shots** use a 1,024×1,000 viewport, the window stored at `{"x":16,"y":16,"width":560,"height":968}`, and `clipRect` `{"x":0,"y":0,"width":900,"height":1000}` (taller than first planned, so the coding chat's first question shows).
- **The trace lists** use `cols=start,name,duration,cost`. The app's column widths still cut long values (start times, some costs); that is the UI's, not the shots'.
- **Captions changed against their pictures** in Q2 step 7, as listed in its commit (4caee2f):
  - the support queue caption no longer says "next to the trace";
  - the voice hero says 3.1 s;
  - the extraction hero and trace-list captions describe what the pictures show.
- **The switcher has no committed browser test**: its 68 Playwright checks ran from a scratch script. Accepted for the marketing page; `check-marketing` and the node tests guard the data.
- **Known limit:** generated traces carry generic names (`billing-lookup`, `draft-reply`…), so a story's trace list shows its hero trace among generic rows, not a list of `voice-agent` traces.
