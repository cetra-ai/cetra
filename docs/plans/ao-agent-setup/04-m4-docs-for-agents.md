Status: plan, written 2026-11-21. Not implemented.

# AO · M4 — Docs for agents: `.md` pages, "Copy page", the docs MCP

This milestone does three things:
- After the build, every docs page gets a Markdown copy at `/docs/<page>/index.md`, made with M2's converter.
- The page title gets two actions, "Copy page" and "View as Markdown".
- The `cetra-site` Worker gets a script that answers `https://cetra.sh/mcp`: a stateless, read-only docs MCP server with `search_docs` and `get_page`, over a MiniSearch index built with the site.

The design is `docs/plans/ao-agent-setup/00-design.md` § Docs for agents (M4) and § M4; D-AO-12 and D-AO-13 apply. **Haiku implements it one phase at a time.** If a step is unclear or contradicts the code, **stop and report**.

**Check rule** (user, 2026-11-20): no full checks per phase. Each phase may run:
- its own `node --test` files;
- Biome from `website/`: `cd website && ../node_modules/.bin/biome check --write <files>`;
- `pnpm install` when it changes dependencies;
- one `pnpm --dir website run build` where "Done when" says so.

The coordinating session runs `just check` once after W4.

**Node on PATH.** Every command that runs `node` or `pnpm` starts with `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`.

## Read first

- `AGENTS.md`, `website/STYLE.md`.
- The session note `ao-m4-code-facts.md` in `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/`:
  - § 1: the site layout and URL mapping;
  - § 2: the converter over every page, and the multi-line `LinkCard` finding;
  - § 3: `PageTitle`, `_headers`;
  - § 4: the Worker config;
  - § 5: the MCP SDK API, verified in workerd;
  - § 6: MiniSearch;
  - § 7–8: checks and the docs page.
- `website/scripts/markdown.mjs` (the converter) and its tests.
- `website/src/components/TwoColumnContent.astro` (the existing override's shape), `website/wrangler.jsonc`, `website/public/_headers`, `website/package.json`.

## What it is, and what it is not

**When M4 is done:**
- `pnpm --dir website run build`, and the `check` script, write `dist/docs/**/index.md` for every docs source page and `dist/mcp-index.json`.
- `/docs/*/index.md` is served as `text/plain`.
- Every docs page except the generated API reference pages shows "Copy page" and "View as Markdown" next to its title.
- `website/worker/index.mjs` serves `POST /mcp` (MCP streamable HTTP, stateless, JSON responses) and nothing else; every other path stays a static asset.
- `platform/mcp.mdx` documents the `.md` pages and the docs MCP.
- D-W-3 is amended (the Worker has a script for `/mcp`).

**Not in M4:**
- AI answers (D-W-11 stands);
- attaching the custom domain route for `/mcp`: production uses the existing `cetra-site` Worker, and the domain is attached by hand (D-W-3);
- checking `wrangler preview` with a script, which only a deploy can do. The first PR deploy is the check (§ After W4).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-AO-12 | Our converter writes the `.md` pages; a `PageTitle` override adds the two actions; `llms.txt` stays with `starlight-llms-txt` |
| D-AO-13 | The docs MCP on the `cetra-site` Worker at `/mcp`: stateless, no auth, MiniSearch, nothing logged |
| D-W-3 | The site is a Worker with static assets; amended here (a script for `/mcp`) |
| D-W-10, D-W-11 | No analytics beyond Cloudflare's, no AI search |

Settled from the code (code facts § 3–6), recorded in the design on 2026-11-21:
- **Gating.** The title actions are hidden under `/docs/reference/api/`, except `/docs/reference/api/overview/`, the only page there with a source.
- **Snippets.** The snippet is the page's description, stored in the index.
- **Runtime.** No `nodejs_compat`. `zod` 4.6.7 is pinned. One server and one transport per request.

## Versions

| Package | Version | Where | Why |
|---|---|---|---|
| `@modelcontextprotocol/sdk` | `1.32.2` (2026-10-23, MIT) | `website/package.json` `dependencies`, exact | the MCP server and the Web-standard transport |
| `zod` | `4.6.7` (2026-10-12, MIT; already in the lock as a transitive) | `website/package.json` `dependencies`, exact | the SDK's required peer, and the tools' input schemas |
| `minisearch` | `7.2.0` (2025-09-16, MIT, no deps) | `website/package.json` `dependencies`, exact | the search index, built and read |

Re-check each with `npm view <name> version time --json`. Each must be older than 7 days (`minimumReleaseAge`). Report the versions used.

## Phases

```
W1 (converter: multi-line LinkCard; md-pages) ─► W2 (PageTitle actions, _headers) ─► W3 (index, Worker, MCP) ─► W4 (docs, decisions)
```

The phases run in order. Files per phase:
- W1: `website/scripts/{markdown.mjs, markdown.test.mjs, md-pages.mjs (new), md-pages.test.mjs (new)}`, `website/package.json`.
- W2: `website/src/components/PageTitle.astro` (new), `website/astro.config.mjs`, `website/public/_headers`.
- W3: `website/scripts/{docs-index.mjs, docs-index.test.mjs, docs-mcp.mjs, docs-mcp.test.mjs}` (new), `website/worker/index.mjs` (new), `website/wrangler.jsonc`, `website/package.json`, `pnpm-lock.yaml`.
- W4: `website/src/content/docs/docs/platform/mcp.mdx`, `docs/mvp-decisions.md`, `docs/plans/docs-site/README.md` (the D-W-3 text, if it repeats it), `CHANGELOG.md`, `docs/roadmap.md`.

---

## W1 — Multi-line `LinkCard` and the `.md` pages

**Goal:** the converter handles a `<LinkCard>` written over several lines, and a build step writes one `.md` per docs source page.

**Edit:** `website/scripts/markdown.mjs`, `markdown.test.mjs`, `md-pages.mjs` (new), `md-pages.test.mjs` (new), `website/package.json`.

**Steps.**
1. **`markdown.mjs` `prepass`.** When a line's trimmed text starts with `<LinkCard` and does not end with `/>`, join it with the following lines, each trimmed and separated by one space, up to and including the first line whose trimmed text ends with `/>`. Then apply the existing one-line `LinkCard` rule to the joined text. If no closing line comes, throw `Error("unclosed <LinkCard> at line <n>")`.
2. **`md-pages.mjs`.** Header comment: "Writes a Markdown copy of every docs page next to its HTML after `astro build` (D-AO-12): `src/content/docs/docs/<p>.mdx` → `dist/docs/<p>/index.md`."

   ```js
   export const SOURCE = "src/content/docs/docs";
   export const OUT = "dist/docs";
   /** "reference/cli.md" → "reference/cli/index.md"; "index.mdx" → "index.md"; "evaluation/index.mdx" → "evaluation/index.md" */
   export function outputPath(relative) {}
   export function main() {}  // returns 0 or 1
   ```

   - **`main`:**
     1. If `OUT` does not exist, print `dist/docs is missing: run astro build first` and return 1.
     2. Walk `SOURCE` for `*.md` and `*.mdx` with `readdirSync(SOURCE, { recursive: true })`, as `check-terms.mjs` does.
     3. Convert each file with `toMarkdown`, and write it to `join(OUT, outputPath(rel))`.
     4. A converter error prints `<rel>: <message>` and makes `main` return 1, after trying every page.
     5. Print `wrote <n> Markdown pages`.
   - The entry guard is the usual one.
3. **`website/package.json`.**
   - `build` becomes `pnpm gen:openapi && pnpm gen:changelog && astro build && node scripts/md-pages.mjs`.
   - In `check`, replace the final `astro build` with `astro build && node scripts/md-pages.mjs`.

**Tests:**
- **`markdown.test.mjs`:** a three-line `<LinkCard title="Quick start" description="Run it." href="/docs/get-started/quickstart/" />` becomes `- [Quick start](https://cetra.sh/docs/get-started/quickstart/): Run it.`; an unclosed multi-line card throws.
- **`md-pages.test.mjs`:** `outputPath` for `index.mdx`, `a/index.mdx`, `a/b.md` and `a/b.mdx`.

**Done when:**
- `cd website && node --test scripts/markdown.test.mjs scripts/md-pages.test.mjs` passes;
- `pnpm --dir website run build` ends with `wrote <n> Markdown pages`, and `dist/docs/index.md`, `dist/docs/get-started/quickstart/index.md` and `dist/docs/index.md`'s "Where to go next" list are bullets with descriptions;
- `just docs-gen` leaves `skills/cetra/references/` unchanged (the converter change touches no listed page). If it does change one, commit the regenerated files and say so;
- Biome is clean.

**Commit:** `Write a Markdown copy of every docs page`

---

## W2 — "Copy page" and "View as Markdown"

**Goal:** two small actions next to every docs page title, and `.md` served as plain text.

**Edit:** `website/src/components/PageTitle.astro` (new), `website/astro.config.mjs`, `website/public/_headers`.

**Steps.**
1. **`PageTitle.astro`:**

   ```astro
   ---
   // Starlight's PageTitle (0.42.10) with two actions for agents (D-AO-12): copy the page's
   // Markdown copy (written by scripts/md-pages.mjs) or open it. Hidden on the generated
   // API reference pages, which have no Markdown source.
   import Default from "@astrojs/starlight/components/PageTitle.astro";

   const path = Astro.url.pathname;
   const generated = path.startsWith("/docs/reference/api/") && path !== "/docs/reference/api/overview/";
   const markdown = `${path.endsWith("/") ? path : `${path}/`}index.md`;
   ---
   <div class="title-row">
     <Default {...Astro.props} />
     {!generated && path.startsWith("/docs/") && (
       <div class="page-actions">
         <button type="button" class="page-action" data-markdown={markdown}>Copy page</button>
         <a class="page-action" href={markdown}>View as Markdown</a>
       </div>
     )}
   </div>
   <script>
     for (const button of document.querySelectorAll<HTMLButtonElement>("button[data-markdown]")) {
       button.addEventListener("click", async () => {
         const label = button.textContent;
         try {
           const response = await fetch(button.dataset.markdown ?? "");
           if (!response.ok) throw new Error(String(response.status));
           await navigator.clipboard.writeText(await response.text());
           button.textContent = "Copied";
         } catch {
           button.textContent = "Copy failed";
         }
         setTimeout(() => { button.textContent = label; }, 2000);
       });
     }
   </script>
   <style>
     .title-row { display: flex; flex-wrap: wrap; align-items: baseline; justify-content: space-between; gap: 0.5rem 1rem; }
     .page-actions { display: flex; gap: 0.5rem; }
     .page-action {
       font-size: var(--sl-text-sm); color: var(--sl-color-gray-2); background: none; cursor: pointer;
       border: 1px solid var(--sl-color-gray-5); border-radius: 0.25rem; padding: 0.125rem 0.5rem; text-decoration: none;
     }
     .page-action:hover { color: var(--sl-color-white); border-color: var(--sl-color-gray-3); }
   </style>
   ```

   - **The import.** If `@astrojs/starlight/components/PageTitle.astro` is not exported by the package (check its `package.json` `exports`), import the path that the package does export, and say which in the report.
   - **Vertical spacing.** The default `h1` has `margin-top: 1rem`. If the row then sits badly, leave it; do not restyle the `h1`.
2. **`astro.config.mjs`.** Add `PageTitle: "./src/components/PageTitle.astro",` to the `components` object, with the comment `// Title actions: copy or view the page as Markdown (D-AO-12).`
3. **`public/_headers`.** Append:

   ```
   /docs/*/index.md
     Content-Type: text/plain; charset=utf-8
     X-Robots-Tag: noindex
   /docs/index.md
     Content-Type: text/plain; charset=utf-8
     X-Robots-Tag: noindex
   ```

   Keep the file's existing rules and format. Cloudflare's `*` splat matches across `/`, so nested pages are covered; the first deploy checks it (§ After W4).

**Tests:** none in `node --test`, since Astro components are checked by `astro check` and the build.

**Done when:**
- `pnpm --dir website run build` succeeds;
- `grep -c 'data-markdown' dist/docs/get-started/quickstart/index.html` is 1, and the same grep on `dist/docs/reference/api/index.html` is 0;
- `cd website && pnpm exec astro check` passes.

**Commit:** `Add Copy page and View as Markdown to the docs`

---

## W3 — The search index and the docs MCP Worker

**Goal:** `dist/mcp-index.json` is built with the site, and the Worker answers MCP on `/mcp` with `search_docs` and `get_page`.

**Edit:** `website/scripts/docs-index.mjs`, `docs-index.test.mjs`, `docs-mcp.mjs`, `docs-mcp.test.mjs` (new), `website/worker/index.mjs` (new), `website/wrangler.jsonc`, `website/package.json`, `pnpm-lock.yaml`.

**Steps.**
1. **Dependencies.** Add `"@modelcontextprotocol/sdk": "1.32.2"`, `"minisearch": "7.2.0"` and `"zod": "4.6.7"` to `website/package.json` `dependencies`, in alphabetical order. Run `pnpm install` at the root.
2. **`docs-index.mjs`** (build time, Node):

   ```js
   /** The index options, shared with the Worker (loadJSON needs the same ones). */
   export const INDEX_OPTIONS = {
     idField: "url",
     fields: ["title", "headings", "body"],
     storeFields: ["title", "description"],
   };
   export const SEARCH_OPTIONS = { prefix: true, fuzzy: 0.2, boost: { title: 3, headings: 2 }, combineWith: "AND" };
   /** One document per Markdown page: url "/docs/<p>/", title (the "# " line), description (the first
    *  paragraph after it), headings ("##"–"####" lines joined by "\n"), body (the rest). */
   export function documentFor(url, markdown) {}
   export function main() {} // reads dist/docs/**/index.md, writes dist/mcp-index.json; 0 or 1
   ```

   - **Page URLs.** `dist/docs/index.md` gives `/docs/`, and `dist/docs/a/b/index.md` gives `/docs/a/b/`.
   - **`main`** builds `new MiniSearch(INDEX_OPTIONS)`, calls `addAll`, and writes `JSON.stringify(index)`. It prints `indexed <n> pages (<size> bytes)`.
   - **`website/package.json`:** after `node scripts/md-pages.mjs` in both `build` and `check`, add `&& node scripts/docs-index.mjs`.
3. **`docs-mcp.mjs`** (the Worker's logic, importable by `node --test`):

   ```js
   import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
   import { WebStandardStreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/webStandardStreamableHttp.js";
   import MiniSearch from "minisearch";
   import { z } from "zod";
   import { INDEX_OPTIONS, SEARCH_OPTIONS } from "./docs-index.mjs";

   export const SITE = "https://cetra.sh";
   export const MAX_PAGE_CHARS = 100_000;

   /** "https://cetra.sh/docs/x/", "/docs/x/", "/docs/x", "docs/x" → "/docs/x/"; anything else → undefined. */
   export function pagePath(input) {}
   /** Search results: [{title, url: SITE + path, snippet: description}], best first, at most `limit`. */
   export function search(index, query, limit) {}
   /** A page's Markdown cut to MAX_PAGE_CHARS, with a final note line when cut. */
   export function cut(markdown) {}
   /** Handles one HTTP request. `assets.fetch(request)` reads the static files. */
   export async function handle(request, assets, loadIndex) {}
   ```

   - **`pagePath`** accepts only paths made of `[a-z0-9-/]` under `/docs/`. `..` is refused.
   - **`handle`:**
     1. A method other than `POST` answers `405` with `Allow: POST` and an empty body.
     2. Otherwise, build `new McpServer({ name: "cetra-docs", version: "1" }, { instructions: "Search and read the cetra documentation (https://cetra.sh/docs/)." })` and register two tools with `registerTool`. Both have the annotations `{ readOnlyHint: true, openWorldHint: false }`.
        - **`search_docs`.** Description "Search the cetra documentation. Returns pages with a URL and a one-line summary; read one with get_page." Input schema: `{ query: z.string().min(1).max(200), limit: z.number().int().min(1).max(20).optional() }`.
          - It answers `{ content: [{ type: "text", text: JSON.stringify(search(await loadIndex(), query, limit ?? 8)) }] }`.
          - With no hits, the text is `[]`.
        - **`get_page`.** Description "Read one cetra documentation page as Markdown. `url` is a https://cetra.sh/docs/… URL or a /docs/… path." Input schema: `{ url: z.string().min(1).max(300) }`.
          - `pagePath(url)` → `undefined` answers `{ isError: true, content: [{ type: "text", text: "not a cetra docs page: give a https://cetra.sh/docs/… URL" }] }`.
          - Otherwise it fetches `new Request(new URL(`${path}index.md`, request.url))` through `assets`. Only status 200 counts; the 404 page also comes back as a body. On any other status it answers `isError` with the text `no such page: <path>`.
          - On 200 it answers the text `cut(markdown)`.
     3. `const transport = new WebStandardStreamableHTTPServerTransport({ sessionIdGenerator: undefined, enableJsonResponse: true });`, then `await server.connect(transport);`, then `return transport.handleRequest(request);`.

     A new server and transport are made per request: the SDK refuses to reuse a stateless transport.
   - **`cut`.** Text over `MAX_PAGE_CHARS` becomes its first `MAX_PAGE_CHARS` characters, then `\n\n[cut at 100000 characters]`.
   - Nothing is logged: no `console.*` anywhere.
4. **`website/worker/index.mjs`:**

   ```js
   // The cetra-site Worker (D-W-3, D-AO-13): /mcp is the docs MCP server; every other path is a
   // static asset and never reaches this script (assets.run_worker_first).
   import { handle } from "../scripts/docs-mcp.mjs";
   import { INDEX_OPTIONS } from "../scripts/docs-index.mjs";
   import MiniSearch from "minisearch";

   let index; // loaded once per isolate

   export default {
     async fetch(request, env) {
       const url = new URL(request.url);
       if (url.pathname !== "/mcp") return env.ASSETS.fetch(request);
       return handle(request, env.ASSETS, async () => {
         if (index === undefined) {
           const response = await env.ASSETS.fetch(new Request(new URL("/mcp-index.json", request.url)));
           index = MiniSearch.loadJSON(await response.text(), INDEX_OPTIONS);
         }
         return index;
       });
     },
   };
   ```

5. **`wrangler.jsonc`.**
   - Add `"main": "./worker/index.mjs"`.
   - Add `"binding": "ASSETS"` and `"run_worker_first": ["/mcp"]` to `assets`.
   - Update the header comment: "The docs site as a Worker with static assets (D-W-3); its script answers only /mcp, the docs MCP server (D-AO-13)."
   - Add no `compatibility_flags`.
6. **`website/package.json` `test:scripts`** stays `node --test scripts/`. The Worker entry has no logic of its own to test.

**Tests:**
- **`docs-index.test.mjs`:**
  - `documentFor` splits a small Markdown page into its title, description, headings and body;
  - an index built from three small pages finds `"annotaton queue"` (a typo) on the right page with `SEARCH_OPTIONS`;
  - `toJSON`, then `loadJSON` with `INDEX_OPTIONS`, finds the same.
- **`docs-mcp.test.mjs`**, with Node 26's `Request` and `Response` and a fake `assets` whose `fetch` serves a Map of paths:
  - `pagePath` on good input, and on `../`, `/docs/../x` and `https://evil.example/docs/x/`;
  - `search` limits, and fills the snippet from the description;
  - `cut` at the limit;
  - `handle`:
    - `GET /mcp` gives 405;
    - an MCP `initialize` POST (with `Accept: application/json, text/event-stream` and `Content-Type: application/json`) gives a JSON result with `serverInfo.name` `cetra-docs`;
    - `tools/list` lists exactly `search_docs` and `get_page`;
    - `tools/call search_docs` returns hits;
    - `tools/call get_page` for a known page returns its Markdown;
    - for an unknown page it gives `isError` `no such page`;
    - for an off-site URL it gives `isError`;
    - an empty `query` gives `isError`.

    Each call is a fresh request with a fresh server, as in production. For the `initialize` handshake, copy the JSON-RPC bodies from the code-facts note § 5.

**Done when:**
- `cd website && node --test scripts/docs-index.test.mjs scripts/docs-mcp.test.mjs` passes;
- `pnpm --dir website run build` ends with `indexed <n> pages`;
- `pnpm --dir website exec wrangler deploy --dry-run --outdir /tmp/claude-1000/ao-w3-bundle` succeeds and reports a gzip size under 1 MB. Report the size;
- Biome, run from `website/`, is clean.

**Commit:** `Serve a docs MCP server from the site's Worker`

---

## W4 — Docs page, decisions, changelog

**Goal:** users and agents can find the `.md` pages and the docs MCP; the decisions say what changed.

**Edit:** `website/src/content/docs/docs/platform/mcp.mdx`, `docs/mvp-decisions.md`, `docs/plans/docs-site/README.md` (only where it states that the Worker has no script), `CHANGELOG.md`, `docs/roadmap.md`.

**Steps.**
1. **`platform/mcp.mdx`.** Rename the last section, `## For coding agents that read the docs`, to `## Docs for coding agents`. Keep its `llms.txt` sentence, and add:
   - **The `.md` copy of every page:** `https://cetra.sh/docs/<page>/index.md`, and the "Copy page" and "View as Markdown" actions next to each title.
   - **A `### Docs MCP server` subsection:**
     - the URL `https://cetra.sh/mcp`, no token;
     - a two-row tool table (`search_docs`: `query`, `limit`; `get_page`: `url`);
     - the Claude Code command in a `bash` fence: `claude mcp add --transport http cetra-docs https://cetra.sh/mcp`;
     - the Cursor JSON in a `json` fence, the same shape as the page's trace-MCP example, without the header;
     - one sentence saying it searches the docs only, has no access to your data, and is separate from the trace MCP above.

   Leave the page's `description` alone: its "ten read-only tools" is about the trace MCP.
2. **`docs/mvp-decisions.md`.**
   - **D-W-3:** append "Amended by D-AO-13: the Worker has a script, `website/worker/index.mjs`, that answers only `/mcp` (`assets.run_worker_first`); every other path is a static asset."
   - **D-W-19:** append "The `.md` copy of each page (D-AO-12) is written by `scripts/md-pages.mjs`."
   - `grep -n "no Worker script\|no script" docs/plans/docs-site/README.md` and add the same amendment note after each statement found.
3. **`CHANGELOG.md` `[Unreleased]` `### Added`:** "Every docs page has a Markdown copy at `<page>/index.md`, with Copy page and View as Markdown actions; `https://cetra.sh/mcp` is a docs MCP server for coding agents (`search_docs`, `get_page`)."
4. **`docs/roadmap.md`.** The AO row's Status adds `M4 implemented (Markdown pages, Copy page, docs MCP)`.

**Done when:**
- `cd website && node scripts/check-terms.mjs && node scripts/check-env-vars.mjs` exits 0;
- `pnpm --dir website run build` succeeds, which also validates the links.

**Commit:** `Document the Markdown pages and the docs MCP`

---

## After W4 (the coordinating session)

1. `just check`, once.
2. A Sonnet code review of W1–W4, then the fixes.
3. **First deploy check**, by the PR's `docs-deploy` preview (`pr-<n>`):
   - `POST <preview>/mcp` `initialize` answers;
   - `<preview>/docs/get-started/quickstart/index.md` is `text/plain`;
   - a nested page such as `/docs/reference/python/tracing/index.md` is `text/plain` too.

   If the preview cannot deploy a script, report it to the user: it is the unverified part (code facts § 4).
