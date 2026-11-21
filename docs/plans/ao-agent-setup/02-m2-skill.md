Status: plan, written 2026-11-21. Not implemented.

# AO · M2 — The cetra skill, its checks and the mirror

This milestone adds the cetra agent skill (`skills/cetra/`): a hand-written `SKILL.md`, plus reference pages generated from the docs site by a new Markdown converter. `just check-docs` fails when the references are stale or `SKILL.md` names something that does not exist. A workflow mirrors the skill to the public `cetra-ai/skills` repository. The design is `docs/plans/ao-agent-setup/00-design.md` (§ The skill (M2), § M2); D-AO-8, D-AO-9 and D-AO-12 apply. **Haiku implements it one phase at a time.** If a step is unclear or contradicts the code, **stop and report**; do not improvise.

**Check rule** (user, 2026-11-20): phases are committed without the full checks. Exceptions, needed to produce committed files:
- S1 runs `pnpm install` to update the lockfile.
- S2 runs the references generator once in write mode.
- S1–S3 run their own unit tests: `cd website && node --test scripts/<file>.test.mjs`, which is fast.
- S1–S3 format and lint their new scripts with `pnpm exec biome check --write website/scripts/<files>` from the repository root (Biome is a root devDependency). `pnpm check` runs `biome check .` over `website/`, and an unformatted script would fail the final check. Files under `skills/` are outside Biome: write them by hand exactly as shown.

The full `just check` runs once, after S4, by the coordinating session.

**Node on PATH.** Every command that runs `node` or `pnpm` starts with `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`.

## Prerequisites

- The design doc is approved (2026-11-21).
- Nothing from M1 is needed: M1 and M2 touch different files.

## Read first

- `AGENTS.md`: § Rules, § Layout, § Commands, § Tests (the `CETRA_DOCS_WRITE` bullet), § CI.
- `website/STYLE.md`.
- `website/scripts/check-env-vars.mjs` and `check-env-vars.test.mjs`: the style every new script follows. It has a header comment, pure exported functions, a `main` that prints `path:line: message` to stderr and returns 0 or 1, and the entry guard `if (process.argv[1] === fileURLToPath(import.meta.url))`. Its tests are flat `test("sentence", …)` cases with inline strings.
- `website/package.json` (`check:content`, `check`) and the `justfile` recipes `js-install`, `check-docs`, `docs-gen`, `check-workflows`.
- `.github/workflows/docs-deploy.yml`: how a workflow passes a secret through `env:` and calls `just`.
- The session note `ao-m2-code-facts.md` (in `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/`): § 3 (what `tidymd` does and does not convert), § 6 (CI), § 7 (the Agent Skills format and plugin manifests).

## What it is, and what it is not

**When M2 is done:**
- `website/scripts/markdown.mjs` turns a docs page (MDX) into clean Markdown. M4 reuses it for the site's `.md` pages.
- `skills/cetra/` holds `SKILL.md` and `references/*.md`, generated from the pages listed in `skills/references.json`.
- `just check-docs` fails when:
  - a reference is stale;
  - `SKILL.md`'s frontmatter is invalid;
  - `SKILL.md` links a missing reference or docs page;
  - `SKILL.md` names a `cetra` subcommand that does not exist, or an unknown `CETRA_*` variable.
- `just docs-gen` rewrites the references.
- `skills/manifests/` holds the plugin manifests, the README and the LICENSE of the mirror repository.
- `just skills-mirror <dir>` copies everything into a checkout of `cetra-ai/skills`, and `--push` commits and pushes it.
- `.github/workflows/skills-mirror.yml` runs it on pushes to `main`. When the `SKILLS_MIRROR_KEY` secret is missing, it does nothing.

**Not in M2:**
- installing the skill from the CLI (M3);
- checking `npx @cetra-ai/cli <command>` names in `SKILL.md` (M3, when the Node CLI exists);
- the site's `.md` pages, the "Copy page" button and the docs MCP (M4);
- creating the `cetra-ai/skills` repository and its deploy key: the user does that (design § Open questions 1). The repository needs an initial commit (a README) and a default branch that accepts a direct push. **`SKILLS_MIRROR_KEY` is set only after M3 and M4 are merged**: `SKILL.md` names `npx @cetra-ai/cli` (M3) and `https://cetra.sh/mcp` (M4), and the mirror must not publish them before they exist. Until then the workflow does nothing;
- the skill's docs page and its `CHANGELOG.md` line: M3 adds both with the "Set up with your coding agent" page, where users first meet the skill.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-AO-8 | `skills/cetra/` in this repo; `SKILL.md` hand-written, `references/` generated from the docs; checked in `just check-docs` |
| D-AO-9 | Mirrored by CI to the public `cetra-ai/skills` repository, with Claude, Codex and Cursor plugin manifests |
| D-AO-12 | One converter (`markdown.mjs`: `tidymd` plus a pre-pass) for the skill references and, in M4, the `.md` pages |
| D-W-6 | Sample markers `{/* test */}`, `{/* untested: … */}`, `{/* example:… */}` in the docs (the converter drops them) |
| D-W-7 | Generated pages are committed, with a check that fails when stale and `CETRA_DOCS_WRITE=1` to rewrite |

The user's answers this plan relies on (2026-11-21):
- the skill lives in this repo and is mirrored;
- the CLI is the Node package `@cetra-ai/cli`, run with `npx @cetra-ai/cli`.

## Versions

| Package | Version | Where | Why |
|---|---|---|---|
| `tidymd` | `0.2.0` (latest on 2026-11-21, published 2026-08-12, MIT; one dependency, `yaml ^2.9.0`, already in the lock at 2.9.1) | `website/package.json` `dependencies`, exact | The MDX-to-Markdown cleaner `markdown.mjs` wraps |

Re-check with `npm view tidymd version time --json` and report the version used. It must be older than 7 days (`minimumReleaseAge`).

## Phases

```
S1 (converter) ──► S2 (references) ──► S3 (SKILL.md and its check) ──► S4 (manifests, mirror, AGENTS.md)
```

The phases run in order: each one uses the files of the one before. Files per phase:
- S1: `website/package.json`, `pnpm-lock.yaml`, `website/scripts/markdown.mjs`, `website/scripts/markdown.test.mjs`.
- S2: `skills/references.json`, `skills/cetra/references/*.md`, `website/scripts/skill-references.mjs` and its test, `website/package.json`, `justfile`.
- S3: `skills/cetra/SKILL.md`, `website/scripts/check-skill.mjs` and its test, `website/scripts/check-env-vars.mjs` and its test, `website/package.json`.
- S4: `skills/manifests/**`, `tools/skills/mirror.sh`, `justfile`, `.github/workflows/skills-mirror.yml`, `AGENTS.md`, `docs/roadmap.md`.

---

## S1 — The Markdown converter

**Goal:** a pure function that turns one docs page (MDX or MD source) into clean Markdown with absolute links.

**Edit:**
- `website/package.json`
- `pnpm-lock.yaml`
- `website/scripts/markdown.mjs` (new)
- `website/scripts/markdown.test.mjs` (new)

**Steps.**
1. Add `"tidymd": "0.2.0"` to `website/package.json` `dependencies`, in alphabetical order. Then run `pnpm install` at the repository root, which updates `pnpm-lock.yaml`. Commit both files.
2. Create `website/scripts/markdown.mjs`. Its header comment reads: "Converts a docs page (MDX) to plain Markdown for agents: the skill references (D-AO-8) and the site's `.md` pages (M4). tidymd does the Starlight components; the pre-pass below handles ours." Its exports:

   ```js
   /** The site's origin, used to make `/docs/...` links absolute. */
   export const SITE = "https://cetra.sh";

   /**
    * Our components and markers, removed or rewritten before tidymd runs.
    * Lines inside fenced code blocks are never touched.
    * @param {string} source the page's full text, front matter included
    * @returns {string}
    */
   export function prepass(source) {}

   /**
    * The page as Markdown: `# <title>`, the description as the first paragraph,
    * then the body. Links to `/docs/...` become `${SITE}/docs/...`.
    * Throws an Error naming the line when a JSX component survives the conversion.
    * @param {string} source
    * @returns {string}
    */
   export function toMarkdown(source) {}
   ```

3. **`prepass(source)`**, line by line, in this order:
   1. If the text starts with a line `---`, copy every line up to and including the next line `---` unchanged. That is the front matter.
   2. **Fences.** Track fenced code blocks.
      - A line whose trimmed text starts with three or more backticks opens a fence when none is open.
      - Inside a fence, a line whose trimmed text is **only** backticks, at least as many as the opening run, closes it.
      - Lines inside a fence, and the fence lines themselves, are copied unchanged.
      - A line outside a fence whose trimmed text starts with `~~~` throws `Error("line <n>: use backtick fences")`: tidymd does not know `~~~` fences and would eat `import … from` lines inside them. The docs have none today.
   3. Outside fences, with `t` the line's trimmed text:
      - `t` matches `/^\{\/\*[\s\S]*\*\/\}$/` (a sample marker or MDX comment on its own line): drop the line.
      - `t` matches `/^<Tabs\b[^>]*>$/`, or is `</Tabs>` or `</TabItem>`: drop the line.
      - `t` matches `/^<TabItem\b[^>]*\blabel="([^"]+)"[^>]*>$/`: replace the line with three lines: `""`, `**<label>**`, `""`.
      - `t` starts with `<Backing` or `<Image`: drop lines starting with this one, up to and including the first line, **this one included**, whose trimmed text ends with `/>`. A one-line element drops just its own line. If no such line comes before the end of the text, throw `Error("unclosed <Backing> or <Image> at line <n>")` (1-based).
      - `t` is `<CardGrid>` or `</CardGrid>`, or matches `/^<CardGrid\b[^>]*>$/`: drop the line.
      - `t` matches `/^<LinkCard\b[^>]*\btitle="([^"]+)"[^>]*\bhref="([^"]+)"[^>]*\/>$/`: replace the line with `- [<title>](<href>)`, followed by `: <description>` when the tag also has `description="…"`. tidymd alone runs the cards of a grid together on indented lines (`integrations/index.mdx`). A `<LinkCard` line that does not match (a tag over several lines, or without `title` or `href`) is left to tidymd; the guard of step 4.5 then throws if a tag survives, which is the wanted failure.
      - Any other line: copy it unchanged.
   4. Join with `\n`.
4. **`toMarkdown(source)`:**
   1. `const fm = extractStarlightFrontmatter(source)`, imported from `tidymd`. It can be `null`.
   2. `let body = cleanStarlightMarkdown(prepass(source), { frontmatter: "title-as-heading", internalLinks: { mode: "baseUrl", baseUrl: SITE } })`.
   3. If `fm?.description` is a non-empty string, insert `""` and the description right after the first line that starts with `# `. If there is no such line, put the description, then `""`, at the start.
   4. **Post-pass**, line by line:
      - remove trailing whitespace on every line;
      - outside fences, remove leading spaces before a line whose trimmed text starts with `#` or with three backticks, because tidymd leaves such indentation behind. Track fences in the post-pass with the same rule as the pre-pass (step 3.2): the leading spaces of a fence's opening and closing lines are removed too, but lines inside the fence are left as they are;
      - collapse runs of more than one blank line into one;
      - end the text with exactly one `\n`.
   5. **Guard.** Outside fences, remove inline code spans (`` `…` ``) from each line, then test it against `/<\/?[A-Z][a-z][A-Za-z]*\b/`. A match means an unconverted component: throw `Error("line <n>: unconverted component <match>")`. `<YOUR_KEY>`-style placeholders do not match, because their second character is uppercase or `_`.
5. Biome formats the file: 2 spaces, double quotes, semicolons, trailing commas, 100 columns.

**Tests** (`markdown.test.mjs`, `node:test` and `node:assert/strict`, inline strings):
- `prepass` turns `<Tabs syncKey="lang">` / `<TabItem label="Python">` / a python fence / `</TabItem>` / `<TabItem label="TypeScript">` / a ts fence / `</TabItem>` / `</Tabs>` into `**Python**`, the python fence, `**TypeScript**`, the ts fence, with no tag left.
- `prepass` drops `{/* untested: reason */}`, `{/* test */}` and `{/* example:quick-start */}` lines.
- `prepass` drops a one-line `<Backing fixtures={["a/b"]} tests={["x"]} />`, and a `<Backing` that spans three lines.
- `prepass` drops `<Image src={traceView} alt="The trace view" />`.
- `prepass` turns `<CardGrid>`, `<LinkCard title="OpenAI" description="Chat and embeddings." href="/docs/integrations/openai/" />`, `<LinkCard title="DSPy" href="/docs/integrations/dspy/" />`, `</CardGrid>` into `- [OpenAI](/docs/integrations/openai/): Chat and embeddings.` and `- [DSPy](/docs/integrations/dspy/)`.
- `prepass` throws on a `~~~` line outside fences. A fence opened by three backticks is not closed by a line reading three backticks followed by `bash` inside it.
- `prepass` leaves a fence that contains `<TabItem label="X">` and `{/* test */}` untouched.
- `prepass` leaves the front matter untouched.
- `prepass` throws on a `<Backing` that is never closed.
- `toMarkdown` makes `# Title`, then the description paragraph, then the body.
- `toMarkdown` rewrites `[Traces](/docs/tracing/traces/)` to `[Traces](https://cetra.sh/docs/tracing/traces/)`, and leaves `[x](#anchor)` and `[y](https://example.com/)` alone.
- `toMarkdown` removes `import { Tabs, TabItem } from "@astrojs/starlight/components";`.
- `toMarkdown` keeps `<YOUR_PASSWORD>` in a fence and in prose.
- `toMarkdown` converts `<Aside>` (tidymd: `**Note:** …`).
- `toMarkdown` throws on a page with `<Widget foo="1" />` outside fences, and the message names the line.
- `toMarkdown` on a source without front matter returns the body without a `# ` line added.
- `toMarkdown` ends with exactly one newline and has no line with trailing spaces.
- `toMarkdown` on the real file `src/content/docs/docs/integrations/openai.mdx` (read with `readFileSync`, the test's cwd being `website/`) contains `**Python**` and `**TypeScript**` and none of `{/*`, `<Backing`, `<Tab`.

**Done when:**
- `cd website && node --test scripts/markdown.test.mjs` passes;
- `pnpm exec biome check --write website/scripts/markdown.mjs website/scripts/markdown.test.mjs` (from the repository root) leaves no error;
- `pnpm-lock.yaml` has `tidymd@0.2.0`.

**Commit:** `Add the Markdown converter for agent docs`

---

## S2 — Generated skill references

**Goal:** `skills/cetra/references/` is generated from the docs pages listed in `skills/references.json`, `just docs-gen` rewrites it, and `just check-docs` fails when it is stale.

**Edit:**
- `skills/references.json` (new)
- `skills/cetra/references/*.md` (new, generated)
- `website/scripts/skill-references.mjs` (new)
- `website/scripts/skill-references.test.mjs` (new)
- `website/package.json`
- `justfile`

**Steps.**
1. Create `skills/references.json`. Each `page` is relative to `website/src/content/docs/docs/`. Write it exactly:

   ```json
   {
     "references": [
       { "name": "python", "page": "reference/python/index.mdx" },
       { "name": "python-tracing", "page": "reference/python/tracing.mdx" },
       { "name": "python-context", "page": "reference/python/context.mdx" },
       { "name": "python-opentelemetry", "page": "reference/python/opentelemetry.mdx" },
       { "name": "typescript", "page": "reference/typescript/index.mdx" },
       { "name": "typescript-tracing", "page": "reference/typescript/tracing.mdx" },
       { "name": "typescript-context", "page": "reference/typescript/context.mdx" },
       { "name": "typescript-opentelemetry", "page": "reference/typescript/opentelemetry.mdx" },
       { "name": "typescript-runtimes", "page": "reference/typescript/runtimes.mdx" },
       { "name": "opentelemetry", "page": "get-started/opentelemetry.mdx" },
       { "name": "otlp", "page": "reference/otlp.mdx" },
       { "name": "attributes", "page": "reference/attributes.md" },
       { "name": "integrations", "page": "integrations/index.mdx" },
       { "name": "openai", "page": "integrations/openai.mdx" },
       { "name": "anthropic", "page": "integrations/anthropic.mdx" },
       { "name": "bedrock", "page": "integrations/bedrock.mdx" },
       { "name": "google-genai", "page": "integrations/google-genai.mdx" },
       { "name": "google-adk", "page": "integrations/google-adk.mdx" },
       { "name": "langchain", "page": "integrations/langchain.mdx" },
       { "name": "llama-index", "page": "integrations/llama-index.mdx" },
       { "name": "litellm", "page": "integrations/litellm.mdx" },
       { "name": "openai-agents", "page": "integrations/openai-agents.mdx" },
       { "name": "crewai", "page": "integrations/crewai.mdx" },
       { "name": "dspy", "page": "integrations/dspy.mdx" },
       { "name": "vercel-ai-sdk", "page": "integrations/vercel-ai-sdk.mdx" },
       { "name": "openinference", "page": "integrations/openinference.mdx" },
       { "name": "openllmetry", "page": "integrations/openllmetry.mdx" },
       { "name": "opentelemetry-genai", "page": "integrations/opentelemetry-genai.mdx" },
       { "name": "from-langfuse", "page": "integrations/from-langfuse.mdx" },
       { "name": "from-laminar", "page": "integrations/from-laminar.mdx" },
       { "name": "sessions", "page": "tracing/sessions.mdx" },
       { "name": "users", "page": "tracing/users.mdx" },
       { "name": "metadata-and-tags", "page": "tracing/metadata-and-tags.mdx" },
       { "name": "privacy-controls", "page": "tracing/privacy-controls.mdx" },
       { "name": "sql-schema", "page": "reference/sql-schema.md" },
       { "name": "troubleshooting", "page": "help/troubleshooting.mdx" }
     ]
   }
   ```

   Before writing it, check that each `page` exists under `website/src/content/docs/docs/`. If one does not, stop and report.
2. Create `website/scripts/skill-references.mjs`. Header comment: "Generates skills/cetra/references/ from the docs pages that skills/references.json lists (D-AO-8). Checks them by default; CETRA_DOCS_WRITE=1 rewrites them (`just docs-gen`)." Its exports:

   ```js
   /** Paths relative to website/ (the cwd of `pnpm check`). */
   export const LIST = "../skills/references.json";
   export const PAGES = "src/content/docs/docs";
   export const OUT = "../skills/cetra/references";

   /** The header line of every generated file. */
   export function header(page) {
     return `<!-- Generated from website/src/content/docs/docs/${page} by website/scripts/skill-references.mjs. Do not edit: run \`just docs-gen\`. -->\n\n`;
   }

   /**
    * Validates the parsed references.json; returns error messages (empty when valid).
    * Each entry needs `name` matching /^[a-z0-9]+(-[a-z0-9]+)*$/ and a string `page`
    * ending in `.md` or `.mdx`; names are unique; `pageExists(page)` must be true.
    */
   export function validateList(list, pageExists) {}

   /** The expected files: Map of `<name>.md` → header(page) + toMarkdown(readPage(page)). */
   export function expectedFiles(list, readPage) {}

   /**
    * Compares expected and actual (Map of file name → text) and returns messages:
    * `skills/cetra/references/<f> is stale: run \`just docs-gen\``,
    * `skills/cetra/references/<f> is missing: run \`just docs-gen\``,
    * `skills/cetra/references/<f> is not in skills/references.json: run \`just docs-gen\``.
    * Sorted by file name.
    */
   export function diffFiles(expected, actual) {}

   export function main(write) {}
   ```

3. **`main(write)`:**
   1. Read and parse `LIST`. A missing file, invalid JSON, or a `references` that is not an array prints `skills/references.json: <reason>` and returns 1.
   2. Run `validateList(list.references, (p) => existsSync(join(PAGES, p)))`. Errors are printed as `skills/references.json: <message>` and return 1.
   3. Call `expectedFiles`. A converter error prints `<page>: <message>` and returns 1.
   4. **Write mode** (`write` true): create `OUT` if needed, delete every `*.md` in `OUT` that is not expected, write every expected file, print `wrote <n> references` to stderr, and return 0.
   5. **Check mode:** read every `*.md` in `OUT` into a Map (an empty Map if `OUT` is missing), print each message of `diffFiles` to stderr, and return 1 if there are any, else 0.
   6. The entry guard: `process.exit(main(Boolean(process.env.CETRA_DOCS_WRITE)))`.
4. In `website/package.json`, append ` && node scripts/skill-references.mjs` to the end of the `check:content` script.
5. In the `justfile`:
   - make `docs-gen` depend on `js-install` (`docs-gen: js-install`);
   - add the line `cd website && CETRA_DOCS_WRITE=1 node scripts/skill-references.mjs` after its two cargo lines;
   - update its comment to "Regenerate the generated docs pages (D-W-7): the CLI and SQL schema references, and the skill references (D-AO-8)".
6. Generate the files: `cd website && CETRA_DOCS_WRITE=1 node scripts/skill-references.mjs`. Commit everything under `skills/cetra/references/`.
7. Open four generated files: `openai.md`, `python.md`, `troubleshooting.md` and `integrations.md`. Check by eye that they read as plain Markdown, that tab labels are bold lines, that the integration cards are a bullet list, and that no marker, `<Backing>` or JSX is left. `sql-schema.md` keeps the page's own `<!-- Generated … -->` comment under our header; that is expected. If one does not, stop and report rather than adding special cases.

**Tests** (`skill-references.test.mjs`):
- `validateList` accepts a valid list.
- `validateList` reports a duplicate name, a name with `_` or uppercase, a page without `.md`/`.mdx`, and a page for which `pageExists` returns false.
- `expectedFiles` returns `openai.md` → header plus the converted text, for a stub `readPage`.
- `diffFiles` reports stale, missing and extra files, sorted.
- `diffFiles` returns `[]` for equal Maps.
- `header("integrations/openai.mdx")` equals the exact string above.

**Done when:**
- `cd website && node --test scripts/skill-references.test.mjs` passes;
- `cd website && node scripts/skill-references.mjs` (check mode) exits 0 right after the write;
- `pnpm exec biome check --write website/scripts/skill-references.mjs website/scripts/skill-references.test.mjs` (from the repository root) leaves no error.

**Commit:** `Generate the skill references from the docs`

---

## S3 — `SKILL.md` and its check

**Goal:** the hand-written `SKILL.md`, and a check that its frontmatter, links, commands and variables are valid.

**Edit:**
- `skills/cetra/SKILL.md` (new)
- `website/scripts/check-skill.mjs` (new)
- `website/scripts/check-skill.test.mjs` (new)
- `website/scripts/check-env-vars.mjs`
- `website/scripts/check-env-vars.test.mjs`
- `website/package.json`

**Steps.**
1. Create `skills/cetra/SKILL.md` with exactly this content. The file is what lies between the outer four-backtick fence lines, with the plan's three-space indent removed from every line. The three-backtick fences inside it are part of the file.

   ````markdown
   ---
   name: cetra
   description: Add LLM tracing with cetra to an app, with the cetra Python or TypeScript SDK or plain OpenTelemetry, then check that the traces arrive. Use when the user asks to add cetra, LLM observability, or tracing of LLM calls, agents, tools or prompts, or to send OpenTelemetry GenAI spans to cetra.
   license: MIT
   allowed-tools: Bash(npx @cetra-ai/cli query:*) Bash(cetra query:*) Bash(cetra traces:*) Bash(cetra projects:*)
   metadata:
     homepage: https://cetra.sh
   ---

   # cetra

   cetra is an open-source LLM observability platform. An app sends OpenTelemetry spans to cetra, and cetra shows them as traces with their prompts, answers, tokens and cost. The cetra SDKs for Python and TypeScript set this up in a few lines, and plain OpenTelemetry works from any language.

   Follow the steps in order. Keep the change small: add tracing, change nothing else in the app.

   ## 1. Check the key and the URL

   The app needs two environment variables:

   - `CETRA_API_KEY`: a project key that starts with `sk-cetra-`.
   - `CETRA_BASE_URL`: the cetra server, for example `https://eu1.cetra.sh` or `http://localhost:8080`.

   `npx @cetra-ai/cli init` writes both to `.env`. If they are missing, ask the user to run it. Never ask the user to paste a key into the chat, never write a key into code, and never commit `.env`.

   The SDKs read the process environment, not the `.env` file. Load `.env` the way the app already loads its settings: `python-dotenv`, `node --env-file=.env`, or the framework's own loader. If the app loads nothing yet, use one of those.

   ## 2. Find the stack

   Read the project's manifest (`pyproject.toml`, `requirements.txt`, `package.json`) and the code that calls the model. Then read the matching reference pages:

   | The app uses | Read |
   |---|---|
   | Python | [python.md](references/python.md), [python-tracing.md](references/python-tracing.md) |
   | TypeScript or JavaScript | [typescript.md](references/typescript.md), [typescript-tracing.md](references/typescript-tracing.md) |
   | A serverless or edge runtime | [typescript-runtimes.md](references/typescript-runtimes.md), [python-opentelemetry.md](references/python-opentelemetry.md) |
   | Another language, or its own OpenTelemetry setup | [opentelemetry.md](references/opentelemetry.md), [otlp.md](references/otlp.md), [python-opentelemetry.md](references/python-opentelemetry.md), [typescript-opentelemetry.md](references/typescript-opentelemetry.md) |
   | OpenAI | [openai.md](references/openai.md) |
   | Anthropic | [anthropic.md](references/anthropic.md) |
   | Amazon Bedrock | [bedrock.md](references/bedrock.md) |
   | Google Gen AI | [google-genai.md](references/google-genai.md) |
   | Google ADK | [google-adk.md](references/google-adk.md) |
   | LangChain or LangGraph | [langchain.md](references/langchain.md) |
   | LlamaIndex | [llama-index.md](references/llama-index.md) |
   | LiteLLM | [litellm.md](references/litellm.md) |
   | OpenAI Agents SDK | [openai-agents.md](references/openai-agents.md) |
   | CrewAI | [crewai.md](references/crewai.md) |
   | DSPy | [dspy.md](references/dspy.md) |
   | Vercel AI SDK | [vercel-ai-sdk.md](references/vercel-ai-sdk.md) |
   | OpenInference instrumentation | [openinference.md](references/openinference.md) |
   | OpenLLMetry (Traceloop) | [openllmetry.md](references/openllmetry.md) |
   | OpenTelemetry GenAI instrumentation | [opentelemetry-genai.md](references/opentelemetry-genai.md) |
   | Langfuse today | [from-langfuse.md](references/from-langfuse.md) |
   | Laminar today | [from-laminar.md](references/from-laminar.md) |

   [integrations.md](references/integrations.md) lists every integration. A library that is not listed is still traced when it calls a listed provider.

   ## 3. Add tracing

   - Install the SDK with the project's package manager: `cetra-sdk` with the extras of the libraries the app uses (for example `cetra-sdk[openai]`), or `@cetra-ai/sdk` with the instrumentation packages its reference page names.
   - Call `init` once, at startup, before the app creates its LLM clients. Pass the instrumentations the reference page gives for each library. Leave `api_key` and `base_url` out: the SDK reads `CETRA_API_KEY` and `CETRA_BASE_URL`.
   - Wrap the app's entry point or request handler with `observe`, so each request is one trace with the LLM calls under it.
   - When the app has conversations or signed-in users, set `session_id` and `user_id` with `context` (Python) or `withContext` (TypeScript). See [sessions.md](references/sessions.md) and [users.md](references/users.md).
   - In scripts, tests and serverless handlers, call `flush()` (Python) or `await flush()` (TypeScript) before the process ends. Otherwise the last spans are lost.
   - If the app already has its own OpenTelemetry setup, keep it, and read the OpenTelemetry page of its language before changing anything.
   - To leave prompts and answers out of the traces, set `CETRA_CAPTURE_CONTENT=false` (or `capture_content=False` / `captureContent: false` in `init`); see [privacy-controls.md](references/privacy-controls.md).

   ## 4. Run the app

   Run the app or its tests once, so that it makes at least one LLM call. This needs the provider's own key, for example `OPENAI_API_KEY`. If you cannot make a real call, say so and stop here: the user runs it.

   ## 5. Check the traces

   Run:

   ```bash
   npx @cetra-ai/cli query "SELECT trace_id, name, start_time, span_count, llm_count, error_count FROM traces ORDER BY start_time DESC LIMIT 5" --since 15m --json
   ```

   The command reads the server and the project from `.cetra/project.json`, which `npx @cetra-ai/cli init` wrote. The columns are in [sql-schema.md](references/sql-schema.md).

   - **Rows come back:** check that `llm_count` is above 0 and `error_count` is 0. To see the spans of a trace, query the `spans` view with `WHERE trace_id = '<id>'`.
   - **No rows:** read "No traces at all" and "A short script loses its last spans" in [troubleshooting.md](references/troubleshooting.md). The usual causes are a missing `flush()`, a key or URL the app did not load, or `init` called after the LLM client was created.

   If the user has the `cetra` binary and has run `cetra login`, `cetra query "<sql>" --project <org>/<project> --since 15m --format json` works too. An agent connected to cetra's MCP server can run the same SQL with its `query_sql` tool.

   ## 6. Report

   Tell the user which files changed, which libraries are traced, and where to see the traces: `<CETRA_BASE_URL>/<org>/<project>/traces`.

   ## More

   - The whole documentation for agents: `https://cetra.sh/llms.txt`.
   - Search the docs with the docs MCP server at `https://cetra.sh/mcp`.
   - Span attributes cetra reads, for hand-made spans: [attributes.md](references/attributes.md) and [metadata-and-tags.md](references/metadata-and-tags.md).
   - Tracing the coding agent itself (Claude Code, Codex, Gemini CLI, Copilot) is a different setup, `cetra agents setup`: see [Claude Code](https://cetra.sh/docs/integrations/claude-code/).
   ````

2. Create `website/scripts/check-skill.mjs`. Header comment: "Checks skills/cetra/SKILL.md (D-AO-8): the Agent Skills frontmatter, links to references and docs pages, and the `cetra` subcommands it names." Its exports:

   ```js
   export const SKILL_DIR = "../skills/cetra";
   export const PAGES = "src/content/docs/docs";
   export const CLI_REFERENCE = "src/content/docs/docs/reference/cli.md";

   /** `cetra` subcommand names: the `### cetra <name>` headings of the CLI reference. */
   export function cliCommands(cliReference) {}

   /**
    * Returns `{line, message}` errors for SKILL.md's text.
    * ctx = { referenceExists(name: string) => boolean,
    *         pageExists(docsPath: string) => boolean,   // path after `/docs/`, e.g. `integrations/claude-code/`
    *         commands: Set<string> }
    */
   export function checkSkill(text, ctx) {}

   export function main() {}
   ```

3. **`cliCommands`.** The reference's command headings are level 3 with the command in backticks: ``### `cetra query` ``. Sub-commands are level 4 (``#### `cetra agents setup` ``) and are not read. For each line matching ``/^### `cetra ([a-z][a-z-]*)`/``, take the name.
4. **`checkSkill(text, ctx)`**, with each error carrying the 1-based line it concerns:
   1. **Frontmatter.** The text must start with `---\n` and have a closing `---` line. Parse it with `extractStarlightFrontmatter` from `tidymd`; a parse error is an error at line 1.
      - `name` must be the string `cetra`, which is the directory name.
      - `description` must be a string of 1 to 1024 characters.
      - `allowed-tools`, when present, must be a string.
      - Any other key except `license`, `compatibility` and `metadata` is an error (`unknown frontmatter key <k>`).
      - `extractStarlightFrontmatter` throws on invalid YAML or a YAML value that is not an object. Catch any error and report it at line 1.
      - When the frontmatter is missing or cannot be parsed, report that one error and skip the `name`, `description`, `allowed-tools` and key checks; the other rules still run.
   2. **Length.** At most 500 lines in total (`SKILL.md has <n> lines; the limit is 500`).
   3. **Reference links.** Every Markdown link target matching `/^references\/([a-z0-9-]+)\.md$/` needs `ctx.referenceExists(<name>)`; otherwise `missing reference references/<name>.md`. Any other relative link target (one that does not start with `http://`, `https://` or `#`) is an error: `link <target> must be references/<name>.md or an absolute URL`.
   4. **Docs pages.** For every `https://cetra.sh/docs/<path>` in the text, in a link target or bare, `<path>` must end with `/` and `ctx.pageExists(<path>)` must be true; otherwise `no docs page for https://cetra.sh/docs/<path>`. Cut `<path>` at `#`, `)`, `` ` ``, whitespace or the end of the line, then strip trailing `.`, `,`, `;` and `:`. Other `https://cetra.sh/` URLs (`/llms.txt`, `/mcp`) are not checked.
   5. **Commands.** Only code counts here: inline code spans and lines inside fences. Every match of `/(?:^|[\s`(])cetra ([a-z][a-z-]*)/` needs its name in `ctx.commands`; otherwise ``unknown command `cetra <name>` ``. `npx @cetra-ai/cli …` does not match, since `@cetra-ai/cli` has no space after `cetra`. Prose such as "cetra shows them" is not code, so it is not checked.
5. **`main()`:**
   1. Read `SKILL_DIR/SKILL.md`. If it is missing, print `skills/cetra/SKILL.md: missing` and return 1.
   2. `referenceExists(n)` is `existsSync(join(SKILL_DIR, "references", n + ".md"))`.
   3. `pageExists(p)` is true when one of `join(PAGES, p.slice(0, -1) + ".mdx")`, `….md`, `join(PAGES, p, "index.mdx")` or `join(PAGES, p, "index.md")` exists.
   4. `commands` is `cliCommands(readFileSync(CLI_REFERENCE, "utf8"))`. If it is empty, print `website/src/content/docs/docs/reference/cli.md: no command headings found` and return 1.
   5. Also list `SKILL_DIR`. It may hold only `SKILL.md` and `references/`, and `references/` only `*.md` files. Anything else is `skills/cetra/<path>: not part of the skill`.
   6. Print each error as `skills/cetra/SKILL.md:<line>: <message>`, and return 1 if there are any.
6. **`check-env-vars.mjs`.** In `main`, the `pages` object is keyed by absolute paths (`join(root, …)`). Add `join(root, "skills/cetra/SKILL.md")` and every `*.md` under `join(root, "skills/cetra/references")` to it, keyed the same way. Use the file's existing `walk` helper, which already skips a missing directory, with an accept function that keeps names ending in `.md`. Their `CETRA_*` names then go through the existing rule. Change nothing else.
7. In `website/package.json`, append ` && node scripts/check-skill.mjs` to the end of `check:content`, after `skill-references.mjs`.

**Tests:**
- `check-skill.test.mjs`:
  - a valid minimal skill (the frontmatter plus one reference link, one docs link and `` `cetra query` ``) gives `[]`;
  - a missing frontmatter, `name: other`, an empty `description`, a 1025-character `description`, an unknown key `version`, and a non-string `allowed-tools` each give one error;
  - 501 lines give the length error;
  - `[x](references/nope.md)` gives a missing reference; `[x](other.md)` gives the relative-link error;
  - `https://cetra.sh/docs/nope/` with `pageExists` false gives an error; `https://cetra.sh/docs/x` without a trailing slash gives an error; `https://cetra.sh/llms.txt` gives none;
  - `` `cetra frobnicate` `` gives an unknown command; `` `npx @cetra-ai/cli query` `` and the prose "cetra shows traces" give none; `cetra query` on a line inside a fence is checked;
  - `cliCommands` on ``### `cetra serve` ``, ``#### `cetra agents setup` `` and ``### `cetra query` `` gives `{serve, query}`;
  - `cliCommands` on the real `src/content/docs/docs/reference/cli.md` gives exactly `serve, admin, version, health, login, logout, query, traces, projects, agents`;
  - a docs link followed by a full stop (`see https://cetra.sh/docs/integrations/claude-code/.`) gives no error when the page exists;
  - a `compatibility` key is accepted, and a frontmatter of `- a list` reports a line-1 error instead of throwing;
  - `checkSkill` on the real `../skills/cetra/SKILL.md` with the real context gives `[]`.
- `check-env-vars.test.mjs`: add one case where a `pages` entry keyed `skills/cetra/SKILL.md` names an unknown `CETRA_NOPE` and is reported, through `checkSite`, as the existing cases do.

**Done when:**
- `cd website && node --test scripts/check-skill.test.mjs scripts/check-env-vars.test.mjs` passes;
- `cd website && node scripts/check-skill.mjs && node scripts/check-env-vars.mjs` exits 0;
- `pnpm exec biome check --write website/scripts/check-skill.mjs website/scripts/check-skill.test.mjs website/scripts/check-env-vars.mjs website/scripts/check-env-vars.test.mjs` (from the repository root) leaves no error.

**Commit:** `Add the cetra agent skill and its check`

---

## S4 — Plugin manifests, the mirror and the docs of the repository

**Goal:** `just skills-mirror <dir> [--push]` copies the skill and its manifests into a checkout of `cetra-ai/skills`. A workflow runs it on `main`. `AGENTS.md` documents the new directory and checks.

**Edit:**
- `skills/manifests/.claude-plugin/plugin.json` (new)
- `skills/manifests/.claude-plugin/marketplace.json` (new)
- `skills/manifests/.codex-plugin/plugin.json` (new)
- `skills/manifests/.cursor-plugin/plugin.json` (new)
- `skills/manifests/README.md` (new)
- `skills/manifests/LICENSE` (new)
- `tools/skills/mirror.sh` (new, executable)
- `justfile`
- `.github/workflows/skills-mirror.yml` (new)
- `AGENTS.md`
- `docs/roadmap.md`

**Steps.**
1. **Manifests**, exactly as below. None carries a `version`: the plugin hosts fall back to the commit.
   - `skills/manifests/.claude-plugin/plugin.json`:

     ```json
     {
       "name": "cetra",
       "description": "Add cetra LLM tracing to an app with the Python or TypeScript SDK or OpenTelemetry, and check that traces arrive.",
       "author": { "name": "cetra" },
       "homepage": "https://cetra.sh",
       "repository": "https://github.com/cetra-ai/skills",
       "license": "MIT",
       "keywords": ["observability", "tracing", "opentelemetry", "llm"]
     }
     ```

   - `skills/manifests/.claude-plugin/marketplace.json`:

     ```json
     {
       "name": "cetra",
       "description": "The cetra agent skill.",
       "owner": { "name": "cetra" },
       "plugins": [
         {
           "name": "cetra",
           "source": "./",
           "description": "Add cetra LLM tracing to an app and check that traces arrive."
         }
       ]
     }
     ```

   - `skills/manifests/.codex-plugin/plugin.json`: the Claude `plugin.json` above plus `"skills": "./skills/"`.
   - `skills/manifests/.cursor-plugin/plugin.json`: the Claude `plugin.json` above plus `"displayName": "cetra"`, `"category": "developer-tools"` and `"skills": "./skills/"`.
2. **`skills/manifests/README.md`.** About 25 lines, in STYLE.md's voice. It says:
   - what the skill does;
   - how to install it:
     - `npx @cetra-ai/cli init` installs it with the rest of the setup;
     - `npx skills add cetra-ai/skills`;
     - Claude Code: `/plugin marketplace add cetra-ai/skills` then `/plugin install cetra@cetra`;
     - Codex and Cursor: their plugin settings;
   - that the repository is a copy of `skills/` in `https://github.com/cetra-ai/cetra`, updated on every change there, so issues and pull requests go to that repository.
3. **`skills/manifests/LICENSE`.** The MIT licence text of the root `LICENSE`: its `MIT License` part, with the same copyright line, without the paragraph about `ee` directories.
4. **`tools/skills/mirror.sh`** (bash, `set -euo pipefail`):
   - **Usage.** `mirror.sh <dest> [--push]`, run from the repository root. Print usage and exit 2 when `<dest>` is missing or is not a directory with a `.git` entry.
   - **Copy:**
     1. Remove `<dest>/skills/cetra`, `<dest>/.claude-plugin`, `<dest>/.codex-plugin`, `<dest>/.cursor-plugin`, `<dest>/README.md` and `<dest>/LICENSE`.
     2. Copy `skills/cetra` to `<dest>/skills/cetra`.
     3. Copy the three dot-directories and the two files from `skills/manifests/` to `<dest>/`.
     4. Touch nothing else in `<dest>`.
   - **With `--push`:**
     1. `git -C <dest> add -A`.
     2. If `git -C <dest> diff --cached --quiet` succeeds, print `skills: nothing to mirror` and exit 0.
     3. Otherwise commit with `-c user.name="cetra skills mirror" -c user.email="41898282+github-actions[bot]@users.noreply.github.com"` and the message `Mirror cetra-ai/cetra@${SOURCE_SHA:-$(git rev-parse HEAD)}`.
     4. `git -C <dest> push origin HEAD`.
5. **`justfile`.** Add, near `docs-gen`:

   ```
   # Copy the agent skill and its plugin manifests into a checkout of cetra-ai/skills (D-AO-9); --push commits and pushes
   skills-mirror dest *flags:
       tools/skills/mirror.sh {{dest}} {{flags}}
   ```

6. **`.github/workflows/skills-mirror.yml`.** Use the SHAs that `ci.yml` and `.github/actions/js/action.yml` already pin: `actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1` and `taiki-e/install-action@cfdb7ed4ccc7c7dce37759eae89dc5674b469b39 # v2.90.3`.

   ```yaml
   # Mirrors skills/ to the public cetra-ai/skills repository (D-AO-9). Outside ci-ok.
   # Needs the SKILLS_MIRROR_KEY secret: a deploy key with write access to cetra-ai/skills.
   name: skills-mirror

   on:
     push:
       branches: [main]
       paths: ["skills/**", "tools/skills/**", ".github/workflows/skills-mirror.yml"]
     workflow_dispatch:

   permissions:
     contents: read

   concurrency:
     group: skills-mirror
     cancel-in-progress: false

   jobs:
     mirror:
       runs-on: ubuntu-latest
       timeout-minutes: 10
       steps:
         - id: key
           env:
             KEY: ${{ secrets.SKILLS_MIRROR_KEY }}
           run: |
             if [ -n "$KEY" ]; then
               echo "present=true" >> "$GITHUB_OUTPUT"
             else
               echo "present=false" >> "$GITHUB_OUTPUT"
               echo "::notice::SKILLS_MIRROR_KEY is not set; nothing mirrored"
             fi
         - if: steps.key.outputs.present == 'true'
           uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
           with:
             persist-credentials: false
         - if: steps.key.outputs.present == 'true'
           uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
           with:
             repository: cetra-ai/skills
             path: mirror
             ssh-key: ${{ secrets.SKILLS_MIRROR_KEY }}
             persist-credentials: true # the push below needs the key; nothing is uploaded
         - if: steps.key.outputs.present == 'true'
           uses: taiki-e/install-action@cfdb7ed4ccc7c7dce37759eae89dc5674b469b39 # v2.90.3
           with:
             tool: just@1.59.0
         - if: steps.key.outputs.present == 'true'
           env:
             SOURCE_SHA: ${{ github.sha }}
           run: just skills-mirror mirror --push
   ```

   The first checkout puts the repository at the workspace root, and the second puts the mirror under `mirror/` inside it. `mirror/` is never committed to this repository: the job ends after the push.

   If `actionlint` or `zizmor` (`just check-workflows`) asks for a change, apply it and report it. The cold review ran both on this YAML, and both were clean.
7. **`AGENTS.md`:**
   - **§ Layout:** add the line `skills/                      the cetra agent skill (skills/cetra/), its reference list and plugin manifests; mirrored to cetra-ai/skills (D-AO-9)` after the `sdks/typescript/` line.
   - **§ Tests:** add a bullet after the `CETRA_DOCS_WRITE=1` one: "`skills/cetra/references/` is generated from the docs pages that `skills/references.json` lists. A docs change that touches one of them runs `just docs-gen` and commits the result; `just check-docs` fails otherwise (`skill-references.mjs`). `check-skill.mjs` checks `SKILL.md`'s frontmatter, links and `cetra` subcommands."
   - **§ Commands:** add the row `| \`just skills-mirror <dir> [--push]\` | copy the agent skill and its manifests into a checkout of cetra-ai/skills; \`--push\` commits and pushes (D-AO-9) |` after the `docs-gen` row, and change the `docs-gen` row's text to "regenerate the site's generated pages (the CLI reference, D-W-7) and the skill references (D-AO-8)".
8. **`docs/roadmap.md`:** in the AO row's Status column, write `**Design** approved; M2 implemented (the skill, its checks and the mirror).` If M1 is already marked implemented there, keep its words and add M2's.

**Tests:** none new; the script is exercised by hand:
- `mkdir -p /tmp/claude-1000/mirror-test && git -C /tmp/claude-1000/mirror-test init -q && just skills-mirror /tmp/claude-1000/mirror-test` leaves `skills/cetra/SKILL.md`, `.claude-plugin/plugin.json` and `LICENSE` there;
- `tools/skills/mirror.sh` without arguments, and with a directory that has no `.git`, exits 2; `just skills-mirror` without arguments fails (just itself refuses the missing argument).

**Done when:**
- both manual runs above behave as described;
- `just check-workflows` passes, if `actionlint` and `zizmor` are installed; otherwise say so in the report;
- `shellcheck tools/skills/mirror.sh` passes, if `shellcheck` is installed;
- `claude plugin validate skills/manifests`, if `claude` is installed; otherwise say so in the report. A warning that no version is set is expected (the manifests carry none on purpose); report other warnings, and stop only on an error.

**Commit:** `Mirror the agent skill to cetra-ai/skills`

---

## After S4 (the coordinating session)

- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check`, once.
- A Sonnet code review of S1–S4.
