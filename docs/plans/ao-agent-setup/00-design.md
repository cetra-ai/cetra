Status: design, for review with the user (2026-11-21). No plan is written yet.

# AO · Agent-led setup — design

Track AO lets a developer set up cetra with **one command and one prompt to their coding agent**. The command signs in, picks the project, writes the key to `.env` and installs a cetra skill. The prompt tells the agent (Claude Code, Codex, Cursor, Gemini CLI, Copilot) to instrument the app, run it and check that the traces arrived.

```bash
curl -fsSL https://cetra.sh/install.sh | sh
cetra init
```

`cetra init` ends by printing a prompt like this:

> Use the cetra skill to add cetra tracing to this app. Use the cetra SDK for this app's language; the API key and URL are in `.env`. Run the app once. Check that traces arrived with `cetra query "SELECT name, start_time FROM traces ORDER BY start_time DESC LIMIT 5" --project acme/demo --since 15m`, then give me the link `https://eu1.cetra.sh/acme/demo/traces`.

The launch plan measures **time to first trace**: under 5 minutes self-hosted and under 2 minutes on the cloud. This track is how we hit those numbers for users who work with a coding agent.

**Read first:**
- `AGENTS.md`.
- `docs/mvp-decisions.md`: D140 (CLI login), D-P-70 (session-only routes), D-H-2–D-H-4 and D-H-12 (MCP and the CLI query commands), D170 (cells and the control plane), D-W-2–D-W-4, D-W-11 and D-W-19 (docs site, Worker, quick start, search, `llms.txt`), D-CA-10 (`cetra agents setup`).
- Specs:
  - 08 § CLI login and § Access;
  - 09 § Orgs, projects, members, keys, § MCP server and § CLI;
  - 10 § Configuration;
  - 11 (the `/cli-auth` page);
  - 12 (configuration, `init.*`).
- `crates/cli/src/{main,login,credentials,client,query}.rs`, `crates/cli/src/agents/setup.rs`.
- `web/src/routes/cli-auth.tsx`.
- `website/astro.config.mjs`, `website/wrangler.jsonc`, `website/src/content/docs/docs/get-started/quickstart.mdx`.
- Competitor notes: `ao-competitors.md` in the session notes folder (Laminar's `lmnr-cli setup`, Langfuse's skills and docs MCP).

This document holds the decisions, the contracts and the milestones. Each milestone gets its own plan, written just before it starts, from the code that exists then. Haiku implements the plans one phase at a time; Sonnet reviews.

## What the competitors do

**Laminar** (`npx lmnr-cli@latest setup`, a Node CLI):
1. Sign-in uses an OAuth device flow: it prints a URL and a code, and you approve in the browser.
2. You pick the project in the terminal. When you have none, a create-project step appears in the browser.
3. It mints a project key that never expires and writes `LMNR_PROJECT_API_KEY` to `./.env`.
4. It downloads the skill from the `main` branch of `lmnr-ai/lmnr-skills` into `.claude`, `.cursor`, `.codex` and `.agents`, under `skills/laminar/`.
5. It prints a five-step prompt: set up, instrument, run, check with `lmnr-cli sql query … --json`, then open the browser.
6. Re-runs reuse the existing key. `--json` and distinct exit codes let an agent run it.

**Langfuse** has no setup CLI:
- The user sets the keys. Their prompt only installs the skill, from `langfuse/skills` with `npx skills add`, or through the Claude, Codex and Cursor plugin manifests kept in that repo.
- The skill is thin and hand-written, and sends the agent to the live docs. Every docs page is also served as `.md`.
- `langfuse.com/api/mcp` is a docs MCP without auth, with tools to search the docs and fetch a page.

**What we take, and what we leave:**
- From Laminar we take one command that leaves the project ready, the device flow, `--json` and exit codes for agents, and checking with SQL.
- From Langfuse we take a thin skill that sends the agent to the docs, a `.md` copy of every page, and a docs MCP.
- We leave Laminar's keys that never expire and its pickers that run in the terminal: our project picker runs in the browser, so creating a key still needs a browser session (D-P-70).
- We leave skills downloaded from a moving branch: ours is built into the binary, so the skill matches the CLI that installs it.

## Goals

1. **`cetra init`.** One command that:
   - signs in;
   - picks or creates a project, in the browser;
   - creates an ingest-only project key and writes `CETRA_API_KEY` and `CETRA_BASE_URL` to `.env`;
   - saves a token for `cetra query`;
   - installs the cetra skill for the coding agents;
   - prints the agent prompt.

   It works on the cloud with no flags, against a self-hosted URL, and on a laptop with no server yet: there it offers to start a local `cetra serve`.
2. **A device authorization flow** (RFC 8628) on the server. It replaces the localhost-callback login, so `cetra login` and `cetra init` work over SSH, in containers and in cloud agents.
3. **A cetra agent skill.**
   - `SKILL.md` plus reference pages taken from the docs. It teaches an agent to instrument an app with the Python or TypeScript SDK, or with plain OTLP, and then to check the result.
   - It lives in this repository and is checked against the docs in `just check-docs`.
   - It is built into the binary and mirrored to a public `cetra-ai/skills` repository, for `npx skills add` and the agents' plugin marketplaces.
4. **Docs for agents.**
   - `llms.txt` already exists (D-W-19).
   - Every page gets a `.md` copy and a "Copy page" button.
   - A docs MCP at `https://cetra.sh/mcp` with `search_docs` and `get_page`.
5. **Cloud routing.** `cetra init` with no URL goes to `app.cetra.sh`, which hands the device code to the cell that holds the user's org. Secrets never pass through the control plane.
6. **Docs and tests.**
   - A "Set up with your coding agent" page.
   - The quick start gets a coding-agent tab.
   - The CLI reference is regenerated.
   - An end-to-end test from `cetra init` to a traced span read back with `cetra query`.

## Non-goals

- **A JavaScript or Python wrapper for the CLI** (`npx @cetra-ai/cli`, `uvx cetra`). The user chose install.sh and the binary (D-AO-2). The one-liner is `curl … | sh && cetra init`.
- **Editing the app's code.** That is the coding agent's job, guided by the skill and the prompt. `cetra init` changes only `.env`, `.gitignore` and the skill directories.
- **Changing the agent's MCP settings.** `cetra init` does not register the trace MCP (`/mcp`) or the docs MCP in the agent's config files: that needs a token in the agent's config, and every agent keeps that config differently. The skill names both MCPs, and the docs page shows how to add them.
- **Tracing the coding agent itself.** That is `cetra agents setup` (track CA). The two commands share nothing but the binary, and the docs keep them apart (D-AO-1).
- **AI search on the docs site** (D-W-11 stands): the docs MCP returns pages and snippets, with no model behind it.
- **A Windows build of `cetra init`.** It follows the release targets (D-N-1: Linux and macOS).

## Architecture

```
developer's terminal                 browser                       cell (cetra serve)
─────────────────────               ─────────                     ──────────────────
cetra init ──POST /api/auth/device/code──────────────────────────► device store (in memory)
   │  prints the code and the URL                                    │
   │                          /device?code=… ── session ────────────►│ approve: project,
   │                          picks or creates the project           │ mints the PAT and the
   │                                                                 │ ingest key, holds them
   ├──POST /api/auth/device/token (polls) ──────────────────────────►│ hands them over once
   ├─ writes .env, ~/.config/cetra/credentials.toml
   ├─ installs .claude/skills/cetra and .agents/skills/cetra (built into the binary)
   └─ prints the prompt ──► coding agent ── reads the skill, cetra.sh/llms.txt, cetra.sh/mcp
                                 └─ instruments, runs the app, `cetra query` checks the traces
```

On the cloud, step one goes to `app.cetra.sh` instead. The control plane signs the user in, finds the org's cell and registers the code there; the CLI is then sent on to the cell (§ Cloud routing).

### Parts

| Part | Where | Milestone |
|---|---|---|
| Device flow: code store, routes, approval | `crates/server` (`auth`, `http`), `openapi.json` | M1 |
| `/device` approval page; `/cli-auth` deleted | `web/` | M1 |
| `cetra login` on the device flow; the callback deleted | `crates/cli/src/login.rs` | M1 |
| The skill | `skills/cetra/` (new), `website/scripts/` (generate and check) | M2 |
| Mirror to `cetra-ai/skills` | `.github/workflows/skills-mirror.yml` (new), `justfile` | M2 |
| `cetra init` | `crates/cli/src/init/` (new) | M3 |
| `.md` pages, Copy page, docs MCP | `website/` (plugin, Worker script, build script) | M4 |
| Internal route for the control plane | `crates/server` | M5 |
| The relay on `app.cetra.sh` | the private `cetra-control` repository (track Q) | Q's request |

## Contracts

### Device flow (M1)

The routes follow RFC 8628, plus a project step and the cetra secrets. Every route is in `openapi.json`.

| Route | Access | Body → answer |
|---|---|---|
| `POST /api/auth/device/code` | public, rate-limited per IP (10 a minute) | `{purpose: "login" \| "init", host?: string}` → `200 {device_code, user_code, verification_uri, verification_uri_complete, expires_in: 600, interval: 5}` |
| `POST /api/auth/device/token` | public | `{device_code}` → `200 DeviceGrant`, or `400 {error}` with `error` ∈ `authorization_pending`, `slow_down`, `access_denied`, `expired_token` (RFC 8628 § 3.5 names) |
| `GET /api/auth/device/{user_code}` | session | `{purpose, host?, created_at}` for the approval page; 404 when unknown or expired |
| `POST /api/auth/device/{user_code}/approve` | session only | `{project_id?}`: required for `init`, refused for `login` → `204` |
| `POST /api/auth/device/{user_code}/deny` | session only | → `204` |

```
DeviceGrant = {
  server: string,              // the cell's public_url: what the CLI and the SDK talk to
  token: string,               // pat-cetra-…, 90 days
  project?: {id, org_slug, slug, name},   // init only
  ingest_key?: string,         // sk-cetra-…, scope ingest only; init only
}
```

- **Codes.**
  - `device_code` is 32 random bytes in base64url. The store keeps only its SHA-256.
  - `user_code` is 8 characters from `BCDFGHJKLMNPQRSTVWXZ`, shown as `XXXX-XXXX`, matched ignoring case and the dash.
  - Both expire after 10 minutes. `verification_uri` is `<public_url>/device`, and `verification_uri_complete` adds `?code=XXXX-XXXX`.
- **Polling.** A `token` call within `interval` of the previous one answers `slow_down` and adds 5 s to that code's interval.
- **Approval** mints the secrets on the server, at approval time, as the session user:
  - `login`: a PAT with scopes `read, write`, as `cetra login` gets today.
  - `init`: a PAT with scope `read`, which is what `cetra query` and the trace MCP need. It also mints an ingest-only project key named `cetra init (<host>)`. The user needs a project role that may create keys; otherwise the approval answers 403.

  The PAT's name is `cetra <purpose> on <host>`, as today.
- **Hand-over.** The plaintext secrets stay **in memory only**, in the code's entry, until the first successful `token` call takes them, at most 10 minutes. They are never written to disk or logged.
  - A restart loses pending codes. The CLI then gets `expired_token` and says to run the command again.
  - The PAT and the key already exist in `meta.db` and appear on the account and project pages, where the user can revoke them.
- **Bounds.**
  - At most 10 000 pending codes per process; one more answers `503`.
  - Expired entries are swept every minute.
  - A denied or expired code never hands anything over.
- **Phishing.** The approval page shows the `host` the CLI reported and the purpose, warns "Only continue if you just ran `cetra init` (or `cetra login`) on this computer", and needs an explicit click. The code alone never approves.
- **Approval page** (`/device`, web):
  1. It asks for the code, or reads `?code=`. A user who is not signed in goes through login first and comes back.
  2. For `init`, it lists the orgs and projects the user can create keys in, offers "New project" (name, then the org's project-creation route), and preselects the only project when there is one.
  3. Approve or Deny.
- **Deleted:** the `/cli-auth` page, its tests, the localhost callback server in `login.rs`, and the callback's sections in spec 08. There is no compatibility path (`AGENTS.md`).

### `cetra init` (M3)

```
cetra init [--url <URL>] [--project <org/project>] [--dir <path>] [--env-file <path>]
           [--agents claude,agents | --no-skill] [--no-browser] [--yes] [--json]
```

**Steps, in order:**

1. **Choose the server.** The URL comes from `--url`, then `CETRA_URL`, then `https://app.cetra.sh` (the cloud).
   - On a loopback URL with no answer on `/readyz`, init offers to start a local server (§ Local server). With `--yes` it starts one without asking. Without a terminal and without `--yes` it fails with exit code 3.
   - Any other URL with no answer fails with exit code 3.
2. **Run the device flow** with `purpose: init`. It prints the code and the URL and opens the browser, unless `--no-browser` is given or there is no display. Then it polls.
   - `--project` preselects that project on the approval page (`&project=org/slug` is added to the URL). The user still approves.
   - On the cloud the first answers come from `app.cetra.sh` (§ Cloud routing).
3. **Save the PAT** in `~/.config/cetra/credentials.toml` under the grant's `server`, as `cetra login` does. It replaces the token saved for that server.
4. **Write the env file** (`--env-file`, default `<dir>/.env`, where `--dir` defaults to the current directory). Init sets two variables, the SDK's own (spec 10):

   ```
   CETRA_API_KEY=sk-cetra-…
   CETRA_BASE_URL=https://eu1.cetra.sh
   ```

   - **Existing lines.** A line that already sets one of the two variables is replaced in place. Other lines, comments and their order are kept. The file is written 0600 through a temporary file and a rename. A new file gets a `# cetra (cetra init)` comment above the two lines.
   - **An existing key.** When the file already has a `CETRA_API_KEY` for the same server, init asks before replacing it. With `--yes` it replaces it. The key the grant brought stays valid on the server either way.
   - **Git.** When `<dir>` is inside a git work tree and the env file is not ignored (`git check-ignore`), init adds its name to `<dir>/.gitignore` and says so. Init runs no other git command.
5. **Install the skill** (§ Skill install).
6. **Print the agent prompt** (§ Prompt) and the project's traces URL. With `--json`, stdout carries one JSON object and nothing else:

   ```json
   {"server": "…", "project": "org/slug", "env_file": "/abs/.env",
    "skills": ["/abs/.claude/skills/cetra"], "prompt": "…", "traces_url": "…"}
   ```

   It never carries a secret.

**Exit codes:**

| Code | Meaning |
|---|---|
| 0 | done |
| 1 | other error |
| 2 | usage error (clap) |
| 3 | server not reachable, or the local server would not start |
| 4 | sign-in denied, or the code expired |
| 5 | the env file could not be written |

Skill install failures are warnings, not errors: the setup works without the skill.

**Privacy.** Init never prints or logs the key or the token. `--json` carries neither. Error messages name files by their base name, as `credentials.rs` does.

### Local server (M3)

When the URL is loopback (the default for a self-hosted trial is `http://localhost:8080`) and nothing answers, init asks: "No cetra server at http://localhost:8080. Start one on this machine? [Y/n]".

1. **The data directory** is `<data_local_dir>/cetra/local`: `~/.local/share/cetra/local` on Linux, `~/Library/Application Support/cetra/local` on macOS.
   - Init writes `cetra.toml` there, with `data_dir`, `listen` and `public_url`.
   - When the directory already holds data, init only starts the server: the `init.*` values below apply on a first start only (spec 12).
2. **First-start values** go in the environment of the child, never in the file:
   - `CETRA_INIT_ADMIN_EMAIL`, asked for at the prompt; `--yes` uses `admin@localhost`;
   - a generated 24-character `CETRA_INIT_ADMIN_PASSWORD`;
   - `CETRA_INIT_ORG=local`;
   - `CETRA_INIT_PROJECT`, the base name of `<dir>` made into a slug, else `default`;
   - a generated `CETRA_INIT_SECRET_KEY`.
3. **Start.** Init starts `cetra serve --config <dir>/cetra.toml` as a detached child (its own session, `setsid`). Its output goes to `<data dir>/server.log` and its PID to `<data dir>/server.pid`. Then init waits up to 30 s for `/readyz`.
4. **Sign in without a browser.** On a first start init knows the password and the key. It signs in with `POST /api/auth/login`, creates the `read` PAT with that session (`POST /api/v1/me/tokens`), then signs out.
   - The device flow is skipped, and the key is the generated `CETRA_INIT_SECRET_KEY`.
   - Init prints the admin email and password once, with the UI's URL.
   - On a later start, when the data was already there, it runs the device flow as on any server.
5. **The last lines** print how to stop the server (`kill <pid>`) and how to start it again (`cetra serve --config …`). There is no `cetra stop` command: one `kill` does it.

### Skill install (M3)

- **Where.** The skill is built into the binary (`include_str!` of every file under `skills/cetra/`; a test checks that the list matches the directory). It is written to:
  - `<root>/.claude/skills/cetra/`, read by Claude Code;
  - `<root>/.agents/skills/cetra/`, read by Codex, Cursor, Gemini CLI, Copilot and OpenCode (the shared project path the agents settled on; see the competitor notes for sources).

  `<root>` is the git work tree's top level when `<dir>` is inside one, else `<dir>`. `--agents` picks a subset (`claude`, `agents`), and `--no-skill` skips the step.
- **Ownership.** Init writes a `.cetra-skill` file with the binary's version next to `SKILL.md`. It replaces a directory that has this file. A directory without it is not ours: init leaves it alone and warns.
- **Updates.** Running `cetra init` again with a newer binary updates the skill. The mirror (M2) serves users who install skills another way.

### Prompt (M3)

The prompt is a template in `crates/cli`. Init fills it with the project (`org/slug`), the server, the env file's name and the traces URL. It names the skill and gives these steps:

1. Read the cetra skill. If it is not installed, read `https://cetra.sh/llms.txt`.
2. Find the app's language and LLM libraries. Install the cetra SDK with the matching extras or instrumentors. Call `init` once at startup, reading the key and URL from the environment (`.env`). Never paste the key into code or chat.
3. Run the app, or its tests, once so that it makes at least one LLM call. Flush before exit in short scripts.
4. Check with `cetra query "<sql>" --url <server> --project <org/slug> --since 15m --format json`. If it finds no rows, follow the skill's troubleshooting steps.
5. Report what changed and give the traces URL.

The same text is in the docs page, with placeholders, so users of another setup can copy it.

### The skill (M2)

```
skills/cetra/
  SKILL.md                    hand-written
  references/                 generated from the docs; never edited by hand
    python.md  typescript.md  opentelemetry.md  integrations.md
    <one page per integration: openai.md, anthropic.md, langchain.md, vercel-ai-sdk.md, …>
    verify.md  troubleshooting.md
skills.json                   the list: reference name → docs page path
```

**`SKILL.md`** has the Agent Skills frontmatter:
- `name: cetra`;
- `description`, which says when to load it: adding LLM tracing or observability, cetra, OpenTelemetry for LLM apps;
- `allowed-tools`, limited to read-only commands (`cetra query`, `cetra traces`, `cetra projects`).

Its body is short (about 150 lines) and **routes**:
- how to detect the language and the LLM libraries, and which reference page to read for each;
- the rules: the key comes from the environment, flush in scripts, `capture_content`, never paste the key into code or chat;
- how to check the result with `cetra query`, and the trace MCP as an alternative;
- when to read `https://cetra.sh/llms.txt` or use the docs MCP.

**References are generated** from the docs' MDX by the same converter that produces the site's `.md` pages (M4 adds it; M2 adds the script and M4 reuses it). `website/scripts/skill-references.mjs` reads `skills.json`, converts each page, rewrites site links to absolute `https://cetra.sh/...` URLs, and writes `skills/cetra/references/`.

**Checks** in `just check-docs`:
- the generated references must equal what is committed (`CETRA_DOCS_WRITE=1` rewrites them, through `just docs-gen`, like the CLI reference);
- every `CETRA_*` name and `cetra` subcommand that `SKILL.md` names must exist (`check-env-vars.mjs`, extended to `skills/`);
- every link in `SKILL.md` must point at an existing docs page.

**Mirror** (`skills-mirror.yml`, on pushes to `main` that touch `skills/`): a `just skills-mirror` recipe copies `skills/cetra/` into a checkout of `cetra-ai/skills` and commits it there with the source commit's SHA, using a deploy key kept in a repository secret. It runs outside `ci-ok`. That repository also holds the plugin manifests (`.claude-plugin/marketplace.json`, `.codex-plugin/`, `.cursor-plugin/`). They are kept in `skills/manifests/` in this repository and copied with the skill. Users then install with `npx skills add cetra-ai/skills`, or from the Claude Code plugin marketplace.

### Docs for agents (M4)

- **`llms.txt`, `llms-full.txt`, `llms-small.txt`**: already served by `starlight-llms-txt` 0.12.0 (D-W-19), the latest version on 2026-11-21. No change.
- **Markdown pages and the "Copy page" button**:
  - `starlight-page-actions` 0.7.1 (latest on 2026-11-21; peer `@astrojs/starlight >=0.36.0`, we have 0.42.10). Its build copies every page as cleaned Markdown at `<page>.md` and adds a "Copy Markdown" button and an "Open in" menu (Claude, ChatGPT).
  - Configured **without `baseUrl`**: with one set it also writes `llms.txt`, which would clash with `starlight-llms-txt`.
  - It overrides `PageTitle` and `TableOfContents`. M4's plan checks that this works with our `TwoColumnContent` override.
  - The skill references reuse its converter (`tidymd`, its dependency), so the skill and the `.md` pages say the same thing.
- **Docs MCP** at `https://cetra.sh/mcp`:
  - **Where.** A Worker script is added to `cetra-site`. `run_worker_first: ["/mcp"]` sends only that path to the script; everything else stays static assets.
  - **Protocol.** `@modelcontextprotocol/sdk` 1.32.2, streamable HTTP, stateless, JSON responses, no auth. Cloudflare's `agents` 0.34.0 is rejected: it brings Durable Objects for sessions we do not need.
  - **Tools:**

    | Tool | Input | Output |
    |---|---|---|
    | `search_docs` | `query` (1–200 chars), `limit?` (1–20, default 8) | `[{title, url, snippet}]`, best first |
    | `get_page` | `url` (a `https://cetra.sh/docs/…` URL or a `/docs/…` path) | the page's Markdown (its `.md` asset), capped at 100 000 characters with a note when cut |

  - **Search** is MiniSearch 7.2.0 over the `.md` pages: title, headings and body, with prefix and fuzzy matching. A build script writes the index as a static asset, `/mcp-index.json`, which the script loads once per isolate.
  - **Logging.** No query or page is logged, in line with D-W-10's "no data leaves the page" spirit. Cloudflare's own request metrics are all we keep.
  - **Tests.** `node --test` covers the tools against a built index (unknown page, cut page, empty query). `just check-docs` builds the index.
- **Docs pages** (M3 and M4):
  - a new "Set up with your coding agent" page under Get started, with the one-liner, what `cetra init` changes, the prompt, the manual path (`npx skills add cetra-ai/skills`, setting the variables yourself), and how to add the trace and docs MCPs to Claude Code, Codex, Cursor and Gemini CLI;
  - the quick start gets a "Coding agent" tab next to Python, TypeScript and OpenTelemetry (amends D-W-4's "without its agent-install preamble");
  - the CLI reference is regenerated (`init`, and `login` on the device flow);
  - the CA pages say "to trace the agent itself" and link the new page the other way.

### Cloud routing (M5 and Q's request)

`app.cetra.sh` is the control plane. It knows the user and the org → cell mapping (D170). Cells hold the keys.

1. **The control plane issues the code.** The CLI calls `POST https://app.cetra.sh/api/auth/device/code` and gets codes in the same shape, with `verification_uri` `https://app.cetra.sh/device`.
2. **The user picks the org on the control plane.** The user opens that URL and signs in at the control plane. When they belong to orgs on more than one cell, they pick the org. The control plane then calls the org's cell:

   `POST /api/internal/v1/device/codes`, a control-plane token with scope `device` (D-P-28), body `{device_code_sha256, user_code, purpose, host?, expires_at}`.

   The cell stores the entry as if it had issued it. The control plane then redirects the browser to `<cell>/device?code=…`, where the cell's own page approves it with a cloud session (D183 sign-in).
3. **The CLI moves to the cell.** The CLI keeps polling `app.cetra.sh` with its `device_code`. Once the code is registered on a cell, the control plane answers `400 {error: "moved", server: "<cell public_url>"}`. The CLI then polls the cell with the same `device_code`, and the cell hands the grant over.

The secrets are minted on the cell and leave it only toward the CLI: the control plane never sees a token or a key. The cell side (the internal route and the `moved` handling in the CLI) is M5. The control-plane side is a request to track Q, written to the session notes as `ao-requests-for-q.md` when M5's plan is written. Until Q ships it, `cetra init --url https://eu1.cetra.sh` works directly.

## Decisions

| ID | Topic | Decision |
|---|---|---|
| D-AO-1 | Name | **`cetra init`** sets up an app to send traces. **`cetra agents setup`** (track CA) traces a coding agent itself. They are different commands with different docs pages, and each page links the other in one sentence. Rejected: `cetra setup`, too close to `cetra agents setup`; `cetra agents init`, which would put app setup under the coding-agent command. |
| D-AO-2 | Distribution | **The existing binary and `install.sh` only** (the user, 2026-11-21). The one-liner is `curl -fsSL https://cetra.sh/install.sh \| sh` then `cetra init`. Rejected for now: an npm package with platform binaries (`npx @cetra-ai/cli`) and a PyPI wheel (`uvx`), which mean more release work. The same binary already has `cetra query` for the check step. |
| D-AO-3 | Sign-in | **A device authorization flow (RFC 8628) on the cell, and the browser picks the project** (the user, 2026-11-21). The approval page mints a 90-day PAT and, for `init`, an ingest-only project key, with the session, so creating keys stays session-only (D-P-70). Rejected: extending the localhost-callback page, which fails when the browser is on another machine (SSH, devcontainers, cloud agents); letting PATs create keys, which loosens D-P-70; Laminar's keys that never expire. |
| D-AO-4 | One sign-in flow | **`cetra login` moves to the device flow; the localhost callback and `/cli-auth` are deleted.** Two browser flows in one CLI would be two paths to maintain and test (`AGENTS.md`). Amends D140 and spec 08 § CLI login. |
| D-AO-5 | Secrets in transit | **The grant's secrets are minted at approval and held in memory only, until the first successful poll or 10 minutes.** A restart loses pending codes, and the user runs the command again. Rejected: storing them in `meta.db`, which would put plaintext secrets on disk for a 10-minute convenience. |
| D-AO-6 | Scopes | **`init` gets a `read` PAT and an `ingest` key; `login` keeps a `read, write` PAT.** The app needs only ingest. The agent's check step needs only read, and a read-only token in a coding agent's reach cannot delete data. |
| D-AO-7 | Env file | **`CETRA_API_KEY` and `CETRA_BASE_URL`, the SDK's own names (spec 10).** The existing lines are replaced in place, the file is written 0600, the file is added to `.gitignore` inside a git work tree, and an existing key is replaced only after asking. No `CETRA_PROJECT`: the SDK does not read it, and the prompt passes `--project` to `cetra query`. |
| D-AO-8 | Skill source and sync | **`skills/cetra/` in this repository**: `SKILL.md` is hand-written, and `references/` is generated from the docs pages that `skills.json` lists, by the converter behind the site's `.md` pages. `just check-docs` fails when the references are stale or `SKILL.md` names an unknown variable, command or page (the user chose this repository, mirrored, 2026-11-21). Rejected: a hand-written copy of the docs, which drifts; a skill that only links to the live docs, which fails offline and for self-hosted versions behind the site. |
| D-AO-9 | Skill delivery | **Built into the binary and installed by `cetra init` into `.claude/skills/cetra/` and `.agents/skills/cetra/` at the repository root; mirrored by CI to the public `cetra-ai/skills` repository, with the Claude, Codex and Cursor plugin manifests.** The installed skill matches the CLI version. Rejected: downloading from a moving branch at setup time (Laminar), which needs the network, is not pinned to the version, and installs code from outside the release. A directory without our `.cetra-skill` marker is never overwritten. |
| D-AO-10 | Check step | **`cetra query` over the `traces` view**, with `--format json` and a 15-minute window. Every cetra user already has the CLI, and the PAT from `init` is enough. The trace MCP is mentioned in the skill as an alternative but not configured (non-goal). |
| D-AO-11 | Local server | **On a loopback URL with nothing listening, `cetra init` offers to start `cetra serve` detached**, with a data directory under the user's data dir and generated `CETRA_INIT_*` values (the user, 2026-11-21). On a first start it signs in with the generated password, without a browser, and prints the password once. Rejected: printing the start command and exiting, which is one more step before the first trace. |
| D-AO-12 | Markdown pages | **`starlight-page-actions` 0.7.1, without `baseUrl`**: `.md` copies of every page, "Copy Markdown" and "Open in Claude / ChatGPT". `llms.txt` stays with `starlight-llms-txt` (D-W-19). Rejected: `starlight-dot-md` 0.2.1 (no button), and a hand-written component (we would own the MDX-to-Markdown conversion). |
| D-AO-13 | Docs MCP | **In this track** (the user, 2026-11-21). The tools are `search_docs` and `get_page` on the `cetra-site` Worker at `/mcp`: stateless, no auth, MiniSearch over the `.md` pages, nothing logged. Amends D-W-3: the Worker gains a script for that one path. D-W-11 stands: search without a model. |
| D-AO-14 | Cloud routing | **`app.cetra.sh` issues the code and registers it on the org's cell through a control-plane call. The CLI is sent on to the cell with `moved`, and the secrets never pass through the control plane** (the user chose routing through `app.cetra.sh`, 2026-11-21). The cell side is in this track; the control-plane side is a request to track Q. Rejected: asking the region in the CLI, which hard-codes cell URLs in the binary. |
| D-AO-15 | Agent-friendly CLI | **`--json` (one object on stdout, no secrets), `--yes`, `--no-browser`, and fixed exit codes 3–5** for an agent that runs `cetra init` itself. |

## Milestones

M1 and M2 do not depend on each other. M4 depends only on M2's script (it can run after or alongside M2's last phase). M3 needs M1 and M2. M5 needs M1 and M3.

### M1 — Device flow on the server, the approval page, `cetra login` on it

**Delivers:**
- The device routes, the in-memory store and the approval with its project step, in `openapi.json` and spec 08/09.
- The `/device` page, with the project picker and "New project".
- `cetra login` on the device flow. The callback server and `/cli-auth` are deleted.
- Server tests:
  - expiry, `slow_down`, deny, a second poll after the hand-over, the cap;
  - a session-only check on approve;
  - a user without a key-creating role gets 403.
- Web tests on the MSW mocks, and a CLI test against `test_server`.

**Depends on:** nothing new.

**Leaves out:** `purpose: init` on the CLI side (M3), and the internal route (M5).

### M2 — The skill, its checks and the mirror

**Delivers:**
- `skills/cetra/SKILL.md`, `skills.json`, and the generated `references/`.
- `skill-references.mjs` with its write and check modes; the checks in `just check-docs` and the rewrite in `just docs-gen`.
- The plugin manifests in `skills/manifests/`, `just skills-mirror`, and `skills-mirror.yml`.

**Depends on:** nothing new. The converter comes in with M2: M2 adds the `starlight-page-actions` dependency for its `tidymd` converter, or `tidymd` directly; M2's plan settles which from the code then.

**Leaves out:**
- creating the `cetra-ai/skills` repository and its deploy key: the user does that (§ Open questions);
- installing the skill from the CLI (M3).

### M3 — `cetra init`

**Delivers:**
- `cetra init` with every step and flag above: the device flow with `purpose: init`, the credentials, the env file and `.gitignore`, the skill built in and installed, the prompt, `--json`, the exit codes, and the local server.
- Spec 09 § CLI, and the regenerated CLI reference.
- The "Set up with your coding agent" docs page and the quick-start tab.
- `CHANGELOG.md`.
- An end-to-end test: `cetra serve` started by `cetra init` on a temporary data dir, then a span sent with the written key, then `cetra query` with the saved token reads it back.

**Depends on:** M1 and M2.

**Leaves out:** the cloud's `moved` answer (M5).

### M4 — Docs for agents

**Delivers:**
- `starlight-page-actions` configured, with `.md` pages and the "Copy page" button.
- The docs MCP: the Worker script, the index build and its tests.
- The `wrangler.jsonc` change, and D-W-3 amended in spec and decisions.
- A docs page section on the docs MCP.

**Depends on:** M2's converter choice.

**Leaves out:** AI answers (D-W-11).

### M5 — Cloud routing, cell side

**Delivers:**
- `POST /api/internal/v1/device/codes` with the scope `device` on control-plane tokens.
- The CLI following `moved` once, to the cell named in the answer, only over `https` (or loopback in tests).
- Tests with a fake control plane.
- `ao-requests-for-q.md`, Q's side as a request.

**Depends on:** M1 and M3, and P's control-plane tokens (done).

**Leaves out:** the control-plane routes and pages, which belong to track Q in `cetra-control`.

## Open questions

1. **The `cetra-ai/skills` repository and its deploy key.** The user creates the public repository and adds the deploy key secret (`SKILLS_MIRROR_KEY`) before M2's workflow can run. Until then the workflow's job is skipped when the secret is missing.
2. **The cloud default before Q ships the relay.** Until then, `cetra init` with no URL would reach `app.cetra.sh` without the device routes. Proposed: the CLI reads a 404 there as "use `--url <your cell>`" and says so. Is that acceptable for launch, or must M5 and Q's side land before launch?
3. **The local admin email.** Init asks for it, and `--yes` uses `admin@localhost`. Should it default to `git config user.email` instead? Proposed: no, the CLI reads no personal data it does not need.
4. **Plugin marketplaces.** Should the mirror also be submitted to the Claude Code and Cursor plugin directories at launch? That is a one-off manual step for the user, not code.
