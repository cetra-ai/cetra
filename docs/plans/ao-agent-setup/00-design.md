Status: design, for review with the user (2026-11-21). No plan is written yet.

# AO · Agent-led setup — design

Track AO lets a developer set up cetra with **one command and one prompt to their coding agent**. The command signs in, picks the project, writes the key to `.env` and installs a cetra skill. The prompt tells the agent (Claude Code, Codex, Cursor, Gemini CLI, Copilot) to instrument the app, run it and check that the traces arrived.

```bash
npx @cetra-ai/cli@latest init
```

`init` ends by printing a prompt like this:

> Use the cetra skill to add cetra tracing to this app. Use the cetra SDK for this app's language; the API key and URL are in `.env`. Run the app once. Check that traces arrived with `npx @cetra-ai/cli query "SELECT name, start_time FROM traces ORDER BY start_time DESC LIMIT 5" --since 15m --json`, then give me the link `https://eu1.cetra.sh/acme/demo/traces`.

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
- `sdks/typescript/package.json` and `.github/workflows/release.yml`: how an npm package of this repository is built, versioned and published (PR #207 adds the npm publish).
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
- From Laminar we take a Node CLI run with `npx` (nothing to install), one command that leaves the project ready, the device flow, a project link file, `--json` and exit codes for agents, and checking with SQL.
- From Langfuse we take a thin skill that sends the agent to the docs, a `.md` copy of every page, and a docs MCP.
- We leave Laminar's keys that never expire and its pickers that run in the terminal: our project picker runs in the browser, so creating a key still needs a browser session (D-P-70).
- We leave skills downloaded from a moving branch: ours ships inside the npm package, so the skill matches the CLI that installs it.

## Goals

1. **`npx @cetra-ai/cli init`.** A small Node CLI, `@cetra-ai/cli`, with one command that:
   - signs in;
   - picks or creates a project, in the browser;
   - creates an ingest-only project key and writes `CETRA_API_KEY` and `CETRA_BASE_URL` to `.env`;
   - saves a read-only token and links the directory to the project, for its `query` command;
   - installs the cetra skill for the coding agents;
   - prints the agent prompt.

   It works on the cloud with no flags, against a self-hosted URL, and on a laptop with no server yet: there it offers to start a local server (the `cetra` binary or Docker).

   The CLI also has `query` (the agent's check step) and `login`. The Rust `cetra` binary stays the server and operator tool and gets no `init` (D-AO-2).
2. **A device authorization flow** (RFC 8628) on the server. It replaces the localhost-callback login, so `cetra login`, `npx @cetra-ai/cli login` and `init` work over SSH, in containers and in cloud agents.
3. **A cetra agent skill.**
   - `SKILL.md` plus reference pages taken from the docs. It teaches an agent to instrument an app with the Python or TypeScript SDK, or with plain OTLP, and then to check the result.
   - It lives in this repository and is checked against the docs in `just check-docs`.
   - It ships inside the `@cetra-ai/cli` package and is mirrored to a public `cetra-ai/skills` repository, for `npx skills add` and the agents' plugin marketplaces.
4. **Docs for agents.**
   - `llms.txt` already exists (D-W-19).
   - Every page gets a `.md` copy and a "Copy page" button.
   - A docs MCP at `https://cetra.sh/mcp` with `search_docs` and `get_page`.
5. **Cloud routing.** `init` with no URL goes to `app.cetra.sh`, which hands the device code to the cell that holds the user's org. Secrets never pass through the control plane.
6. **Docs and tests.**
   - A "Set up with your coding agent" page.
   - The quick start gets a coding-agent tab.
   - The CLI reference is regenerated.
   - An end-to-end test from `init` to a traced span read back with the CLI's `query`, against a real `cetra serve`.

## Non-goals

- **`cetra init` in the Rust binary**, and an npm package that carries the Rust binary. Setup has one implementation, the Node CLI (D-AO-2), as Laminar's `lmnr-cli` is its only setup path. The binary (server, UI, DataFusion) is far too large to fetch through `npx`.
- **A Python CLI** (`uvx`). Developers who use coding agents have Node; one CLI is enough for the MVP.
- **Editing the app's code.** That is the coding agent's job, guided by the skill and the prompt. `init` changes only `.env`, `.gitignore`, `.cetra/project.json` and the skill directories.
- **Changing the agent's MCP settings.** `init` does not register the trace MCP (`/mcp`) or the docs MCP in the agent's config files: that needs a token in the agent's config, and every agent keeps that config differently. The skill names both MCPs, and the docs page shows how to add them.
- **Tracing the coding agent itself.** That is `cetra agents setup` in the Rust binary (track CA). The two share nothing, and the docs keep them apart (D-AO-1).
- **AI search on the docs site** (D-W-11 stands): the docs MCP returns pages and snippets, with no model behind it.
- **Windows support for the local server.** The CLI itself runs wherever Node runs, but starting a local server follows the binary's release targets (D-N-1: Linux and macOS) or Docker.

## Architecture

```
developer's terminal                 browser                       cell (cetra serve)
─────────────────────               ─────────                     ──────────────────
npx @cetra-ai/cli init ─POST /api/auth/device/code──────────────────────────► device store (in memory)
   │  prints the code and the URL                                    │
   │                          /device?code=… ── session ────────────►│ approve: project,
   │                          picks or creates the project           │ mints the PAT and the
   │                                                                 │ ingest key, holds them
   ├──POST /api/auth/device/token (polls) ──────────────────────────►│ hands them over once
   ├─ writes .env, .cetra/project.json, ~/.config/cetra/credentials.toml
   ├─ installs .claude/skills/cetra and .agents/skills/cetra (shipped in the npm package)
   └─ prints the prompt ──► coding agent ── reads the skill, cetra.sh/llms.txt, cetra.sh/mcp
                                 └─ instruments, runs the app, `npx @cetra-ai/cli query` checks
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
| `@cetra-ai/cli`: `init`, `query`, `login` | `node-cli/` (new pnpm workspace package), `release.yml`, `justfile` | M3 |
| `.md` pages, Copy page, docs MCP | `website/` (plugin, Worker script, build script) | M4 |
| Internal route for the control plane; `moved` in the Node CLI | `crates/server`, `node-cli/` | M5 |
| The relay on `app.cetra.sh` | the private `cetra-control` repository (track Q) | Q's request |

## Contracts

### Device flow (M1)

The routes follow RFC 8628, plus a project step and the cetra secrets. Every route is in `openapi.json`.

| Route | Access | Body → answer |
|---|---|---|
| `POST /api/auth/device/code` | public; 10 a minute per client IP, an IPv6 address counted by its /64 | `{purpose: "login" \| "init", host?: string}` → `200 {device_code, user_code, verification_uri, verification_uri_complete, expires_in: 600, interval: 5}` |
| `POST /api/auth/device/token` | public | `{device_code}` → `200 DeviceGrant`, or a `400` problem whose `code` is `authorization_pending`, `slow_down`, `access_denied` or `expired_token` (RFC 8628 § 3.5 names, in our problem+json shape) |
| `GET /api/auth/device/{user_code}` | session only; with approve and deny, 30 a minute per user, so user codes cannot be enumerated | `{purpose, host?, created_at}` for the approval page; 404 when unknown or expired |
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
  - `login` (the Rust `cetra login` and `cetra-cli login`): a PAT with scopes `read, write`, as `cetra login` gets today.
  - `init`: a PAT with scope `read`, which is what `query` and the trace MCP need. It also mints an ingest-only project key. The user needs the project permission that creating keys needs (`ManageSettings`: project admin, or org admin or owner). Otherwise the approval answers 403, or 404 for a project the user cannot see.

  Names: `CLI on <host>` for `login`, as today (D-L-9), and `CLI init on <host>` for both secrets of `init`. Without a host they are `CLI` and `CLI init`.
- **Hand-over.** The plaintext secrets stay **in memory only**, in the code's entry, until the first successful `token` call takes them, at most one minute after the code's 10-minute expiry. They are never written to disk or logged. The grant answer carries `Cache-Control: no-store`.
- **Approval in progress.** Approving claims the code for at most 60 s while the secrets are minted. If the request dies in between (a closed tab), the code falls back to pending.
  - A restart loses pending codes. The CLI then gets `expired_token` and says to run the command again.
  - The PAT and the key already exist in `meta.db` and appear on the account and project pages, where the user can revoke them.
- **Bounds.**
  - At most 10 000 pending codes per process; one more answers `503`.
  - Expired entries are swept every minute.
  - A denied or expired code never hands anything over.
- **CSRF.** `code` and `token` are exempt from the `Origin` check on `/api/auth/` (D141). They carry no ambient credential, and a CLI sends no `Origin`. Approve and deny are browser calls with a session, so the check applies to them as before.
- **Phishing.** The approval page shows the `host` the CLI reported and the purpose, warns "Only continue if you just ran `cetra-cli init`, `cetra-cli login` or `cetra login` on this computer", and needs an explicit click. The code alone never approves.
- **Approval page** (`/device`, web):
  1. It asks for the code, or reads `?code=`. A user who is not signed in goes through login first and comes back.
  2. For `init`, it lists the orgs and projects the user can create keys in, offers "New project" (name, then the org's project-creation route), and preselects the only project when there is one.
  3. Approve or Deny.
- **Deleted:** the `/cli-auth` page and its tests, the localhost callback server in `login.rs`, the callback's sections in spec 08, the CSP's `form-action http://127.0.0.1:*`, and the reserved org slug `cli-auth` (`device` replaces it). There is no compatibility path (`AGENTS.md`).

### `@cetra-ai/cli` (M3)

**The package.**
- **Name and command.** `@cetra-ai/cli` on npm, next to `@cetra-ai/sdk`. Its bin is `cetra-cli`, so it never clashes with the Rust `cetra` binary on PATH. Users run it through `npx @cetra-ai/cli@latest …`.
- **Where it lives.** `node-cli/`, a new package in the pnpm workspace. It is TypeScript compiled to one ESM bundle and needs Node `>=22.12`, the TypeScript SDK's floor.
- **Release.** It ships from the same release workflow as `@cetra-ai/sdk` (PR #207), at the workspace version, with npm provenance, and `just versions-check` covers it.
- **Dependencies.** Only Node built-ins: `fetch`, `node:util` `parseArgs`, `node:readline`, `node:child_process` and `node:fs`. The one exception is `smol-toml` 1.9.1 (latest on 2026-11-21, BSD-3-Clause, no dependencies), which reads and writes the shared credentials file. It is a devDependency, inlined into the bundle by tsdown, so `npx` installs nothing else. Tests run with vitest, as in the TypeScript SDK.
- **Output.** Human text goes to stderr and `--json` output to stdout, so an agent can run any command.

**Commands:**

```
cetra-cli init  [--url <URL>] [--project <org/project>] [--dir <path>] [--env-file <path>]
                [--agents claude,agents | --no-skill] [--no-browser] [--yes] [--json]
cetra-cli query "<sql>" [--since 15m | --from … --to …] [--limit n] [--json] [--url …] [--project …]
cetra-cli login [--url <URL>] [--no-browser]
```

#### `init`, step by step

1. **Choose the server.** The URL comes from `--url`, then `CETRA_URL`, then the link file (§ Project link), then `https://app.cetra.sh` (the cloud).
   - On a loopback URL with no answer on `/readyz`, init offers to start a local server (§ Local server). With `--yes` it starts one without asking. Without a terminal and without `--yes` it fails with exit code 3.
   - Any other URL with no answer fails with exit code 3.
2. **Run the device flow** with `purpose: init`. It prints the code and the URL and opens the browser, unless `--no-browser` is given or there is no display. Then it polls.
   - The browser opens with `open` on macOS and `xdg-open` on Linux, if present.
   - `--project` preselects that project on the approval page (`&project=org/slug` is added to the URL). The user still approves.
   - On the cloud the first answers come from `app.cetra.sh` (§ Cloud routing).
3. **Save the PAT** in the credentials file the Rust CLI already uses, `<config dir>/cetra/credentials.toml`, mode 0600 (D-AO-17). It is saved under the URL the CLI talks to (the normalized `--url`, or the cell after a `moved`), as the Rust `cetra login` does, and replaces the token saved there. Afterwards the Rust `cetra query` and `cetra traces` work too. `.env`'s `CETRA_BASE_URL` is the grant's `server` (the server's public URL), which is what the SDK should use.
4. **Write the env file** (`--env-file`, default `<dir>/.env`, where `--dir` defaults to the current directory). Init sets two variables, the SDK's own (spec 10):

   ```
   CETRA_API_KEY=sk-cetra-…
   CETRA_BASE_URL=https://eu1.cetra.sh
   ```

   - **Existing lines.** A line that already sets one of the two variables is replaced in place. Other lines, comments and their order are kept. The file is written 0600 through a temporary file and a rename. A new file gets a `# cetra (npx @cetra-ai/cli init)` comment above the two lines.
   - **An existing key.** When the file already has a `CETRA_API_KEY` for the same server, init asks before replacing it. With `--yes` it replaces it. The key the grant brought stays valid on the server either way.
   - **Git.** When `<dir>` is inside a git work tree and the env file is not ignored (`git check-ignore`), init adds its name to `<dir>/.gitignore` and says so. Init runs no other git command.
5. **Write the project link** (§ Project link).
6. **Install the skill** (§ Skill install).
7. **Print the agent prompt** (§ Prompt) and the project's traces URL. With `--json`, stdout carries one JSON object and nothing else:

   ```json
   {"server": "…", "project": "org/slug", "env_file": "/abs/.env",
    "skills": ["/abs/.claude/skills/cetra"], "prompt": "…", "traces_url": "…"}
   ```

   It never carries a secret.

**Exit codes** (every command):

| Code | Meaning |
|---|---|
| 0 | done |
| 1 | other error |
| 2 | usage error |
| 3 | server not reachable, or the local server would not start |
| 4 | sign-in denied, the code expired, or no saved token (`query`) |
| 5 | the env file or the link could not be written |
| 6 | the query failed (`query`: SQL error, timeout, 4xx) |

Skill install failures are warnings, not errors: the setup works without the skill.

**Privacy.** The CLI never prints or logs the key or the token, and `--json` carries neither. Error messages name files by their base name, as the Rust `credentials.rs` does.

#### `query` and `login`

- **`query`.** It reads the server and project from `--url` and `--project`, then `CETRA_URL` and `CETRA_PROJECT`, then the link file. The token comes from `CETRA_TOKEN`, then the credentials file for that server. It calls `POST /api/v1/projects/{project}/sql`, the SQL console endpoint, with the window and limit.
  - The window is `--since` (default `24h`) or `--from`/`--to`. The default limit is 100.
  - It prints a table on a terminal, and `{columns, rows, truncated}` with `--json`.
  - It is a thin client: no export formats, no `--file`, no params. The Rust `cetra query` keeps those.
- **`login`.** The device flow with `purpose: login`, which saves a `read, write` PAT, as the Rust `cetra login` does. It is there for when the 90-day token from `init` has expired.

#### Project link

`init` writes `<dir>/.cetra/project.json`:

```json
{"server": "https://eu1.cetra.sh", "org": "acme", "project": "demo", "project_id": "…"}
```

`server` is the URL the CLI talked to, the same key as in the credentials file.

It holds no secret, so it may be committed. That lets a teammate run `query` with their own login, and later inits keep the same server. `query` looks for the file in the current directory and its parents, up to the git work tree's top level. The Rust CLI does not read it.

### Local server (M3)

When the URL is loopback (the default for a self-hosted trial is `http://localhost:8080`) and nothing answers, init offers to start a server on this machine, in this order:

1. **The `cetra` binary**, if it is on PATH: "No cetra server at http://localhost:8080. Start one with `cetra serve`? [Y/n]".
   - **Data directory.** It is `<data dir>/cetra/local`: `~/.local/share/cetra/local` on Linux, `~/Library/Application Support/cetra/local` on macOS. Init writes `local.toml` next to it (below), so the data dir stays empty until the first start; a first start is one without `meta.db`.
   - **Start.** `cetra serve --config <local.toml>` runs as a detached child (`spawn` with `detached: true`, then `unref`). Its output goes to `server.log` in the data directory, and its PID to `server.pid`.
   - **Last lines.** Init prints how to stop it (`kill <pid>`) and how to start it again.
2. **Docker**, if `docker version` answers: "Start one with Docker? [Y/n]".
   - The command is the quick start's `docker run -d` (`ghcr.io/cetra-ai/cetra:<the CLI's version>`), with the volume `cetra-local`, the container name `cetra-local` and the ports on `127.0.0.1`.
   - **Last lines.** Init prints `docker stop cetra-local` and `docker start cetra-local`.
3. **Neither**: init prints the install.sh and Docker steps from the quick start and exits with code 3.

**First-start values.** In both cases they go in the child's environment, never in a file:
- `CETRA_INIT_ADMIN_EMAIL`, asked for at the prompt; `--yes` uses `admin@cetra.local` (the server needs a dotted domain);
- a generated 24-character `CETRA_INIT_ADMIN_PASSWORD` (letters and digits);
- `CETRA_INIT_ORG=local`;
- `CETRA_INIT_PROJECT`, the base name of `<dir>`, else `default`. The server derives the slugs; the CLI reads them back from the API, never computes them.

No `CETRA_INIT_SECRET_KEY`: the server would give that key `ingest, read, write`, and D-AO-6 wants an ingest-only key. The variables apply on a first start only (spec 12). Init waits up to 30 s for `/readyz`.

**The local config.** `local.toml` sets `data_dir`, `public_url = "http://localhost:<port>"`, and `http_addr`, `otlp_http_addr` and `grpc_addr` on `127.0.0.1` (the server's defaults listen on every interface).

**Sign in without a browser.** Init uses `http://localhost:<port>` as the server for a local start, whatever loopback spelling was typed, because the server checks `Origin` against `public_url`. On a first start init knows the password. With `Origin: <public_url>` on every session call:
1. `POST /api/auth/login` gives the session cookie;
2. `GET /api/auth/me` and `GET /api/v1/orgs/{org}/projects` give the org and project slugs and the project id;
3. `POST /api/v1/me/tokens` creates the `read` PAT, with `expires_at` 90 days ahead, named `CLI init on <host>`;
4. `POST /api/v1/projects/{project}/api-keys` creates the `ingest` secret key with the same name;
5. `POST /api/auth/logout`.

Init prints the admin email and password once, with the UI's URL. When the data was already there, init only starts the server and then runs the device flow as on any server.

**Docker before the first release.** The image `ghcr.io/cetra-ai/cetra` stays private until the first public release, so the Docker path fails to pull until then; init then says so and prints the install steps.

### Skill install (M3)

- **What ships.** The skill ships inside the npm package. The package's build copies `skills/cetra/` into it, and a test checks that the copy equals the source.
- **Where it goes.** It is written to:
  - `<root>/.claude/skills/cetra/`, read by Claude Code;
  - `<root>/.agents/skills/cetra/`, read by Codex, Cursor, Gemini CLI, Copilot and OpenCode (the shared project path the agents settled on; see the competitor notes for sources).

  `<root>` is the git work tree's top level when `<dir>` is inside one, else `<dir>`. `--agents` picks a subset (`claude`, `agents`), and `--no-skill` skips the step.
- **Ownership.** Init writes a `.cetra-skill` file with the CLI's version next to `SKILL.md`. It replaces a directory that has this file. A directory without it is not ours: init leaves it alone and warns.
- **Updates.** `npx @cetra-ai/cli@latest init` updates the skill. The mirror (M2) serves users who install skills another way.

### Prompt (M3)

The prompt is a template in `node-cli/`. Init fills it with the project (`org/slug`), the env file's name and the traces URL. It names the skill and gives these steps:

1. Read the cetra skill. If it is not installed, read `https://cetra.sh/llms.txt`.
2. Find the app's language and LLM libraries, and install the cetra SDK with the matching extras or instrumentors. Call `init` once at startup, reading the key and URL from the environment (`.env`). Never paste the key into code or chat.
3. Run the app, or its tests, once so that it makes at least one LLM call. Flush before exit in short scripts.
4. Check with `npx @cetra-ai/cli query "<sql>" --since 15m --json`. The link file gives the server and project. If it finds no rows, follow the skill's troubleshooting steps.
5. Report what changed and give the traces URL.

The same text is in the docs page, with placeholders, so users of another setup can copy it.

### The skill (M2)

```
skills/cetra/
  SKILL.md                    hand-written
  references/                 generated from the docs; never edited by hand
    python.md  python-tracing.md  typescript.md  typescript-tracing.md  typescript-runtimes.md
    opentelemetry.md  otlp.md  metadata-and-tags.md  privacy-controls.md  sql-schema.md
    <one page per integration: openai.md, anthropic.md, langchain.md, vercel-ai-sdk.md, …>
    troubleshooting.md
references.json               (in skills/, outside the skill) reference name → docs page path
```

**`SKILL.md`** has the Agent Skills frontmatter:
- `name: cetra`;
- `description`, which says when to load it: adding LLM tracing or observability, cetra, OpenTelemetry for LLM apps;
- `allowed-tools`, limited to read-only commands (`npx @cetra-ai/cli query`, and the Rust `cetra query`, `cetra traces` and `cetra projects` for users who have the binary).

Its body is short (about 150 lines) and **routes**:
- how to detect the language and the LLM libraries, and which reference page to read for each;
- the rules: the key comes from the environment, flush in scripts, `capture_content`, never paste the key into code or chat;
- how to check the result with `npx @cetra-ai/cli query`, and the trace MCP as an alternative;
- when to read `https://cetra.sh/llms.txt` or use the docs MCP.

**References are generated** from the docs' MDX by **our converter**, `website/scripts/markdown.mjs`. It wraps `tidymd` 0.2.0 (the converter inside `starlight-page-actions`) with a pre-pass for our components: tab labels kept as bold lines, sample markers, `<Backing>` and `<Image>` dropped. `tidymd` alone loses the tab labels and leaves the markers (`ao-m2-code-facts.md` § 3). M4's `.md` pages use the same converter, so the skill and the pages say the same thing. `website/scripts/skill-references.mjs` reads `skills/references.json` (reference name → docs page), converts each page with absolute `https://cetra.sh/...` links, and writes `skills/cetra/references/`.

**Checks** in `just check-docs`:
- the generated references must equal what is committed (`CETRA_DOCS_WRITE=1` rewrites them, through `just docs-gen`, like the CLI reference);
- every `CETRA_*` name and `cetra` subcommand that `SKILL.md` names must exist (`check-env-vars.mjs`, extended to `skills/`);
- every link in `SKILL.md` must point at an existing docs page.

**Mirror** (`skills-mirror.yml`, on pushes to `main` that touch `skills/`): a `just skills-mirror` recipe copies `skills/cetra/` into a checkout of `cetra-ai/skills` and commits it there with the source commit's SHA, using a deploy key kept in a repository secret. It runs outside `ci-ok`. That repository also holds the plugin manifests (`.claude-plugin/marketplace.json`, `.codex-plugin/`, `.cursor-plugin/`). They are kept in `skills/manifests/` in this repository and copied with the skill. Users then install with `npx skills add cetra-ai/skills`, or from the Claude Code plugin marketplace.

### Docs for agents (M4)

- **`llms.txt`, `llms-full.txt`, `llms-small.txt`**: already served by `starlight-llms-txt` 0.12.0 (D-W-19), the latest version on 2026-11-21. No change.
- **Markdown pages and the "Copy page" button**:
  - After `astro build`, `website/scripts/md-pages.mjs` writes `dist/docs/<page>/index.md` for every page with the M2 converter, so `https://cetra.sh/docs/<page>/index.md` serves the Markdown. A `_headers` rule serves those files as `text/plain`, so a browser shows them. Generated pages (the API reference from `starlight-openapi`) have no MDX source and get no `.md` copy; the title actions are hidden there.
  - The converter learns multi-line `<LinkCard … />` tags, which three pages use.
  - A `PageTitle` override adds two small actions next to the title: "Copy page", which fetches that `.md` and copies it, and "View as Markdown".
  - `llms.txt` stays with `starlight-llms-txt` (D-W-19).
  - `starlight-page-actions` is rejected: its transform cannot be changed, so its `.md` pages would keep our sample markers and lose the tab labels.
- **Docs MCP** at `https://cetra.sh/mcp`:
  - **Where.** A Worker script is added to `cetra-site`. `run_worker_first: ["/mcp"]` sends only that path to the script; everything else stays static assets.
  - **Protocol.** `@modelcontextprotocol/sdk` 1.32.2, streamable HTTP, stateless, JSON responses, no auth. Cloudflare's `agents` 0.34.0 is rejected: it brings Durable Objects for sessions we do not need.
  - **Tools:**

    | Tool | Input | Output |
    |---|---|---|
    | `search_docs` | `query` (1–200 chars), `limit?` (1–20, default 8) | `[{title, url, snippet}]`, best first |
    | `get_page` | `url` (a `https://cetra.sh/docs/…` URL or a `/docs/…` path) | the page's Markdown (its `.md` asset), capped at 100 000 characters with a note when cut |

  - **Search** is MiniSearch 7.2.0 over the `.md` pages: title, headings and body, with prefix and fuzzy matching. A build script writes the index as a static asset, `/mcp-index.json`, which the script loads once per isolate. The snippet is the page's description, stored in the index.
  - **Runtime.** No `nodejs_compat`; `zod` 4.6.7 is an explicit dependency; one server and transport per request (stateless). Wrangler bundles the entry itself.
  - **Logging.** No query or page is logged, in line with D-W-10's "no data leaves the page" spirit. Cloudflare's own request metrics are all we keep.
  - **Tests.** `node --test` covers the tools against a built index (unknown page, cut page, empty query). `just check-docs` builds the index.
- **Docs pages** (M3 and M4):
  - a new "Set up with your coding agent" page under Get started, with the one-liner, what `init` changes, the prompt, the manual path (`npx skills add cetra-ai/skills`, setting the variables yourself), and how to add the trace and docs MCPs to Claude Code, Codex, Cursor and Gemini CLI;
  - the quick start gets a short pointer to that page above its tabs (amends D-W-4's "without its agent-install preamble");
  - the CLI reference is regenerated (`init`, and `login` on the device flow);
  - the CA pages say "to trace the agent itself" and link the new page the other way.

### Cloud routing (M5 and Q's request)

`app.cetra.sh` is the control plane. It knows the user and the org → cell mapping (D170). Cells hold the keys.

1. **The control plane issues the code.** The CLI calls `POST https://app.cetra.sh/api/auth/device/code` and gets codes in the same shape, with `verification_uri` `https://app.cetra.sh/device`.
2. **The user picks the org on the control plane.** The user opens that URL and signs in at the control plane. When they belong to orgs on more than one cell, they pick the org. The control plane then calls the org's cell:

   `POST /api/internal/v1/device/codes`, a control-plane token with scope `device` (D-P-28), body `{device_code_sha256, user_code, purpose, host?, expires_at}`.

   The cell stores the entry as if it had issued it. The control plane then redirects the browser to `<cell>/device?code=…`, where the cell's own page approves it with a cloud session (D183 sign-in).
3. **The CLI moves to the cell.** The CLI keeps polling `app.cetra.sh` with its `device_code`. Once the code is registered on a cell, the control plane answers `400 application/problem+json` with `{type: "about:blank", title: "Bad Request", status: 400, code: "moved", server: "<cell public_url origin>"}`. The CLI then polls the cell with the same `device_code`, and the cell hands the grant over.

The secrets are minted on the cell and leave it only toward the CLI: the control plane never receives a token or a key. The cell side (the internal route and the `moved` handling in the CLI) is M5. The control-plane side is a request to track Q, written to the session notes as `ao-requests-for-q.md` when M5's plan is written. Until Q ships it, `npx @cetra-ai/cli init --url https://eu1.cetra.sh` works directly.

**Decisions settled in M5:**
- **The `moved` answer** is a `400 application/problem+json` with `code: "moved"` and one extra member, `server`: the origin of the cell's public URL. The control plane answers it on `POST /api/auth/device/token` once the code is registered on a cell.
- **The CLI follows `moved` at most once**, to that origin only, over `https`. Loopback `http` is allowed only when the typed server is itself loopback (tests). It sends nothing there but the device code. Afterwards every request that carries the new token (`/api/auth/me`) goes to the new server, never to the old one.
- **Trust boundary.** The control plane is the cells' identity provider (D183). A compromised one can already sign in as anyone on any cell, so it is trusted. "The secrets never pass through it" means they are minted on the cell and sent only to the CLI. The control plane, which knows the device code, could still poll the cell for a grant itself. A PKCE-style poll secret is rejected: it protects nothing against an identity provider.
- **Registration is idempotent.** The same registration twice (same hash, user code, purpose, host and expiry, still pending) answers 204. Any other clash answers 409.

## Decisions

| ID | Topic | Decision |
|---|---|---|
| D-AO-1 | Name | **`npx @cetra-ai/cli init`** sets up an app to send traces. **`cetra agents setup`** (track CA, Rust binary) traces a coding agent itself. They are different tools with different docs pages, and each page links the other in one sentence. Rejected: `setup` as the subcommand, too close to `cetra agents setup`. |
| D-AO-2 | Distribution | **A Node CLI, `@cetra-ai/cli` (bin `cetra-cli`), run with `npx @cetra-ai/cli@latest init`, as Laminar's `npx lmnr-cli setup`** (the user, 2026-11-21, replacing a first choice of install.sh and the binary). Setup lives only there, and the Rust binary gets no `init`, so setup has one implementation; Laminar likewise has only its Node CLI. Its commands are `init`, `query` and `login`. Rejected: `cetra init` in the Rust binary (needs install.sh first and a large download); an npm package carrying the Rust binary (the server and UI are far too large for `npx`); `uvx` (one CLI is enough); the npm name `cetra`, which is taken. |
| D-AO-3 | Sign-in | **A device authorization flow (RFC 8628) on the cell, and the browser picks the project** (the user, 2026-11-21). The approval page mints a 90-day PAT and, for `init`, an ingest-only project key, with the session, so creating keys stays session-only (D-P-70). Rejected: extending the localhost-callback page, which fails when the browser is on another machine (SSH, devcontainers, cloud agents); letting PATs create keys, which loosens D-P-70; Laminar's keys that never expire. |
| D-AO-4 | One sign-in flow | **`cetra login` moves to the device flow; the localhost callback and `/cli-auth` are deleted.** Two browser flows in one CLI would be two paths to maintain and test (`AGENTS.md`). Amends D140 and spec 08 § CLI login. |
| D-AO-5 | Secrets in transit | **The grant's secrets are minted at approval and held in memory only, until the first successful poll, or one minute after the code's 10-minute expiry.** An approval in progress falls back to pending after 60 s if its request dies. A restart loses pending codes, and the user runs the command again. Rejected: storing them in `meta.db`, which would put plaintext secrets on disk for a 10-minute convenience. |
| D-AO-6 | Scopes | **`init` gets a `read` PAT and an `ingest` key; `login` keeps a `read, write` PAT.** The app needs only ingest. The agent's check step needs only read, and a read-only token in a coding agent's reach cannot delete data. |
| D-AO-7 | Env file | **`CETRA_API_KEY` and `CETRA_BASE_URL`, the SDK's own names (spec 10).** The existing lines are replaced in place, the file is written 0600, the file is added to `.gitignore` inside a git work tree, and an existing key is replaced only after asking. No `CETRA_PROJECT`: the SDK does not read it, and the project link gives `query` its project (D-AO-16). |
| D-AO-8 | Skill source and sync | **`skills/cetra/` in this repository**: `SKILL.md` is hand-written, and `references/` is generated from the docs pages that `skills/references.json` lists, by the converter behind the site's `.md` pages. `just check-docs` fails when the references are stale or `SKILL.md` names an unknown variable, command or page (the user chose this repository, mirrored, 2026-11-21). Rejected: a hand-written copy of the docs, which drifts; a skill that only links to the live docs, which fails offline and for self-hosted versions behind the site. |
| D-AO-9 | Skill delivery | **Shipped inside `@cetra-ai/cli` and installed by `init` into `.claude/skills/cetra/` and `.agents/skills/cetra/` at the repository root; mirrored by CI to the public `cetra-ai/skills` repository, with the Claude, Codex and Cursor plugin manifests.** The installed skill matches the CLI version. Rejected: downloading from a moving branch at setup time (Laminar), which needs the network, is not pinned to the version, and installs content from outside the release. A directory without our `.cetra-skill` marker is never overwritten. |
| D-AO-10 | Check step | **`npx @cetra-ai/cli query` over the `traces` view**, with `--json` and a 15-minute window, through the SQL console endpoint. The PAT from `init` is enough, and the project link gives the server and project. The trace MCP is mentioned in the skill as an alternative but not configured (non-goal). |
| D-AO-11 | Local server | **On a loopback URL with nothing listening, `init` offers to start a local server: `cetra serve` detached when the binary is on PATH, else the quick start's `docker run` when Docker answers, else the install steps.** It uses generated `CETRA_INIT_*` values (the user, 2026-11-21). On a first start it signs in with the generated password, without a browser, and prints the password once. Rejected: printing the start command and exiting, which is one more step before the first trace. |
| D-AO-12 | Markdown pages | **Our own converter, `website/scripts/markdown.mjs` (`tidymd` 0.2.0 plus a pre-pass for our components), writes the skill references (M2) and a `.md` copy of every page after the build (M4); a `PageTitle` override adds "Copy page" and "View as Markdown".** One converter keeps the skill and the pages identical. `llms.txt` stays with `starlight-llms-txt` (D-W-19). Rejected: `starlight-page-actions` 0.7.1, whose fixed transform loses tab labels and keeps our sample markers; `starlight-dot-md` 0.2.1, which has no button and the same conversion problem. |
| D-AO-13 | Docs MCP | **In this track** (the user, 2026-11-21). The tools are `search_docs` and `get_page` on the `cetra-site` Worker at `/mcp`: stateless, no auth, MiniSearch over the `.md` pages, nothing logged. Amends D-W-3: the Worker gains a script for that one path. D-W-11 stands: search without a model. |
| D-AO-14 | Cloud routing | **`app.cetra.sh` issues the code and registers it on the org's cell through a control-plane call. The CLI is sent on to the cell with `moved`, and the secrets never pass through the control plane** (the user chose routing through `app.cetra.sh`, 2026-11-21). The cell side is in this track; the control-plane side is a request to track Q. The `moved` answer is a 400 problem with `code: "moved"` and `server`; the CLI follows it once, to an https origin. The control plane, as the cells' identity provider, is trusted: the secrets are not sent through it, but it knows the device code. Rejected: asking the region in the CLI, which hard-codes cell URLs in the binary. |
| D-AO-15 | Agent-friendly CLI | **Human text on stderr; `--json` (one object on stdout, no secrets), `--yes`, `--no-browser`, and fixed exit codes 3–6** for an agent that runs the CLI itself. |
| D-AO-16 | Project link | **`init` writes `.cetra/project.json` (server, org, project, project id; no secret)**, which `query` finds in the directory or its parents, as Laminar's `.lmnr/project.json` (the user, 2026-11-21). The agent's check step then needs no `--url` or `--project`. It can be committed, so teammates query with their own login. The Rust CLI does not read it. |
| D-AO-17 | Credentials | **The Node CLI reads and writes the Rust CLI's `credentials.toml` (same path, format and mode)**, through `smol-toml`. A login with either CLI serves both. Rejected: a second credentials file, which would leave the user signed in with one CLI and not the other. |

## Milestones

M1 and M2 do not depend on each other. M4 depends only on M2's script (it can run after or alongside M2's last phase). M3 needs M1, M2 and the npm publishing of PR #207. M5 needs M1 and M3.

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
- `skills/cetra/SKILL.md`, `skills/references.json`, and the generated `references/`.
- `skill-references.mjs` with its write and check modes; the checks in `just check-docs` and the rewrite in `just docs-gen`.
- The plugin manifests in `skills/manifests/`, `just skills-mirror`, and `skills-mirror.yml`.

**Depends on:** nothing new. M2 adds the converter (`markdown.mjs`, `tidymd`).

**Leaves out:**
- creating the `cetra-ai/skills` repository and its deploy key: the user does that (§ Open questions);
- installing the skill from the CLI (M3).

### M3 — `@cetra-ai/cli`

**Delivers:**
- The `node-cli/` package with `init`, `query` and `login`, every step and flag above:
  - the device flow;
  - the shared credentials file;
  - the env file and `.gitignore`;
  - the project link;
  - the skill, shipped and installed;
  - the prompt, `--json` and the exit codes;
  - the local server through the binary or Docker.
- Its publishing in `release.yml` next to `@cetra-ai/sdk`, and `just check-cli-js` in `just check`.
- `AGENTS.md`'s layout and commands updated.
- The "Set up with your coding agent" docs page, the quick-start tab, and a CLI page under Reference.
- `CHANGELOG.md`.
- Unit tests against a mock server (`node --test`).
- An end-to-end test in `just e2e`: `init --url` against a real `cetra serve` started with `CETRA_INIT_*`, the device approval through the API with a session, then a span sent with the written key, then `query` reading it back through the link file.

**Depends on:** M1 and M2, and the npm publishing of PR #207.

**Leaves out:** the cloud's `moved` answer (M5).

### M4 — Docs for agents

**Delivers:**
- `md-pages.mjs` writing the `.md` pages, and the `PageTitle` override with "Copy page" and "View as Markdown".
- The docs MCP: the Worker script, the index build and its tests.
- The `wrangler.jsonc` change, and D-W-3 amended in spec and decisions.
- A docs page section on the docs MCP.

**Depends on:** M2's converter.

**Leaves out:** AI answers (D-W-11).

### M5 — Cloud routing, cell side

**Delivers:**
- `POST /api/internal/v1/device/codes` with the scope `device` on control-plane tokens.
- The Node CLI following `moved` once, to the cell named in the answer, only over `https` (or loopback in tests).
- Tests with a fake control plane.
- `ao-requests-for-q.md`, Q's side as a request.

**Depends on:** M1 and M3, and P's control-plane tokens (done).

**Leaves out:** the control-plane routes and pages, which belong to track Q in `cetra-control`.

## Open questions

1. **The `cetra-ai/skills` repository and its deploy key.** The user creates the public repository with an initial commit, and adds the deploy key secret (`SKILLS_MIRROR_KEY`) **after M3 and M4 are merged**: the skill names the Node CLI and the docs MCP, which must exist before the skill is published. Until then the workflow does nothing.
2. **The cloud default before Q ships the relay.** Until then, `init` with no URL would reach `app.cetra.sh` without the device routes. Proposed: the CLI reads a 404 there as "use `--url <your cell>`" and says so. Is that acceptable for launch, or must M5 and Q's side land before launch?
3. **The local admin email.** Init asks for it, and `--yes` uses `admin@cetra.local`. Should it default to `git config user.email` instead? Proposed: no, the CLI reads no personal data it does not need.
4. **Plugin marketplaces.** Should the mirror also be submitted to the Claude Code and Cursor plugin directories at launch? That is a one-off manual step for the user, not code.
