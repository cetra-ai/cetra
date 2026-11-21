Status: plan, written 2026-11-21. Not implemented.

# AO · M3 — `@cetra-ai/cli`: `init`, `query`, `login`

This milestone adds the Node CLI that sets an app up with one command, `npx @cetra-ai/cli@latest init`:
- it signs in with the device flow (M1), or with a generated password on a fresh local server;
- it writes `.env`, `.cetra/project.json` and the shared credentials file;
- it installs the skill (M2) and prints the agent prompt.

`query` is the agent's check step, and `login` renews the token. The package is published with `@cetra-ai/sdk` from the release workflow. Docs, an end-to-end test against a real `cetra serve`, and a `check-skill` rule for `npx @cetra-ai/cli` commands come with it.

The design is `docs/plans/ao-agent-setup/00-design.md` § `@cetra-ai/cli` (M3), § Local server, § Skill install, § Prompt, § M3. D-AO-1, D-AO-2, D-AO-6, D-AO-7, D-AO-9 to D-AO-11 and D-AO-15 to D-AO-17 apply. **Haiku implements it one phase at a time.** If a step is unclear or contradicts the code, **stop and report**; do not improvise.

**Check rule** (user, 2026-11-20): no full checks per phase. Each phase may run only:
- its own vitest files: `pnpm --dir node-cli exec vitest run <files>`;
- Biome from inside the package: `cd node-cli && ../node_modules/.bin/biome check --write src/`;
- `pnpm install` when it changes dependencies.

The coordinating session runs `just check` once after C7.

**Node on PATH.** Every command that runs `node` or `pnpm` starts with `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`.

## Prerequisites

- M1 (the device routes) and M2 (`skills/cetra/`) are on this branch.
- The branch is rebased on `origin/main`, which has PR #207: the npm publishing of `@cetra-ai/sdk`, D-N-37.

## Read first

- `AGENTS.md`, and `website/STYLE.md` for the docs phase.
- The session note `ao-m3-code-facts.md` (in `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/`). It has the exact files, lines and behaviours this plan refers to:
  - § 0: traps;
  - § 1: the model package `sdks/typescript`;
  - § 2: publishing;
  - § 4: the Rust CLI behaviours to mirror;
  - § 5: the server API;
  - § 7: the e2e harness;
  - § 8–9: website and skills.
- `sdks/typescript/` (`package.json`, `tsconfig*.json`, `tsdown.config.ts`): the package layout to copy.
- `crates/cli/src/{credentials,login,client,query,output}.rs`: the behaviours this CLI mirrors, message for message.
- `crates/server/src/api/device.rs`, `crates/server/src/routes/device.rs`: the device wire format.

## What it is, and what it is not

**When M3 is done:**
- `node-cli/` is a pnpm workspace package `@cetra-ai/cli`, bin `cetra-cli`, built by tsdown to `dist/cli.js` with the skill copied into `dist/skills/cetra/`.
- `cetra-cli init`, `cetra-cli query` and `cetra-cli login` behave as design § `@cetra-ai/cli` says, with `--json`, the exit codes and the local server.
- `just check-cli-js` runs in `just check` and in CI (`cli-js` and `cli-js-min-node` jobs in `ci-ok`).
- `release.yml` builds, packs and publishes `@cetra-ai/cli` with the SDK. `release-prep` and `versions-check` cover its version.
- `just e2e` runs `init` against a real `cetra serve`: it approves the code, sends a span with the written key, and reads it back with `query`.
- Docs:
  - a "Set up with your coding agent" page;
  - a `cetra-cli` reference page;
  - a quick-start tab;
  - `check-skill` checks `npx @cetra-ai/cli <command>`;
  - `CHANGELOG.md`, `AGENTS.md` and the roadmap row.

**Not in M3:**
- the cloud `moved` answer (M5);
- `.md` pages and the docs MCP (M4);
- the first manual npm publish and the trusted-publisher setting of the new package, which are the user's steps (C7 documents them).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-AO-2 | A Node CLI `@cetra-ai/cli`, bin `cetra-cli`, with `init`, `query`, `login`; no Rust `init` |
| D-AO-6 | `init` gets a `read` PAT and an `ingest` key, on the device path and on the local password path |
| D-AO-7 | `.env` gets `CETRA_API_KEY` and `CETRA_BASE_URL`, replaced in place, 0600, ignored by git |
| D-AO-9 | The skill ships in the package; installed in `.claude/skills/cetra/` and `.agents/skills/cetra/` with a `.cetra-skill` marker |
| D-AO-10 | The check step is `query` over the `traces` view |
| D-AO-11 | Local server: `cetra serve` if on PATH, else Docker, else install steps |
| D-AO-15 | Human text on stderr, `--json` on stdout without secrets, exit codes 0–6 |
| D-AO-16 | `.cetra/project.json` links the directory to the project |
| D-AO-17 | The Rust credentials file is shared |
| D-N-37 | The SDKs share the server's version and tag; npm `latest`/`next`; trusted publishing |

Settled from the code, and recorded in the design on 2026-11-21:
- **Credentials and link key.** Both use the URL the CLI talks to, normalized as the Rust CLI does. `.env`'s `CETRA_BASE_URL` is the grant's `server`.
- **No `CETRA_INIT_SECRET_KEY`.** The local path creates the ingest-only key through the session.
- **`smol-toml` is bundled**: it is a devDependency, inlined by tsdown.
- **`--json` of `query`** prints `{columns, rows, truncated}`. This differs from the Rust CLI, whose JSON is the rows only; the difference is deliberate.

## Versions

| Package | Version | Where | Why |
|---|---|---|---|
| `smol-toml` | `1.9.1` (2026-10-09, BSD-3-Clause, no deps) | `node-cli` devDependencies, exact | the credentials file |
| `typescript`, `vitest`, `tsdown`, `@types/node`, `publint` | the exact versions `sdks/typescript/package.json` pins | `node-cli` devDependencies | the same toolchain as the SDK |

Re-check `smol-toml` with `npm view smol-toml version time --json` and report the version used.

## Phases

```
C1 (skeleton) ─► C2 (http, credentials, link, env file) ─► C3 (device flow, login) ─► C4 (query) ─► C5 (init) ─► C6 (e2e, docs, checks) ─► C7 (CI, release)
```

The phases run in order: each uses the modules of the one before. Files per phase:
- C1: `node-cli/{package.json, tsconfig.json, tsconfig.test.json, tsdown.config.ts, README.md, LICENSE, .gitignore, src/cli.ts, src/version.ts, src/errors.ts, src/cli.test.ts}`, `pnpm-workspace.yaml`, `pnpm-lock.yaml`, `biome.json`, `justfile`.
- C2: `node-cli/src/{http.ts, credentials.ts, link.ts, envfile.ts}` and their tests.
- C3: `node-cli/src/{device.ts, browser.ts, host.ts, login.ts}` and their tests, `node-cli/src/test-server.ts` (test helper), `src/cli.ts`.
- C4: `node-cli/src/{query.ts, table.ts, since.ts}` and their tests, `src/cli.ts`.
- C5: `node-cli/src/{init.ts, local.ts, skill.ts, prompt.ts, prompt-io.ts}` and their tests, `src/cli.ts`.
- C6: `node-cli/src/e2e.test.ts`, `tools/e2e/run.sh`, `website/` pages, `website/astro.config.mjs`, `website/scripts/{check-skill,check-env-vars}.mjs` and tests, `CHANGELOG.md`, `AGENTS.md`, `docs/roadmap.md`, `docs/spec/09-api.md`.
- C7: `.github/workflows/{ci.yml, release.yml}`, `tools/packaging/{release-prep.sh, versions-check.sh}`, `docs/plans/n-packaging/README.md`, `docs/mvp-decisions.md`, `justfile`.

---

## C1 — The package skeleton

**Goal:** an empty-but-working `@cetra-ai/cli` package: it builds, prints `--version` and `--help`, and runs its tests in the workspace's checks.

**Edit:**
- `node-cli/package.json`, `node-cli/tsconfig.json`, `node-cli/tsconfig.test.json`, `node-cli/tsdown.config.ts`, `node-cli/README.md`, `node-cli/LICENSE`, `node-cli/.gitignore` (all new)
- `node-cli/src/cli.ts`, `node-cli/src/version.ts`, `node-cli/src/errors.ts`, `node-cli/src/cli.test.ts` (new)
- `pnpm-workspace.yaml`
- `pnpm-lock.yaml`
- `biome.json`
- `justfile`

**Steps.**
1. **`node-cli/package.json`.** Copy the shape of `sdks/typescript/package.json` as it is on this branch, after #207: `homepage`, `bugs`, `repository` with `directory`, `publishConfig`, `license`, `engines`, `type`. Then:
   - `name` `@cetra-ai/cli`; `version`: the same value as `sdks/typescript/package.json`; `description` "Set up cetra tracing with one command and one prompt to your coding agent.";
   - `homepage` `https://cetra.sh/docs/get-started/coding-agent/`; `repository.directory` `node-cli`;
   - `bin`: `{ "cetra-cli": "./dist/cli.js" }`; `files`: `["dist"]`; no `exports`, no `main`, no `sideEffects`;
   - `scripts`:
     - `typecheck`: `tsc --noEmit && tsc --noEmit -p tsconfig.test.json`;
     - `build`: `tsdown`;
     - `test`: `vitest run`;
     - `check:package`: `publint`;
     - `check`: `biome check . && pnpm typecheck && pnpm build && pnpm test && pnpm check:package`. The build runs before the tests because a test reads `dist/skills`.
   - `devDependencies`: `@types/node`, `publint`, `tsdown`, `typescript` and `vitest` at the SDK's exact versions, plus `smol-toml` `1.9.1`. No `dependencies`.
2. **`tsconfig.json`.** The SDK's, with `"types": ["node"]`, include `src/**/*.ts`, exclude `src/**/*.test.ts` and `src/test-server.ts`. **`tsconfig.test.json`**: the SDK's, including the test files, `src/test-server.ts` and `tsdown.config.ts`.
3. **`tsdown.config.ts`:**

   ```ts
   import { defineConfig } from "tsdown";

   export default defineConfig({
     entry: ["src/cli.ts"],
     format: "esm",
     platform: "node",
     target: "node22",
     dts: false,
     clean: true,
     copy: [{ from: "../skills/cetra", to: "dist/skills", flatten: false }],
   });
   ```

   After the first build, check that `dist/skills/cetra/SKILL.md` exists, with `dist/skills/cetra/references/`. If the copy lands elsewhere (`dist/skills/SKILL.md`, …), adjust `to` so that the tree is `dist/skills/cetra/…`, and report the value used. `dist/cli.js` must start with `#!/usr/bin/env node`, which tsdown keeps from `src/cli.ts`.
4. **`.gitignore`** has one line, `.vitest/`, as the SDK's. Do **not** ignore `dist/` there: the root `.gitignore` already does.
5. **`LICENSE`**: a copy of `sdks/typescript/LICENSE`.
   **`README.md`**, about 25 lines in STYLE.md's voice:
   - what the package does;
   - `npx @cetra-ai/cli@latest init`;
   - the three commands in one line each;
   - a link to `https://cetra.sh/docs/get-started/coding-agent/`.
6. **`src/version.ts`**: exactly `export const VERSION = "<the version>";`, with the same value as `package.json`.
7. **`src/errors.ts`:**

   ```ts
   /** Exit codes (design § `@cetra-ai/cli`, D-AO-15). */
   export const Exit = {
     Ok: 0,
     Error: 1,
     Usage: 2,
     Unreachable: 3,
     SignIn: 4,
     Write: 5,
     Query: 6,
   } as const;
   export type ExitCode = (typeof Exit)[keyof typeof Exit];

   /** An error that ends the command with a message on stderr and an exit code. */
   export class CliError extends Error {
     constructor(
       message: string,
       readonly code: ExitCode = Exit.Error,
     ) {
       super(message);
       this.name = "CliError";
     }
   }
   ```

8. **`src/cli.ts`:**
   - starts with `#!/usr/bin/env node`;
   - `export async function main(argv: string[], io: Io): Promise<ExitCode>`, where `Io = { stdout: (s: string) => void; stderr: (s: string) => void; env: NodeJS.ProcessEnv; cwd: string }`;
   - it reads the first positional argument as the command (`init`, `query`, `login`);
   - `--version` prints `VERSION` on stdout and returns 0;
   - `--help`, `-h` or no argument prints the usage text below on stderr and returns 0 (no argument returns 2);
   - an unknown command prints `cetra-cli: unknown command <name>` plus the usage and returns 2;
   - in C1 the three commands print `cetra-cli: <name> is not implemented yet` and return 1. C3–C5 replace each line; C1 adds no other placeholder;
   - a `CliError` thrown by a command prints `cetra-cli: <message>` on stderr and returns its code. Any other error prints `cetra-cli: <message>` and returns 1. No stack trace is printed;
   - the last lines run `main(process.argv.slice(2), realIo()).then((code) => { process.exitCode = code; })` only when the module is the entry point: `import.meta.url === pathToFileURL(process.argv[1] ?? "").href`, which also matches through the npm bin symlink once `realpathSync` is applied to `argv[1]`. `main` stays importable by the tests.

   Usage text:

   ```
   Usage: cetra-cli <command> [options]

   Commands:
     init     Sign in, pick a project, write .env and install the cetra skill
     query    Run a SQL query on the linked project
     login    Sign in again and save a new token

   Run `cetra-cli <command> --help` for a command's options.
   ```

9. **`pnpm-workspace.yaml`**: add `- node-cli` after `- sdks/typescript`. Run `pnpm install` at the root to add the importer to `pnpm-lock.yaml`.
10. **`biome.json`**: add `"node-cli/**"` to `files.includes`, after `"sdks/typescript/**"`.
11. **`justfile`**: add after `check-ts`:

    ```
    # The cetra-cli package: lint, types, build, tests, package lint (D-AO-2)
    check-cli-js: js-install
        pnpm --dir node-cli run check
    ```

    Add `check-cli-js` to `check:`, after `check-ts`.

**Tests** (`src/cli.test.ts`, vitest; a fake `Io` that records lines):
- `--version` prints the version and returns 0;
- no argument prints the usage and returns 2;
- `--help` prints the usage and returns 0;
- `frob` prints `cetra-cli: unknown command frob` and returns 2;
- a command that throws `new CliError("x", Exit.Write)` prints `cetra-cli: x` and returns 5. Test this through an exported `run(command, …)` helper if `main` cannot reach it; name the helper in the report.
- `VERSION` equals `package.json`'s `version`.

**Done when:**
- `pnpm --dir node-cli run build` succeeds and `dist/skills/cetra/SKILL.md` exists;
- `pnpm --dir node-cli exec vitest run src/cli.test.ts` passes;
- `node node-cli/dist/cli.js --version` prints the version;
- Biome, run from `node-cli/`, leaves no error.

**Commit:** `Add the cetra-cli package skeleton`

---

## C2 — HTTP, credentials, project link, env file

**Goal:** the four shared modules every command uses. Each one mirrors the Rust behaviour named in code facts § 4.

**Edit:** `node-cli/src/http.ts`, `credentials.ts`, `link.ts`, `envfile.ts`, and `http.test.ts`, `credentials.test.ts`, `link.test.ts`, `envfile.test.ts` (all new).

**Steps.**
1. **`http.ts`:**

   ```ts
   export interface Problem { title?: string; status?: number; code?: string; detail?: string;
     errors?: { field: string; message: string; position?: { line: number; column: number } }[] }

   /** One JSON request. Never sends `Origin` unless `origin` is given; never logs. */
   export async function requestJson(
     method: "GET" | "POST" | "DELETE",
     url: string,
     options: { body?: unknown; token?: string; cookie?: string; origin?: string; timeoutMs?: number },
   ): Promise<{ status: number; headers: Headers; body: unknown; text: string }>;

   /** The Rust CLI's problem rendering: `"{title} ({code})"`, `": {detail}"`, one `"\n  {field}: {message}"`
    *  per error (with ` (line L, column C)`), control characters replaced by spaces;
    *  401 → "the server rejected the token; run `cetra-cli login --url <server>`";
    *  otherwise "the server answered <status>". */
   export function problemMessage(status: number, body: unknown, server: string): string;
   ```

   - **Request.**
     - `requestJson` uses `fetch` with `AbortSignal.timeout(timeoutMs ?? 30_000)` and `redirect: "manual"`.
     - It sends `Accept: application/json`, `User-Agent: cetra-cli/<VERSION>`, and `Content-Type: application/json` when there is a body.
     - It adds `Authorization: Bearer <token>`, `Cookie` and `Origin` only when given.
     - It reads at most 64 KiB of an error body; for a 2xx it reads the whole body.
     - `body` is the parsed JSON, or `undefined` when the text is not JSON.
   - **Errors.**
     - A network error or timeout throws `CliError("could not reach <server origin>", Exit.Unreachable)`.
     - The URL in the message is the origin only, never a path or query.
2. **`credentials.ts`** (code facts § 4 "credentials.rs"):

   ```ts
   export function credentialsPath(env: NodeJS.ProcessEnv, platform = process.platform, home = os.homedir()): string;
   export function normalizeServerUrl(raw: string): string;      // trim, drop trailing "/"
   export function serverUrl(raw: string): string;                // checks, then normalizes; throws CliError(…, Exit.Usage)
   export async function loadCredentials(path: string): Promise<Map<string, string>>; // url → token
   export async function saveToken(path: string, server: string, token: string): Promise<void>;
   export function chosenServer(flag: string | undefined, saved: Map<string, string>): string;
   ```

   - **Path.** On Linux: `$XDG_CONFIG_HOME` when it is absolute, else `~/.config`. On macOS: `~/Library/Application Support`. On Windows: `%APPDATA%`. Then `cetra/credentials.toml`.
   - **`serverUrl`.** It refuses a control character, a non-`http(s)` URL, or one that does not parse, with "--url must start with http:// or https://". It refuses a URL with a user or password with "the server URL must not contain credentials". The messages never include the URL.
   - **Load.** A missing file is empty. A file that does not parse as TOML, or whose `servers` is not a table of `{token: string}`, throws `CliError("credentials file is invalid")`.
   - **Save.**
     1. Load (keeping other servers), set the entry, and stringify with `smol-toml` with the servers sorted by URL.
     2. Create the directory and set it to 0700.
     3. Write `credentials.toml.tmp` with mode 0600, `fsync`, and rename it over the file. On failure remove the tmp file and throw `CliError("could not write credentials.toml", Exit.Write)`.

     The format must equal the Rust one: `[servers."<url>"]` then `token = "<token>"`.
   - **`chosenServer`** gives the flag, else the only saved server. With none it throws "no saved server; run `cetra-cli login --url <URL>`"; with several, "more than one saved server; pass --url".
3. **`link.ts`:**

   ```ts
   export interface ProjectLink { server: string; org: string; project: string; project_id: string }
   export const LINK_FILE = ".cetra/project.json";
   export async function writeLink(dir: string, link: ProjectLink): Promise<string>;   // returns the absolute path
   /** Looks in `cwd`, then its parents, up to the git work tree's top (or the filesystem root). */
   export async function findLink(cwd: string): Promise<ProjectLink | undefined>;
   ```

   - The file is pretty JSON with two spaces and a trailing newline, mode 0644. It is created with its directory.
   - A file that is not valid JSON, or that lacks one of the four string fields, throws `CliError(".cetra/project.json is invalid", Exit.Error)`.
   - **The git top.** Walking up, stop after the first directory that contains a `.git` entry, file or directory.
4. **`envfile.ts`:**

   ```ts
   /** Sets the variables in the env file's text; other lines, comments and order are kept. */
   export function setVariables(text: string | undefined, values: Record<string, string>): string;
   export function readVariable(text: string, name: string): string | undefined;
   export async function writeEnvFile(path: string, text: string): Promise<void>;  // 0600, tmp + rename
   /** Adds the env file's name to <dir>/.gitignore when <dir> is in a git work tree and git does not ignore it.
    *  Returns true when it added the line. Runs `git check-ignore -q <name>` in <dir>; exit 1 means "not ignored". */
   export async function ignoreInGit(dir: string, fileName: string): Promise<boolean>;
   ```

   - **`setVariables`.**
     - A line matching `^\s*(export\s+)?NAME\s*=` is replaced with `NAME=value`, keeping its `export ` prefix.
     - Missing variables are appended in the order given.
     - When the text was `undefined`, the result starts with `# cetra (npx @cetra-ai/cli init)`.
     - The result ends with one newline.
     - Values are written bare. Both values match `^[A-Za-z0-9:/._-]+$`; if one does not, throw `CliError("invalid value for <NAME>")`.
   - **`readVariable`** reads the last matching line's value, without surrounding quotes.
   - **`ignoreInGit`.**
     - If `git` is missing or `<dir>` is not in a work tree (`git rev-parse --is-inside-work-tree` fails), return false.
     - Otherwise, when `git check-ignore -q` exits 1, append `\n<name>\n` to `<dir>/.gitignore` (create the file if needed, without doubling blank lines) and return true.
     - Spawn `git` with `execFile`: no shell, arguments as an array.

**Tests:**
- **`http.test.ts`**, against a local `node:http` server started in the test:
  - the headers sent, with no `Origin` unless asked;
  - JSON parsing;
  - a 64 KiB cap on error bodies;
  - an unreachable port throws `Exit.Unreachable` with the origin only;
  - `problemMessage` for a problem with errors and positions, for a 401, and for plain text.
- **`credentials.test.ts`:**
  - the path per platform, with `XDG_CONFIG_HOME` relative or absolute;
  - `serverUrl` refusals, and that the messages hold no URL;
  - a round trip;
  - the file text is exactly `[servers."http://a:1"]\ntoken = "pat-cetra-…"\n` for one server;
  - two servers are sorted;
  - modes 0600 and 0700, on POSIX only (`it.skipIf(process.platform === "win32")`);
  - an invalid file;
  - `chosenServer` in its three cases.
- **`link.test.ts`:** write then find from a subdirectory; stop at a `.git` boundary; an invalid file.
- **`envfile.test.ts`:**
  - replace in place, keep comments and order, keep `export`;
  - append missing variables;
  - the new-file header;
  - an invalid value throws;
  - `readVariable` with quotes;
  - `ignoreInGit` in a temp git repo (run `git init`): a first call adds the line and returns true, a second returns false; outside a repo it returns false.

**Done when:**
- `pnpm --dir node-cli exec vitest run src/http.test.ts src/credentials.test.ts src/link.test.ts src/envfile.test.ts` passes;
- Biome is clean.

**Commit:** `Add the cetra-cli HTTP, credentials, link and env file modules`

---

## C3 — Device flow and `login`

**Goal:** the device flow as the Rust `login.rs` does it (code facts § 4), shared by `login` and `init`, and the `login` command.

**Edit:** `node-cli/src/device.ts`, `browser.ts`, `host.ts`, `login.ts`, `test-server.ts` (new); `device.test.ts`, `login.test.ts` (new); `src/cli.ts`.

**Steps.**
1. **`host.ts`:** `export function localHost(): string | undefined`. It takes `os.hostname()`, keeps only `[A-Za-z0-9.-]`, truncates to 64 characters, and returns `undefined` when empty.
2. **`browser.ts`:** `export function openBrowser(url: string): void`.
   - The command is `open <url>` on macOS, `xdg-open <url>` on Linux, and `rundll32 url.dll,FileProtocolHandler <url>` on Windows.
   - It uses `spawn` with `detached: true`, `stdio: "ignore"`, then `unref()`. No shell.
   - Errors, including a missing program, are ignored.
3. **`device.ts`:**

   ```ts
   export type Purpose = "login" | "init";
   export interface Grant { server: string; token: string;
     project?: { id: string; org_slug: string; slug: string; name: string }; ingest_key?: string }
   export const MIN_INTERVAL_MS = 1000;
   export const SLOW_DOWN_STEP_MS = 5000;
   export const MAX_EXPIRES_IN_S = 900;
   export const DEADLINE_SLACK_MS = 5000;

   export type Step = "wait" | "slow_down" | "denied" | "expired" | { failed: number };
   export function step(status: number, body: unknown): Step;
   export function isUserCode(text: string): boolean;     // exactly /^[A-Z]{4}-[A-Z]{4}$/
   export function isPersonalToken(text: string): boolean; // "pat-cetra-" + 32 ASCII letters or digits
   export function isSecretKey(text: string): boolean;     // "sk-cetra-" + 32 ASCII letters or digits

   /** Requests a code, prints the URL and code on stderr, opens the browser, polls until the grant. */
   export async function deviceSignIn(options: {
     server: string; purpose: Purpose; noBrowser: boolean; project?: string;
     stderr: (line: string) => void; sleep?: (ms: number) => Promise<void>; now?: () => number;
   }): Promise<Grant>;
   ```

   **`deviceSignIn`**, in this order. The messages are the Rust ones (code facts § 4), with `cetra-cli` instead of `cetra` where a command is named.
   1. **Request the code.** `POST {server}/api/auth/device/code` with `{purpose, host}`; `host` is left out when `localHost()` is `undefined`. No `Origin`, no token. A non-200 answer throws `CliError("the server refused a sign-in code (HTTP <n>)", Exit.SignIn)`.
   2. **Validate before printing.**
      - `verification_uri_complete` must parse with `new URL` and have the protocol `http:` or `https:`; otherwise throw "the server sent an invalid sign-in URL" (`Exit.SignIn`).
      - `user_code` must pass `isUserCode`; otherwise throw "the server sent an invalid sign-in code".
      - When `project` is given, append `&project=<encodeURIComponent(project)>` to the URL with `url.searchParams.set("project", project)`.
   3. **Print** to stderr exactly:
      - `Open <url.href> in your browser and approve the code.`;
      - `Code: XXXX-XXXX`.

      Unless `noBrowser`, call `openBrowser(url.href)`.
   4. **Poll.**
      - `interval = max(interval * 1000, MIN_INTERVAL_MS)`, and the deadline is `now + min(expires_in, MAX_EXPIRES_IN_S) * 1000 + DEADLINE_SLACK_MS`.
      - Loop: `await sleep(interval)`. Past the deadline, throw "the sign-in code expired; run the command again" (`Exit.SignIn`). Otherwise `POST {server}/api/auth/device/token` with `{device_code}`.
      - A network error prints `warning: cannot reach the server; still waiting` once, then continues.
      - 200 → the grant. Otherwise `step(status, body)`:
        - 429, 502, 503, 504 → `wait`;
        - 400 with `code` `authorization_pending` → `wait`; `slow_down` → `interval += SLOW_DOWN_STEP_MS`; `access_denied` → throw "sign-in denied in the browser"; `expired_token` → throw the expired message;
        - any other 400, or any other status → throw "sign-in failed (HTTP <n>)".

        All of these throw with `Exit.SignIn`.
   5. **Check the grant.** `isPersonalToken(grant.token)` must hold. For `init`, `grant.project` must be present and `isSecretKey(grant.ingest_key)` must hold. Otherwise throw "the server returned an invalid grant".
   6. Never print the token, the key or the device code.
4. **`test-server.ts`** (test helper, excluded from the build): `startCanned(answers: { status: number; body: unknown; headers?: Record<string,string> }[]): Promise<{ url: string; requests: { method: string; path: string; headers: Record<string,string>; body: unknown }[]; close(): Promise<void> }>`. It is a `node:http` server that answers in order and records each request; past the list it answers 500.
5. **`login.ts`:** `export async function login(args: string[], io: Io): Promise<ExitCode>`.
   - **Options** (`parseArgs`, `strict: true`): `--url <URL>` (else `CETRA_URL`, else `chosenServer` of the saved file), `--no-browser`, `--help`.
   - **Steps:**
     1. `server = serverUrl(…)`, then `deviceSignIn({purpose: "login"})`.
     2. Check the token with `GET {server}/api/auth/me` and the bearer token; a non-2xx answer throws "the server rejected the token" (`Exit.SignIn`).
     3. `saveToken(credentialsPath, server, grant.token)`, then print `Signed in to <server>. The token expires in 90 days.` on stderr.
   - With `--json`, print `{"server": "<server>"}` on stdout.
   - Wire it in `cli.ts`.

**Tests:**
- **`device.test.ts`:**
  - `step` for every case in step 3.4;
  - `isUserCode`, `isPersonalToken`, `isSecretKey` on good and bad input;
  - **canned flows**, with a `sleep` that resolves at once and a fake `now`:
    - pending then grant: the stderr lines, the requests (no `Origin`, no `Authorization`), the grant;
    - `slow_down` grows the interval, which shows in the `sleep` arguments;
    - `access_denied`, `expired_token`, a 500 and the deadline throw their messages;
    - an invalid URL (`javascript:alert(1)`) throws before anything is printed;
    - a bad user code throws;
    - an init grant without a key throws;
    - `project` is added to the URL's query.
- **`login.test.ts`:** a canned server answers code, grant, then `/api/auth/me` 200. The credentials file under a temp `XDG_CONFIG_HOME` holds the token, and stdout is empty without `--json`.

**Done when:** `pnpm --dir node-cli exec vitest run src/device.test.ts src/login.test.ts` passes; Biome is clean.

**Commit:** `Add the device flow and cetra-cli login`

---

## C4 — `query`

**Goal:** `cetra-cli query "<sql>"` on the linked project, with a table or `--json`.

**Edit:** `node-cli/src/query.ts`, `table.ts`, `since.ts` (new) and their tests; `src/cli.ts`.

**Steps.**
1. **`since.ts`:** `export function sinceWindow(text: string, now: Date): { from: string; to: string }`. This is the Rust `since_window`:
   - the last character is the unit, `s`, `m`, `h` or `d`;
   - the rest is digits with no leading `0` and no sign;
   - refused input throws `CliError("--since expects a duration such as 30m, 24h or 7d", Exit.Usage)`.

   The times are RFC 3339 with 9 fractional digits and `Z`, for example `2026-01-01T00:00:00.000000000Z`. Build them from `toISOString()` by replacing the three-digit milliseconds with nine digits (milliseconds followed by `000000`).
2. **`table.ts`:** `export function renderTable(columns: {name: string; type: string}[], rows: Record<string, unknown>[]): string`. This is the Rust `output.rs` table (code facts § 4 "query.rs" table bullet):
   - a header line, a dash line, then the rows, with two spaces between columns;
   - a numeric column is right-aligned;
   - a null cell is `NULL`, and an array or object is compact JSON;
   - control characters become spaces;
   - a cell over 60 characters becomes its first 59 then `…`;
   - trailing spaces are trimmed;
   - no columns gives the empty string.
3. **`query.ts`:** `export async function query(args: string[], io: Io): Promise<ExitCode>`.
   - **Options:** one positional `<sql>`; `--since <d>` (default `24h`); `--from <t>` and `--to <t>`, which need each other and conflict with `--since`, so `Exit.Usage`; `--limit <n>` (default 100; 1–10000); `--json`; `--url`; `--project`; `--help`.
   - **Target:**
     - the server is `--url`, else `CETRA_URL`, else the link's `server`;
     - the project is `--project`, else `CETRA_PROJECT`, else `<link.org>/<link.project>`;
     - the token is `CETRA_TOKEN`, else the credentials file under the normalized server.
   - **Missing pieces:**
     - no server: "no server: run `cetra-cli init`, or pass --url" (`Exit.Usage`);
     - no project: "no project: run `cetra-cli init`, or pass --project";
     - no token: "no saved token for <server>; run `cetra-cli login --url <server>`" (`Exit.SignIn`).
   - An empty statement → "the query is empty" (`Exit.Usage`).
   - **Request.** `POST {server}/api/v1/projects/<encodeURIComponent(project)>/sql` with `{query, from, to, limit}`, the bearer token and a 90 s timeout.
   - **Answer:**
     - 200 → with `--json`, print `JSON.stringify({columns, rows, truncated})` and a newline on stdout. Otherwise print `renderTable` on stdout, and on stderr `1 row` or `<n> rows`, plus `; more rows matched, raise --limit` when `truncated`;
     - 401 → `problemMessage(…)`, `Exit.SignIn`;
     - other non-2xx → `problemMessage(…)`, `Exit.Query`.
   - Wire it in `cli.ts`.

**Tests:**
- `sinceWindow` on good and bad input, with exact strings for a fixed `now`;
- `renderTable`: alignment, NULL, JSON cells, the cut at 60, control characters, no columns;
- **`query` against a canned server:**
  - the request path, with the project encoded as `acme%2Fdemo`;
  - the body;
  - the bearer header;
  - `--json` output;
  - the table output and the row note;
  - truncated;
  - a 400 problem with a position gives `Exit.Query` and the message;
  - a 401 gives `Exit.SignIn` (4);
  - resolution through a link file in a temp directory;
  - the three missing-piece messages;
  - `--from` without `--to` gives `Exit.Usage`.

**Done when:** `pnpm --dir node-cli exec vitest run src/since.test.ts src/table.test.ts src/query.test.ts` passes; Biome is clean.

**Commit:** `Add cetra-cli query`

---

## C5 — `init`

**Goal:** `cetra-cli init` as design § `@cetra-ai/cli` § `init`, step by step, plus § Local server, § Skill install and § Prompt.

**Edit:** `node-cli/src/init.ts`, `local.ts`, `skill.ts`, `prompt.ts`, `prompt-io.ts` (new) and their tests; `src/cli.ts`.

**Steps.**
1. **`prompt-io.ts`.**
   - `export async function ask(question: string, io: Io, fallback: string): Promise<string>` uses `node:readline/promises` on stdin and stderr.
   - `export function interactive(): boolean` is `process.stdin.isTTY && process.stderr.isTTY`.
2. **`skill.ts`:**

   ```ts
   export const SKILL_TARGETS = { claude: ".claude/skills/cetra", agents: ".agents/skills/cetra" } as const;
   export const MARKER = ".cetra-skill";
   /** The skill shipped in the package: dist/skills/cetra next to the running bundle. */
   export function bundledSkill(): string;   // fileURLToPath(new URL("./skills/cetra/", import.meta.url))
   export async function installSkill(source: string, root: string, targets: (keyof typeof SKILL_TARGETS)[],
     version: string): Promise<{ installed: string[]; skipped: { path: string; reason: string }[] }>;
   /** The git top of <dir>, else <dir>. */
   export async function skillRoot(dir: string): Promise<string>;
   ```

   **`installSkill`**, for each target:
   - When the directory exists **without** `MARKER`, skip it with the reason "not installed by cetra-cli; left alone".
   - Otherwise remove it, copy `source` recursively (`fs.cp`, `recursive: true`), and write `MARKER` with the version and a newline.
   - A copy error skips that target with its reason. It does not throw: skill install failures are warnings.
3. **`prompt.ts`:** `export function agentPrompt(values: { project: string; envFile: string; tracesUrl: string; skill: boolean }): string`. It returns this text exactly, with the values filled in; when `skill` is false, step 1 reads "Read https://cetra.sh/llms.txt.":

   ```
   Add cetra tracing to this app.

   1. Read the cetra skill (.claude/skills/cetra or .agents/skills/cetra). If it is not there, read https://cetra.sh/llms.txt.
   2. Find the app's language and LLM libraries, and install the cetra SDK with the matching extras or instrumentations. Call init once at startup. The API key and URL are CETRA_API_KEY and CETRA_BASE_URL in <envFile>: load that file the way the app loads its settings. Never paste the key into code or chat.
   3. Run the app, or its tests, once so that it makes at least one LLM call. Flush before exit in short scripts.
   4. Check that traces arrived:
      npx @cetra-ai/cli query "SELECT trace_id, name, start_time, llm_count, error_count FROM traces ORDER BY start_time DESC LIMIT 5" --since 15m --json
      If it returns no rows, follow the troubleshooting steps of the skill.
   5. Tell me what you changed, and give me this link: <tracesUrl>
   ```

   The project (`<project>`) appears in the link only: the link file holds it for `query`.
4. **`local.ts`:**

   ```ts
   export function isLoopback(url: URL): boolean;   // localhost, 127.0.0.0/8, ::1
   export async function isReady(server: string): Promise<boolean>; // GET /readyz, 2 s timeout, 200 → true
   export function localDataDir(env: NodeJS.ProcessEnv, platform = process.platform, home = os.homedir()): string;
   //   Linux: $XDG_DATA_HOME (absolute) else ~/.local/share; macOS: ~/Library/Application Support; + "cetra/local"
   export function generatedPassword(): string;     // 24 chars from [A-Za-z0-9], crypto.randomInt
   export function configToml(dataDir: string, port: number): string;
   export interface Started { how: "binary" | "docker"; firstStart: boolean; stop: string; start: string;
     email?: string; password?: string }
   export async function startLocal(server: URL, dir: string, options: { yes: boolean; io: Io }): Promise<Started>;
   export async function passwordSignIn(server: string, email: string, password: string, host?: string):
     Promise<{ token: string; key: string; org: string; project: string; projectId: string }>;
   ```

   - **`configToml`** returns exactly:

     ```
     data_dir = "<dataDir>"
     public_url = "http://localhost:<port>"
     http_addr = "127.0.0.1:<port>"
     otlp_http_addr = "127.0.0.1:4318"
     grpc_addr = "127.0.0.1:4317"
     ```

     The data dir is a TOML basic string: escape `\` and `"`.
   - **`startLocal`:**
     1. **First start or not.** `firstStart` is true when `<data dir>` does not exist or is empty.
     2. **The `cetra` binary.** `which cetra` (`execFile("cetra", ["version"])` succeeding means it is on PATH). Ask "No cetra server at <server>. Start one with `cetra serve`? [Y/n]"; `--yes` answers yes, and with neither a terminal nor `--yes` throw `CliError("no cetra server at <server>; pass --yes to start one", Exit.Unreachable)`. Then:
        - create the data dir and write `cetra.toml` there;
        - on a first start, the child's environment gets `CETRA_INIT_ADMIN_EMAIL` (asked, default `admin@localhost`; `--yes` takes the default), `CETRA_INIT_ADMIN_PASSWORD` (generated), `CETRA_INIT_ORG=local`, and `CETRA_INIT_PROJECT` (the base name of `dir`, or `default` if that is empty);
        - spawn `cetra serve --config <data dir>/cetra.toml`, detached, with stdout and stderr appended to `<data dir>/server.log`, then `unref()`;
        - write the PID to `<data dir>/server.pid`;
        - `stop` is `kill <pid>`, and `start` is `cetra serve --config <data dir>/cetra.toml`.
     3. **Else Docker.** `docker version` succeeds: ask "No cetra server at <server>. Start one with Docker? [Y/n]", with the same answers. Run `docker run -d --name cetra-local -p 127.0.0.1:<port>:8080 -p 127.0.0.1:4317:4317 -p 127.0.0.1:4318:4318 -v cetra-local:/data -e CETRA_PUBLIC_URL=http://localhost:<port>` with the same `CETRA_INIT_*` variables as `-e` pairs, then `ghcr.io/cetra-ai/cetra:<VERSION>`.
        - `firstStart` is true when `docker volume inspect cetra-local` fails beforehand.
        - A failed `docker run` throws `CliError("docker could not start cetra (the image is public from the first release); install the binary instead: curl -fsSL https://github.com/cetra-ai/cetra/releases/latest/download/install.sh | sh", Exit.Unreachable)`.
        - `stop` is `docker stop cetra-local`, and `start` is `docker start cetra-local`.
     4. **Neither:** throw `CliError("no cetra server at <server>, and neither cetra nor docker is installed; see https://cetra.sh/docs/get-started/run-cetra/", Exit.Unreachable)`.
     5. **Wait.** Poll `isReady` every 500 ms for up to 30 s. A timeout throws `CliError("cetra did not start; see <data dir>/server.log", Exit.Unreachable)`, or `docker logs cetra-local` for Docker.
   - **`passwordSignIn`.** Every request carries `Origin: <server>`, because the server's `public_url` is that URL.
     1. `POST /api/auth/login` with `{email, password}`. Take the `cetra_session` cookie from `headers.getSetCookie()`. A failure throws "could not sign in to the local server" (`Exit.SignIn`).
     2. `GET /api/auth/me`, with the cookie: the org is the one whose `demo` is false. Then `GET /api/v1/orgs/{org}/projects?limit=1000`: the project is the first one.
     3. `POST /api/v1/me/tokens` with `{name: "CLI init on <host>" (or "CLI init"), scopes: ["read"], expires_at: <now + 90 days, ISO>}`. Expect 201 and take `secret`.
     4. `POST /api/v1/projects/<org>%2F<project>/api-keys` with `{kind: "secret", name: <same name>, scopes: ["ingest"]}`. Expect 201 and take `secret`.
     5. `POST /api/auth/logout`, whose failure is ignored.
     6. Check the token and the key with `isPersonalToken` and `isSecretKey`.
5. **`init.ts`:** `export async function init(args: string[], io: Io): Promise<ExitCode>`.
   - **Options:** `--url`, `--project <org/project>`, `--dir <path>` (default cwd), `--env-file <path>` (default `<dir>/.env`), `--agents <list>` (comma list of `claude`, `agents`; default both), `--no-skill`, `--no-browser`, `--yes`, `--json`, `--help`. `--agents` together with `--no-skill` is a usage error.
   - **Steps,** in the design's order:
     1. **The server.** `--url`, else `CETRA_URL`, else the link's `server`, else `https://app.cetra.sh`, through `serverUrl`.
     2. **Not ready.** If `isReady(server)` is false: on a loopback URL call `startLocal`, otherwise throw `CliError("no cetra server at <server>", Exit.Unreachable)`.
     3. **Sign in.** On a local first start, `passwordSignIn` gives the token, key and project, and `grantServer = server`. Otherwise `deviceSignIn({purpose: "init", project: --project})` gives `grant`, and `grantServer = grant.server`.
     4. `saveToken(credentialsPath, server, token)`.
     5. **The env file.**
        - Read it if present.
        - When it has a `CETRA_API_KEY` and `CETRA_BASE_URL` equals `grantServer`, ask "Replace the CETRA_API_KEY in <file>? [y/N]". `--yes` replaces it. Without a terminal and without `--yes`, keep the old key and say so on stderr.
        - `setVariables` with `CETRA_API_KEY` and `CETRA_BASE_URL = grantServer`, then `writeEnvFile`. A failure throws `CliError("could not write <base name>", Exit.Write)`.
        - Then `ignoreInGit`, and print `Added <name> to .gitignore.` when it returns true.
     6. **The link.** `writeLink(dir, {server, org, project, project_id})`. A failure gives `Exit.Write`.
     7. **The skill.** Unless `--no-skill`, `installSkill(bundledSkill(), await skillRoot(dir), targets, VERSION)`. Print each install, and each skip as `warning: …`.
     8. **The prompt.**
        - `tracesUrl` is `<grantServer>/<org>/<project>/traces`.
        - On stderr: a blank line, `Paste this prompt into your coding agent:`, a blank line, the prompt, a blank line, and `Traces: <tracesUrl>`.
        - On a local first start, also print `Admin: <email> / <password>`, `UI: <server>`, and the stop and start commands.
        - With `--json`, stdout gets exactly one object: `{server: grantServer, project: "<org>/<project>", env_file: <absolute path>, link_file: <absolute path>, skills: [<absolute paths>], prompt, traces_url}`. It never holds the token, the key or the password.
   - Wire it in `cli.ts`.

**Tests:**
- **`skill.test.ts`:**
  - an install into a temp root writes both targets and the marker;
  - a second install replaces them;
  - a directory without the marker is skipped;
  - `--agents claude` writes one target;
  - `bundledSkill()` resolves to an existing directory once built. This test reads `dist/skills/cetra/SKILL.md`, and `pnpm check` builds first; mark it `it.skipIf(!existsSync(...))` with a reason.
  - The bundled `SKILL.md` equals `../skills/cetra/SKILL.md`, and every reference file equals its source.
- **`prompt.test.ts`:** the exact text for given values, and the `skill: false` variant.
- **`local.test.ts`:**
  - `isLoopback` on `localhost`, `127.0.0.2`, `[::1]` and `example.com`;
  - `localDataDir` per platform;
  - `generatedPassword` length and charset;
  - the exact `configToml`, including a path with a `"` in it;
  - `passwordSignIn` against a canned server: the requests carry `Origin` and the cookie, the token request body, and the key request path and body.
- **`init.test.ts`**, against a canned server, with `XDG_CONFIG_HOME` and the cwd in temp directories:
  - a device path with `--yes --no-browser --json`: the stdout object, `.env` content and mode, the link file, the credentials, both skill targets, and no secret in stdout or stderr;
  - an existing `.env` with other lines is kept and updated;
  - in a temp git repo, `.env` is added to `.gitignore`;
  - an unreachable non-loopback URL gives exit 3;
  - an approval denied gives exit 4;
  - an unwritable env file (a directory with that name) gives exit 5;
  - `--agents` with `--no-skill` gives exit 2.

  `startLocal` is not exercised here; the e2e test covers the device path. Test `startLocal`'s refusal path (neither tool found) by passing a `PATH` with neither.

**Done when:** `pnpm --dir node-cli run build && pnpm --dir node-cli exec vitest run src/skill.test.ts src/prompt.test.ts src/local.test.ts src/init.test.ts` passes; Biome is clean.

**Commit:** `Add cetra-cli init`

---

## C6 — End-to-end test, docs and checks

**Goal:** `just e2e` proves `init` → span → `query` against a real server. The docs page and the reference page exist, and the checks know the new CLI.

**Edit:**
- `node-cli/src/e2e.test.ts` (new)
- `tools/e2e/run.sh`
- `website/src/content/docs/docs/get-started/coding-agent.mdx` (new)
- `website/src/content/docs/docs/reference/cetra-cli.md` (new)
- `website/src/content/docs/docs/get-started/quickstart.mdx`
- `website/src/content/docs/docs/reference/index.mdx`
- `website/astro.config.mjs`
- `website/scripts/check-skill.mjs`, `check-skill.test.mjs`
- `website/scripts/check-env-vars.mjs`, `check-env-vars.test.mjs`
- `CHANGELOG.md`, `AGENTS.md`, `docs/roadmap.md`, `docs/spec/09-api.md`

**Steps.**
1. **`tools/e2e/run.sh`.**
   - Add `CETRA_PUBLIC_URL="$base"` to the server's environment, where `$base` is the `http://127.0.0.1:<port>` the script already computes. If it computes it after the start, move that computation before.
   - After the TypeScript SDK suite, add:

     ```bash
     export CETRA_E2E_ADMIN_EMAIL=ada@example.com CETRA_E2E_ADMIN_PASSWORD=cetra-e2e-1
     pnpm --dir node-cli run build
     pnpm --dir node-cli exec vitest run src/e2e.test.ts
     ```
2. **`src/e2e.test.ts`** runs with `describe.skipIf(!process.env.CETRA_E2E_BASE_URL)`:
   1. Make a temp `dir` and a temp `XDG_CONFIG_HOME`.
   2. Spawn `node dist/cli.js init --url $BASE --dir <dir> --yes --no-browser --json --project acme/e2e` with that environment.
   3. Read stderr until the `Code: XXXX-XXXX` line.
   4. Approve as the admin: `POST /api/auth/login` with `Origin: $BASE`, then `GET /api/v1/orgs/acme/projects`, taking the `e2e` project's id, then `POST /api/auth/device/<code>/approve` with `{project_id}`, the cookie and the `Origin`.
   5. Wait for the child to exit 0, and parse its stdout JSON.
   6. Read `CETRA_API_KEY` from `<dir>/.env`, then send one span: copy the minimal OTLP JSON request and the helper of `sdks/typescript/src/e2e.test.ts`, or post the protobuf fixture it uses. If that file has no reusable helper, stop and report.
   7. Poll `node dist/cli.js query "SELECT trace_id FROM traces" --since 1h --json`, run with cwd `<dir>`, until a row appears, for at most 30 s.
   8. Assert that the skill directories exist in `<dir>`, which is not a git repo, so the root is `dir`.
3. **`coding-agent.mdx`** (Get started), in STYLE.md's voice. Title "Set up with your coding agent". Sections:
   - **The one-liner** (`npx @cetra-ai/cli@latest init`), and what it asks in the browser.
   - **What it changes:** `.env`, `.gitignore`, `.cetra/project.json`, the credentials file, and the skill directories.
   - **The prompt**, in a `text` fence.
   - **Self-hosted** (`--url`, and a local server started for you).
   - **Without the CLI:** `npx skills add cetra-ai/skills`, setting `CETRA_API_KEY` and `CETRA_BASE_URL` yourself, and the same prompt.
   - **Check it yourself** (`npx @cetra-ai/cli query`).
   - **Tracing the coding agent itself is different:** one sentence and a link to `/docs/integrations/claude-code/`.

   Only `bash` and `text` fences, so `check-samples` has nothing to mark.
4. **`reference/cetra-cli.md`.** Title "cetra-cli", description "Every cetra-cli command and option: init, query and login." Sections `## init`, `## query`, `## login`, with headings written as `### \`cetra-cli <name>\``, plus:
   - their options, from C3–C5;
   - the exit-code table;
   - the files `init` writes;
   - the environment variables it reads: `CETRA_URL`, `CETRA_TOKEN`, `CETRA_PROJECT`, `XDG_CONFIG_HOME`.
5. **Sidebar** (`astro.config.mjs`):
   - add `{ label: "Set up with your coding agent", link: "/docs/get-started/coding-agent/" }` as the **first** item of the Get started group;
   - add `{ label: "cetra-cli", link: "/docs/reference/cetra-cli/" }` after the `CLI` item of the Reference topic.

   **`reference/index.mdx`:** add a `LinkCard` for cetra-cli on one line, as the integrations cards are, after the CLI card.
6. **`quickstart.mdx`.** Before `## Run cetra`, add an `<Aside>` of two sentences: "Working with a coding agent? `npx @cetra-ai/cli@latest init` sets everything up and gives you a prompt for it." plus a link to `/docs/get-started/coding-agent/`. The page already imports `Aside`.
7. **`check-skill.mjs`:**
   - Add `export const NODE_CLI_REFERENCE = "src/content/docs/docs/reference/cetra-cli.md";` and `export function nodeCliCommands(text)`, which reads the headings ``### `cetra-cli <name>` ``.
   - In `checkSkill`, on the same code texts as the `cetra` rule, match `/(?:^|[\s`("'])(?:npx @cetra-ai\/cli(?:@[\w.-]+)?|cetra-cli) ([a-z][a-z-]*)/g`. The name must be in `ctx.nodeCommands`; otherwise report ``unknown command `cetra-cli <name>` ``.
   - `realContext()` adds `nodeCommands`.
   - **Tests:** a known `npx @cetra-ai/cli@latest init` passes; `npx @cetra-ai/cli frob` is reported; `nodeCliCommands` on the real page gives `init, query, login`.
8. **`check-env-vars.mjs`.** Add `"node-cli/src"` to the code directories it scans (code facts § 0.11). Add a test case with a page naming a `CETRA_*` that only `node-cli/src` contains.
9. **`CHANGELOG.md` `[Unreleased]` `### Added`:** "`npx @cetra-ai/cli init` sets an app up for cetra in one command and prints a prompt for your coding agent; `cetra-cli query` checks the traces from the terminal; the cetra agent skill is installed for Claude Code, Codex, Cursor, Gemini CLI and Copilot."
10. **`AGENTS.md`:**
    - Layout: `node-cli/                     the cetra-cli package (npx @cetra-ai/cli): init, query, login (D-AO-2)` after `sdks/typescript/`.
    - Commands: `| \`just check-cli-js\` | cetra-cli: lint, types, build, tests, package lint |`.
    - In the `just check` row, list cetra-cli.
11. **Spec 09 § CLI.** After the table, add one paragraph: "`npx @cetra-ai/cli` (`cetra-cli`, package `node-cli/`) is the setup CLI: `init`, `query`, `login` (D-AO-2). Its reference is the site page `reference/cetra-cli`."
12. **`docs/roadmap.md`.** The AO row's Status gets `M3 implemented (\`@cetra-ai/cli\`)`, keeping M1 and M2.

**Tests:** `e2e.test.ts`, and the new `check-skill` and `check-env-vars` cases.

**Done when:**
- `cd website && node --test scripts/check-skill.test.mjs scripts/check-env-vars.test.mjs` passes;
- `cd website && node scripts/check-skill.mjs && node scripts/check-env-vars.mjs` exits 0;
- the website files pass `cd website && ../node_modules/.bin/biome check --write scripts/`.

`just e2e` runs in the final check.

**Commit:** `Document cetra-cli and test it end to end`

---

## C7 — CI and release

**Goal:** CI checks the package on Node 26 and 22.12, and a release builds, packs and publishes it next to the SDK.

**Edit:** `.github/workflows/ci.yml`, `.github/workflows/release.yml`, `tools/packaging/release-prep.sh`, `tools/packaging/versions-check.sh`, `docs/plans/n-packaging/README.md`, `docs/mvp-decisions.md`, `justfile`.

**Steps.**
1. **`ci.yml`.** Copy the `ts-sdk` job as `cli-js` (`just check-cli-js`, then `just assert-clean`). Copy `ts-sdk-min-node` as `cli-js-min-node`:
   - build and pack `node-cli` to `$RUNNER_TEMP`;
   - set up Node `22.12.0` with the same pinned action line;
   - run `just cli-smoke <tarball>`;
   - run `node node_modules/vitest/vitest.mjs run` in `working-directory: node-cli`.

   Add both jobs to `ci-ok`'s `needs`. Copy the action SHAs from the existing jobs.
2. **`justfile` `cli-smoke tarball`.** It installs the tarball into a temp directory with `npm install --ignore-scripts --before <7 days ago>`, as `ts-smoke` does, then checks that `node_modules/.bin/cetra-cli --version` prints the tarball's version. Copy `ts-smoke`'s date and temp-dir lines.
3. **`release.yml`.**
   - In the `sdks` job, after the SDK build lines, add:
     - `just check-cli-js`, as its own step after `just check-ts`;
     - `pnpm --dir node-cli run build`;
     - `pnpm --dir node-cli pack --pack-destination "$out/npm"`;
     - `ls "$out/npm/cetra-ai-cli-$VERSION.tgz"`.
   - In the `npm` job, after the SDK's `npm publish`, add the same command for `cetra-ai-cli-$VERSION.tgz`.
   - Update the header comment to name the CLI.
4. **`release-prep.sh` and `versions-check.sh`.** Add `node-cli/package.json` and `node-cli/src/version.ts` exactly as the SDK's two files are handled: the `sed` lines and the `.bak` removal in release-prep, the `expect` lines in versions-check, and the header comments.
5. **`docs/plans/n-packaging/README.md`:**
   - **§ Versions:** list the two files.
   - **§ Releasing**, item 4: add the CLI's one-time steps, which mirror the SDK's:
     - the first manual publish of `@cetra-ai/cli` with `--tag placeholder`;
     - its trusted publisher (org `cetra-ai`, repo `cetra`, workflow `release.yml`, environment `npm`);
     - "Publishing access" requiring 2FA and disallowing tokens.
6. **`docs/mvp-decisions.md`.** Append to D-N-37: "`@cetra-ai/cli` (D-AO-2) is published by the same jobs, with the same version and dist-tags."

**Done when:**
- `just versions-check` passes;
- `just check-workflows` passes, if `actionlint` and `zizmor` are installed; otherwise say so;
- `just cli-smoke` on a locally packed tarball passes (`pnpm --dir node-cli pack --pack-destination /tmp/claude-1000/cli-pack`).

**Commit:** `Check and publish cetra-cli in CI and releases`

---

## After C7 (the coordinating session)

1. `just fmt`, then `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check` and `just e2e`, once.
2. A Sonnet code review of C1–C7, then the fixes.
3. Tell the user about the manual npm steps for the new package.
