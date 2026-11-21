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
- `pnpm install` when it changes dependencies;
- the type check, `pnpm --dir node-cli exec tsc --noEmit -p tsconfig.test.json`, which every phase's "Done when" includes: vitest does not check types, and strict mode errors would otherwise pile up until C7.

The coordinating session runs `just check` once after C7.

**Test rules for every phase:**
- Every flow test passes `--no-browser`.
- Commands take an optional third argument `deps: Deps` for tests: `{ sleep?, now?, skillSource?, openBrowser?, spawn? }`. Production calls pass nothing.
- No test touches the developer's real files. `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and the working directory are temp directories, and `GIT_DIR` and `GIT_INDEX_FILE` are removed from child environments.

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
  - a pointer at the top of the quick start;
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
| `smol-toml` | `1.9.1` (2026-10-09, BSD-3-Clause, no deps) | `node-cli` devDependencies, exact | the credentials file. It is bundled, so `node-cli/LICENSE` appends its BSD-3-Clause notice (copy it from its package) under a line "This package bundles smol-toml:" (C1) |
| `typescript`, `vitest`, `tsdown`, `@types/node`, `publint` | the exact versions `sdks/typescript/package.json` pins | `node-cli` devDependencies | the same toolchain as the SDK |

Re-check `smol-toml` with `npm view smol-toml version time --json` and report the version used.

## Phases

```
C1 (skeleton) ─► C2 (http, credentials, link, env file) ─► C3 (device flow, login) ─► C4 (query)
   ─► C5a (skill, prompt) ─► C5b (local server) ─► C5c (init) ─► C6a (e2e) ─► C6b (docs, checks) ─► C7 (CI, release)
```

The phases run in order: each uses the modules of the one before. Files per phase:
- C1: `node-cli/{package.json, tsconfig.json, tsconfig.test.json, tsdown.config.ts, README.md, LICENSE, .gitignore, src/cli.ts, src/version.ts, src/errors.ts, src/args.ts, src/cli.test.ts}`, `pnpm-workspace.yaml`, `pnpm-lock.yaml`, `biome.json`, `justfile`.
- C2: `node-cli/src/{http.ts, credentials.ts, link.ts, envfile.ts}` and their tests.
- C3: `node-cli/src/{device.ts, browser.ts, host.ts, login.ts}` and their tests, `node-cli/src/test-server.ts` (test helper), `src/cli.ts`.
- C4: `node-cli/src/{query.ts, table.ts, since.ts}` and their tests, `src/cli.ts`.
- C5a: `node-cli/src/{prompt-io.ts, skill.ts, prompt.ts}` and their tests.
- C5b: `node-cli/src/local.ts` and its test.
- C5c: `node-cli/src/init.ts` and its test, `src/cli.ts`.
- C6a: `node-cli/src/e2e.test.ts`, `tools/e2e/run.sh`.
- C6b: `website/` pages, `website/astro.config.mjs`, `website/scripts/{check-skill,check-env-vars}.mjs` and tests, `CHANGELOG.md`, `AGENTS.md`, `docs/roadmap.md`, `docs/spec/09-api.md`.
- C7: `.github/workflows/{ci.yml, release.yml}`, `tools/packaging/{release-prep.sh, versions-check.sh}`, `docs/plans/n-packaging/README.md`, `docs/mvp-decisions.md`, `justfile`.

---

## C1 — The package skeleton

**Goal:** an empty-but-working `@cetra-ai/cli` package: it builds, prints `--version` and `--help`, and runs its tests in the workspace's checks.

**Edit:**
- `node-cli/package.json`, `node-cli/tsconfig.json`, `node-cli/tsconfig.test.json`, `node-cli/tsdown.config.ts`, `node-cli/README.md`, `node-cli/LICENSE`, `node-cli/.gitignore` (all new)
- `node-cli/src/cli.ts`, `node-cli/src/version.ts`, `node-cli/src/errors.ts`, `node-cli/src/args.ts`, `node-cli/src/cli.test.ts` (new)
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
     // `platform: "node"` would name the file cli.mjs; the package is `type: module`, so .js is right.
     fixedExtension: false,
     copy: [{ from: "../skills/cetra", to: "dist/skills" }],
   });
   ```

   **Check after the first build:** `ls node-cli/dist` shows `cli.js`, and `node-cli/dist/skills/cetra/SKILL.md` and `node-cli/dist/skills/cetra/references/` exist. If not, stop and report what `ls -R node-cli/dist` shows. `dist/cli.js` must start with `#!/usr/bin/env node`: tsdown keeps it from `src/cli.ts` and makes the file executable.
4. **`.gitignore`** has one line, `.vitest/`, as the SDK's. Do **not** ignore `dist/` there: the root `.gitignore` already does.
5. **`LICENSE`**: a copy of `sdks/typescript/LICENSE`, then a blank line, `This package bundles smol-toml, under the following licence:`, and smol-toml's `LICENSE` text from its package in the pnpm store (after `pnpm install`; find it with `find node_modules/.pnpm -path '*smol-toml*/LICENSE'`). `README.md`:
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

   In the same file, the shared I/O type, so that command modules do not import `cli.ts`:

   ```ts
   import type { execFile, spawn } from "node:child_process";

   /** Output takes one line at a time, without its newline; realIo() adds it. */
   export interface Io { stdout: (line: string) => void; stderr: (line: string) => void; env: NodeJS.ProcessEnv; cwd: string }
   /** Test seams; production passes nothing. sleep and now: C3 (device flow); skillSource: C5a/C5c;
    *  spawn and execFile: C3 (browser) and C5b (local server). */
   export interface Deps {
     sleep?: (ms: number) => Promise<void>;
     now?: () => number;            // milliseconds, like Date.now
     skillSource?: string;
     spawn?: typeof spawn;
     execFile?: typeof execFile;
   }
   /** The real Io: process.stdout / process.stderr with "\n" added, process.env, process.cwd(). */
   export function realIo(): Io;
   /** Characters to strip or refuse: one place, so Biome's noControlCharactersInRegex is never hit. */
   export const CONTROL = /\p{Cc}/gu;
   ```

   Use `CONTROL` (or `new RegExp(CONTROL.source, "u")` for a test without `g`) wherever this plan says "control characters". A literal `[\u0000-\u001f]` class fails Biome.

   **`src/args.ts`** wraps `node:util` `parseArgs`:

   ```ts
   import { parseArgs, type ParseArgsOptionsConfig } from "node:util";

   type Parsed<T extends ParseArgsOptionsConfig> = ReturnType<
     typeof parseArgs<{ args: string[]; options: T; strict: true; allowPositionals: true }>
   >;
   /** parseArgs with `strict: true` and positionals allowed; any parse error becomes
    *  CliError(<its message>, Exit.Usage). A command that takes no positional checks
    *  `positionals.length === 0` itself. */
   export function parseOptions<const T extends ParseArgsOptionsConfig>(args: string[], options: T): Parsed<T>;
   /** The options list of a command, one line per option, for `--help`. */
   export function optionsHelp(command: string, summary: string, lines: [string, string][]): string;
   ```

   Every command declares `help: { type: "boolean", short: "h" }`; when `values.help === true` it prints `optionsHelp`'s text on stderr and returns 0. If the `Parsed` type does not compile as written, keep the function's behaviour, use the nearest type that compiles, and report it. The option lines are the command's options from this plan, with one short description each.
8. **`src/cli.ts`:**
   - starts with `#!/usr/bin/env node`;
   - `export const COMMANDS: Record<string, (args: string[], io: Io) => Promise<ExitCode>> = {}`. C3, C4 and C5c each add one entry. C1 registers none: there are no placeholder commands;
   - `export async function main(argv: string[], io: Io, commands = COMMANDS): Promise<ExitCode>`;
   - it reads the first positional argument as the command;
   - `--version` prints `VERSION` on stdout and returns 0;
   - `--help`, `-h` or no argument prints the usage text below on stderr and returns 0 (no argument returns 2);
   - an unknown command prints `cetra-cli: unknown command <name>` plus the usage and returns 2;
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
- through `main(argv, io, { fake: async () => { throw new CliError("x", Exit.Write); } })`, `fake` prints `cetra-cli: x` and returns 5; a fake throwing a plain `Error("y")` prints `cetra-cli: y` and returns 1;
- `parseOptions` turns an unknown option into `Exit.Usage`.
- `VERSION` equals `package.json`'s `version`.

**Done when:**
- `pnpm --dir node-cli run build` succeeds and `dist/skills/cetra/SKILL.md` exists;
- `pnpm --dir node-cli exec vitest run src/cli.test.ts` passes;
- `node node-cli/dist/cli.js --version` prints the version;
- the type check passes;
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

   - **Path.** On Linux: `$XDG_CONFIG_HOME` when it is absolute, else `~/.config`. On macOS: `~/Library/Application Support`. On Windows: `%APPDATA%`. Then `cetra/credentials.toml`. Join with `path.win32.join` when `platform` is `win32`, else `path.posix.join`, so the tests can check exact strings for every platform on any host.
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
   /** The first ancestor of `dir`, itself included, that holds a `.git` entry (file or directory); undefined when none. */
   export async function gitTop(dir: string): Promise<string | undefined>;
   export async function writeLink(dir: string, link: ProjectLink): Promise<string>;   // returns the absolute path
   /** Looks in `cwd`, then its parents, up to the git work tree's top (or the filesystem root). */
   export async function findLink(cwd: string): Promise<ProjectLink | undefined>;
   ```

   - The file is pretty JSON with two spaces and a trailing newline, mode 0644. It is created with its directory.
   - A file that is not valid JSON, or that lacks one of the four string fields, throws `CliError(".cetra/project.json is invalid", Exit.Error)`.
   - **The walk.** `findLink` looks in `cwd` and each parent, and stops after `gitTop(cwd)` when there is one, else at the filesystem root.
4. **`envfile.ts`:**

   ```ts
   /** Sets the variables in the env file's text; other lines, comments and order are kept. */
   export function setVariables(text: string | undefined, values: Record<string, string>): string;
   export function readVariable(text: string, name: string): string | undefined;
   export async function writeEnvFile(path: string, text: string): Promise<void>;  // 0600, tmp + rename
   /** Adds the env file (its path relative to <dir>) to <dir>/.gitignore when <dir> is in a git work tree and git
    *  does not ignore it. Returns true when it added the line. */
   export async function ignoreInGit(dir: string, envPath: string): Promise<boolean>;
   ```

   - **`setVariables`.**
     - **Line endings and BOM.** If the text contains `\r\n`, the result uses `\r\n`, else `\n`. A leading BOM is kept. Lines are split on `\n` with any trailing `\r` removed before matching.
     - A line matching `^\s*(export\s+)?NAME\s*=` is replaced with `NAME=value`, keeping its `export ` prefix.
     - Missing variables are appended in the order given.
     - When the text was `undefined`, the result starts with `# cetra (npx @cetra-ai/cli init)`.
     - The result ends with one newline.
     - Values are written bare. Both values must match `^[A-Za-z0-9:/._\[\]-]+$`; otherwise throw `CliError("invalid value for <NAME>", Exit.Write)`. `init` validates the server URL before the sign-in (C5c), so this cannot fail after the secrets exist.
   - **`readVariable`** reads the last matching line's value, without a trailing `\r` and without surrounding quotes.
   - **`ignoreInGit(dir, envPath)`** (the second argument is the env file's path):
     - If the env file is not inside `dir`, return false.
     - If `git` is missing or `<dir>` is not in a work tree (`git rev-parse --is-inside-work-tree` fails), return false.
     - Run `git check-ignore -q <path relative to dir>` in `dir`. Exit 0 means ignored: return false. Exit 1 means not ignored: append `\n<relative path>\n` to `<dir>/.gitignore` (create the file if needed, without doubling blank lines) and return true. Any other exit returns false.
     - Spawn `git` with `execFile`: no shell, arguments as an array, and an environment without `GIT_DIR` and `GIT_INDEX_FILE`.

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
- **`link.test.ts`:** write then find from a subdirectory; stop at a `.git` boundary; an invalid file; `gitTop` with a `.git` directory, a `.git` file, and none.
- **`envfile.test.ts`:**
  - replace in place, keep comments and order, keep `export`;
  - a CRLF file stays CRLF, a BOM is kept, and `readVariable` on a CRLF file returns the value without `\r`;
  - `http://[::1]:8080` is a valid value;
  - append missing variables;
  - the new-file header;
  - an invalid value throws;
  - `readVariable` with quotes;
  - `ignoreInGit` in a temp git repo (run `git init`, without `GIT_DIR` in the environment): a first call adds the line and returns true, a second returns false; outside a repo, and for an env file outside `dir`, it returns false.

**Done when:**
- `pnpm --dir node-cli exec vitest run src/http.test.ts src/credentials.test.ts src/link.test.ts src/envfile.test.ts` passes;
- the type check passes;
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
   - It uses `spawn` with `detached: true`, `stdio: "ignore"`, then `child.on("error", () => {})` and `unref()`. No shell. The `error` listener is what makes a missing program harmless: without it Node throws.
   - On Linux, when neither `DISPLAY` nor `WAYLAND_DISPLAY` is set, it does nothing: there is no display.
   - Signature: `openBrowser(url: string, deps: Deps = {}, env = process.env, platform = process.platform)`; it spawns with `deps.spawn ?? spawn`, so a test can pass a bogus program.
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
     stderr: (line: string) => void; deps?: Deps;
   }): Promise<Grant>;
   ```

   **`deviceSignIn`**, in this order. The messages are the Rust ones (code facts § 4), with `cetra-cli` instead of `cetra` where a command is named.
   1. **Request the code.** `POST {server}/api/auth/device/code` with `{purpose, host}`; `host` is left out when `localHost()` is `undefined`. No `Origin`, no token. A non-200 answer throws `CliError("the server refused a sign-in code (HTTP <n>)", Exit.SignIn)`.
   2. **Validate before printing.**
      - `verification_uri_complete` must parse with `new URL` and have the protocol `http:` or `https:`; otherwise throw "the server sent an invalid sign-in URL" (`Exit.SignIn`).
      - `user_code` must pass `isUserCode`; otherwise throw "the server sent an invalid sign-in code".
      - When `project` is given, `url.searchParams.set("project", project)`.
   3. **Print** to stderr exactly:
      - `Open <url.href> in your browser and approve the code.`;
      - `Code: XXXX-XXXX`.

      Unless `noBrowser`, call `openBrowser(url.href, deps)`.
   4. **Poll.**
      - `interval = max(interval * 1000, MIN_INTERVAL_MS)`, and the deadline is `now + min(expires_in, MAX_EXPIRES_IN_S) * 1000 + DEADLINE_SLACK_MS`.
      - Loop: `await sleep(interval)`. Past the deadline, throw "the sign-in code expired; run the command again" (`Exit.SignIn`). Otherwise `POST {server}/api/auth/device/token` with `{device_code}`.
      - A network error prints `warning: cannot reach the server; still waiting` once, then continues.
      - 200 → the grant. Otherwise `step(status, body)`:
        - 429, 502, 503, 504 → `wait`;
        - 400 with `code` `authorization_pending` → `wait`; `slow_down` → `interval += SLOW_DOWN_STEP_MS`; `access_denied` → throw "sign-in denied in the browser"; `expired_token` → throw the expired message;
        - any other 400, or any other status → throw "sign-in failed (HTTP <n>)".

        All of these throw with `Exit.SignIn`.
   5. **Check the grant.** `serverUrl(grant.server)` must not throw and must match the `.env` value pattern of C2; `isPersonalToken(grant.token)` must hold. For `init`, `grant.project` must be present and `isSecretKey(grant.ingest_key)` must hold. Otherwise throw "the server returned an invalid grant".
   6. Never print the token, the key or the device code.
4. **`test-server.ts`** (test helper, excluded from the build): `startCanned(answers: { status: number; body: unknown; headers?: Record<string,string> }[]): Promise<{ url: string; requests: { method: string; path: string; headers: Record<string,string>; body: unknown }[]; close(): Promise<void> }>`. It is a `node:http` server that answers in order and records each request; past the list it answers 500.
5. **`login.ts`:** `export async function login(args: string[], io: Io, deps: Deps = {}): Promise<ExitCode>`; it passes `deps` to `deviceSignIn`.
   - **Options** (`parseOptions`): `--url <URL>` (else `CETRA_URL`, else `chosenServer` of the saved file), `--no-browser`, `--json`, `--help`.
   - **Steps:**
     1. `server = serverUrl(…)`, then `deviceSignIn({purpose: "login"})`.
     2. Check the token with `GET {server}/api/auth/me` and the bearer token; a non-2xx answer throws "the server rejected the token" (`Exit.SignIn`).
     3. `saveToken(credentialsPath, server, grant.token)`, then print `Signed in to <server>. The token expires in 90 days.` on stderr.
   - With `--json`, print `{"server": "<server>"}` on stdout.
   - Add `login` to `COMMANDS` in `cli.ts`.

**Tests:**
- `openBrowser` with a nonexistent program does not throw or crash the process, and with no `DISPLAY`/`WAYLAND_DISPLAY` on Linux it spawns nothing.
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

**Done when:** `pnpm --dir node-cli exec vitest run src/device.test.ts src/login.test.ts` passes; the type check passes; Biome is clean.

**Commit:** `Add the device flow and cetra-cli login`

---

## C4 — `query`

**Goal:** `cetra-cli query "<sql>"` on the linked project, with a table or `--json`.

**Edit:** `node-cli/src/query.ts`, `table.ts`, `since.ts` (new) and their tests; `src/cli.ts`.

**Steps.**
1. **`since.ts`:** `export function sinceWindow(text: string, now: Date): { from: string; to: string }`. This is the Rust `since_window`:
   - the last character is the unit, `s`, `m`, `h` or `d`;
   - the rest is 1 to 9 digits with no leading `0` and no sign;
   - a window that puts `from` before 1970-01-01 is refused with the same message;
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
   - **Options:** exactly one positional `<sql>` (none or more is `Exit.Usage`); `--since <d>` (default `24h`); `--from <t>` and `--to <t>`, which need each other and conflict with `--since`, so `Exit.Usage`; `--limit <n>` (default 100; 1–10000); `--json`; `--url`; `--project`; `--help`.
   - **Target:**
     - the server is `--url`, else `CETRA_URL`, else the link's `server`;
     - the project is `--project`, else `CETRA_PROJECT`, else `<link.org>/<link.project>`;
     - the token: `CETRA_TOKEN` is used **only** when the server came from `--url` or `CETRA_URL`. When the server came from the link file, only the credentials file is read, under the exact normalized server. A link file can be committed, so a hostile repository could name any server: an environment token must never go to it.
     - The link's `server` goes through `serverUrl`; a bad one throws `CliError(".cetra/project.json is invalid")`.
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
   - `--limit` must be an integer from 1 to 10000; otherwise `Exit.Usage`.
   - Add `query` to `COMMANDS` in `cli.ts`.

**Tests:**
- `sinceWindow` on good and bad input, with exact strings for a fixed `now`; more than 9 digits is refused, and so is `99999d`;
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
  - with the server from a link file and `CETRA_TOKEN` set, the token is **not** sent (the canned server records no `Authorization`, and the command fails with the no-token message);
  - `--from` without `--to` gives `Exit.Usage`.

**Done when:** `pnpm --dir node-cli exec vitest run src/since.test.ts src/table.test.ts src/query.test.ts` passes; the type check passes; Biome is clean.

**Commit:** `Add cetra-cli query`

---

## C5a — Prompts, the skill installer, the agent prompt

**Goal:** the small pieces `init` needs: terminal questions, the skill install, and the prompt text.

**Edit:** `node-cli/src/prompt-io.ts`, `skill.ts`, `prompt.ts` (new), `skill.test.ts`, `prompt.test.ts`, `prompt-io.test.ts` (new).

**Steps.**
1. **`prompt-io.ts`:**
   - `export function interactive(): boolean` is `Boolean(process.stdin.isTTY && process.stderr.isTTY)`.
   - `export async function ask(question: string, fallback: string, input = process.stdin, output = process.stderr): Promise<string>` uses `node:readline/promises` `createInterface({ input, output })`. It closes the interface in a `finally`, or the process would hang. An empty answer gives `fallback`.
   - `export function yes(answer: string, defaultYes: boolean): boolean` trims and lower-cases the answer: `""` gives `defaultYes`, `y` and `yes` give true, and anything else gives false.
2. **`skill.ts`:**

   ```ts
   export const SKILL_TARGETS = { claude: ".claude/skills/cetra", agents: ".agents/skills/cetra" } as const;
   export const MARKER = ".cetra-skill";
   /** The skill shipped in the package: dist/skills/cetra next to the running bundle (dist/cli.js). */
   export function bundledSkill(): string;   // fileURLToPath(new URL("./skills/cetra/", import.meta.url))
   export async function installSkill(source: string, root: string, targets: (keyof typeof SKILL_TARGETS)[],
     version: string): Promise<{ installed: string[]; skipped: { path: string; reason: string }[] }>;
   /** (await gitTop(dir)) ?? dir — gitTop is link.ts's. */
   export async function skillRoot(dir: string): Promise<string>;
   ```

   - `bundledSkill()` is only right when running from the bundle. Callers take the source as a parameter (`deps.skillSource ?? bundledSkill()`). Tests pass the repository's `skills/cetra`, as `fileURLToPath(new URL("../../skills/cetra/", import.meta.url))` from `src/`.
   - **`installSkill`**, per target:
     - a directory that exists **without** `MARKER` is skipped with the reason "not installed by cetra-cli; left alone";
     - otherwise remove it, `fs.cp(source, target, { recursive: true })`, and write `MARKER` holding the version and a newline;
     - an error skips that target with its message. It never throws.
3. **`prompt.ts`:**
   - `export const CHECK_QUERY = "SELECT trace_id, name, start_time, span_count, llm_count, error_count FROM traces ORDER BY start_time DESC LIMIT 5";` This is the same statement as `skills/cetra/SKILL.md`'s step 5.
   - `export function agentPrompt(values: { envFile: string; tracesUrl: string; skill: boolean }): string` returns this text exactly, with the values filled in. When `skill` is false, step 1 reads "Read https://cetra.sh/llms.txt.":

     ```
     Add cetra tracing to this app.

     1. Read the cetra skill (.claude/skills/cetra or .agents/skills/cetra). If it is not there, read https://cetra.sh/llms.txt.
     2. Find the app's language and LLM libraries, and install the cetra SDK with the matching extras or instrumentations. Call init once at startup. The API key and URL are CETRA_API_KEY and CETRA_BASE_URL in <envFile>: load that file the way the app loads its settings. Never paste the key into code or chat.
     3. Run the app, or its tests, once so that it makes at least one LLM call. Flush before exit in short scripts.
     4. Check that traces arrived:
        npx @cetra-ai/cli query "<CHECK_QUERY>" --since 15m --json
        If it returns no rows, follow the troubleshooting steps of the skill.
     5. Tell me what you changed, and give me this link: <tracesUrl>
     ```

**Tests:**
- **`prompt-io.test.ts`:** `yes` for `""`, `Y`, `yes`, `n`, `no` and `maybe`; `ask` with a `PassThrough` input that answers `x\n` gives `x`, and an empty line gives the fallback.
- **`skill.test.ts`**, from the repository's `skills/cetra`:
  - an install into a temp root writes both targets and the marker;
  - a second install replaces them;
  - a directory without the marker is skipped;
  - `["claude"]` writes one target;
  - a source that does not exist gives two skips, not a throw;
  - `skillRoot` stops at a `.git` directory.
  - A separate test compares the built copy with the source: every file of `../dist/skills/cetra` equals the same file of `../../skills/cetra`. It runs only when `dist/skills/cetra/SKILL.md` exists (`it.skipIf(...)`, reason "needs pnpm build"). The package's `check` script builds before testing, so CI runs it.
- **`prompt.test.ts`:**
  - the exact text for given values, and the `skill: false` variant;
  - `CHECK_QUERY` appears verbatim in `../../skills/cetra/SKILL.md`.

**Done when:** `pnpm --dir node-cli exec vitest run src/prompt-io.test.ts src/skill.test.ts src/prompt.test.ts` passes; the type check passes; Biome is clean.

**Commit:** `Add the cetra-cli skill installer and agent prompt`

---

## C5b — The local server

**Goal:** start a local cetra (the binary, else Docker) and sign in to its first start with the generated password.

**Edit:** `node-cli/src/local.ts`, `local.test.ts` (new).

**Steps.**
1. **Contracts:**

   ```ts
   export const DEFAULT_ADMIN_EMAIL = "admin@cetra.local";   // the server requires a dotted domain
   export function isLoopback(url: URL): boolean;   // hostname localhost, 127.0.0.0/8, or "[::1]" (URL keeps the brackets)
   export async function isReady(server: string): Promise<boolean>; // GET /readyz, 2 s timeout, 200 → true
   export function localDataDir(env: NodeJS.ProcessEnv, platform = process.platform, home = os.homedir()): string;
   //   Linux: $XDG_DATA_HOME (absolute) else ~/.local/share; macOS: ~/Library/Application Support;
   //   Windows: %LOCALAPPDATA%; then "cetra/local";
   //   path.win32 / path.posix join by platform, as credentialsPath does
   export function isEmail(text: string): boolean;  // one "@"; local part 1-64 chars; domain with a "." and no empty label; no whitespace; at most 254 chars
   export function generatedPassword(): string;     // 24 chars from [A-Za-z0-9], crypto.randomInt
   export function configToml(dataDir: string, port: number): string;
   /** The server URL init uses for a local start: http://localhost:<port>. */
   export function localServer(url: URL): string;   // throws CliError("--url needs an explicit port for a local server", Exit.Usage) when url.port is ""
   export interface Started { how: "binary" | "docker"; firstStart: boolean; server: string; stop: string; start: string;
     email?: string; password?: string }
   export async function startLocal(url: URL, dir: string, options: { yes: boolean; io: Io; deps?: Deps }): Promise<Started>;
   export async function passwordSignIn(server: string, email: string, password: string, host?: string):
     Promise<{ token: string; key: string; org: string; project: string; projectId: string }>;
   ```

2. **The local URL.** `startLocal` uses `server = localServer(url)`, so `http://localhost:<port>`, whatever loopback spelling the user typed. The written `public_url` is that same string, and every session call sends it as `Origin`, which the server requires to equal `public_url`. `init` then uses `started.server` for `.env`, the link and the credentials.
3. **`configToml`** returns exactly the text below. The data dir is a TOML basic string: escape `\` and `"`.

   ```
   data_dir = "<dataDir>"
   public_url = "http://localhost:<port>"
   http_addr = "127.0.0.1:<port>"
   otlp_http_addr = "127.0.0.1:4318"
   grpc_addr = "127.0.0.1:4317"
   ```

   The file is written to `<data dir>/../local.toml`, next to the data dir, not inside it.
4. **First start.** `firstStart` is true when `<data dir>/meta.db` does not exist (`crates/server/src/meta/mod.rs` names it). The server creates the admin **before** it binds its ports, so a first start that then fails would leave a `meta.db` with an admin whose password nobody kept. Therefore, when a first start fails (spawn error, the 30 s wait, a `docker run` error), `startLocal` removes what it created before it throws: the binary path removes the data dir; the Docker path runs `docker rm -f cetra-local` and `docker volume rm cetra-local`. The next run is a first start again.
5. **`startLocal`:**
   1. **Questions.** Answers are read with `ask` and `yes`. With neither a terminal nor `--yes`, throw `CliError("no cetra server at <server>; pass --yes to start one", Exit.Unreachable)`.
      - The question is "No cetra server at <server>. Start one with `cetra serve`? [Y/n]", or "… with Docker? [Y/n]".
      - On a first start, the admin email: "Admin email for the local server [admin@cetra.local]:". `--yes` takes the default. An answer that fails `isEmail` throws `CliError("invalid email", Exit.Usage)` before anything starts.
   2. **The child's environment** on a first start: `CETRA_INIT_ADMIN_EMAIL`, `CETRA_INIT_ADMIN_PASSWORD` (`generatedPassword()`), `CETRA_INIT_ORG=local` and `CETRA_INIT_PROJECT` (the base name of `dir` cut to 100 characters, or `default` if empty). Nothing else from `CETRA_INIT_*`.
   3. **The binary** (`cetra version` runs through `deps.execFile ?? execFile`; every child process of this module goes through `deps.execFile` or `deps.spawn` when given):
      - `mkdir -p` the data dir and write `local.toml`;
      - open `<data dir>/server.log` with `fs.openSync(log, "a")`;
      - `spawn("cetra", ["serve", "--config", <local.toml>], { detached: true, stdio: ["ignore", fd, fd], env: {...process.env, ...initVars} })`, then `child.on("error", …)` and `unref()`;
      - write the PID to `<data dir>/server.pid`;
      - `stop` is `kill <pid>`, and `start` is `cetra serve --config <local.toml>`.
   4. **Else Docker** (`docker version` runs). Look for an existing container: `docker ps -a --filter name=^cetra-local$ --format {{.Names}}`.
      - **When there is one,** run `docker start cetra-local`, with `firstStart` false. The wait (step 6) uses the host port that `docker port cetra-local 8080` prints, which may differ from `--url`'s; when it differs, say so on stderr and use it.
      - **Otherwise,** `firstStart` is true when `docker volume inspect cetra-local` fails. Run `docker run -d --name cetra-local -p 127.0.0.1:<port>:8080 -p 127.0.0.1:4317:4317 -p 127.0.0.1:4318:4318 -v cetra-local:/data -e CETRA_PUBLIC_URL=http://localhost:<port>`, plus `-e <NAME>` for each init variable (the **name only**; the values go in the `execFile` `env`, so the password is not in the process list), then `ghcr.io/cetra-ai/cetra:<VERSION>`.
      - **Errors.** A failure whose stderr mentions `pull access denied`, `not found` or `unauthorized` throws `CliError("docker could not pull ghcr.io/cetra-ai/cetra:<VERSION> (the image is public from the first release); install the binary instead: curl -fsSL https://github.com/cetra-ai/cetra/releases/latest/download/install.sh | sh", Exit.Unreachable)`. Any other failure throws `CliError("docker could not start cetra: <first line of stderr>", Exit.Unreachable)`.
      - `stop` is `docker stop cetra-local`, and `start` is `docker start cetra-local`.
   5. **Neither:** throw `CliError("no cetra server at <server>, and neither cetra nor docker is installed. Install cetra: curl -fsSL https://github.com/cetra-ai/cetra/releases/latest/download/install.sh | sh", Exit.Unreachable)`.
   6. **Wait.** Poll `isReady` every 500 ms for up to 30 s. A timeout throws `CliError("cetra did not start; see <data dir>/server.log (another program may use port <port>, 4317 or 4318)", Exit.Unreachable)`, or for Docker "…; see docker logs cetra-local".
6. **`passwordSignIn`.** Every request carries `Origin: <server>`.
   1. `POST /api/auth/login` with `{email, password}`. Take `cetra_session` from `headers.getSetCookie()`. Failures:
      - 429 gives "the local server refused the sign-in for now; try again in a minute" (`Exit.SignIn`);
      - any other non-200 gives "could not sign in to the local server" (`Exit.SignIn`).
   2. `GET /api/auth/me` with the cookie. The org is the first with `demo` false; with none, throw "the local server has no organization" (`Exit.SignIn`).
   3. `GET /api/v1/orgs/<org>/projects?limit=1000`. The project is the first one; with none, throw "the local server has no project" (`Exit.SignIn`).
   4. `POST /api/v1/me/tokens` with `{name: "CLI init on <host>" (or "CLI init"), scopes: ["read"], expires_at: <now + 90 days, ISO 8601>}`. Expect 201 and take `secret`.
   5. `POST /api/v1/projects/<encodeURIComponent(org + "/" + project)>/api-keys` with `{kind: "secret", name: <same>, scopes: ["ingest"]}`. Expect 201 and take `secret`.
   6. `POST /api/auth/logout`; its failure is ignored.
   7. Check both secrets with `isPersonalToken` and `isSecretKey`; otherwise throw "the server returned an invalid grant".

**Tests** (`local.test.ts`):
- `isLoopback` on `localhost`, `127.0.0.2`, `[::1]` and `example.com`; `localServer` for `http://127.0.0.1:8080`, `http://[::1]:9000` and `http://localhost` (refused);
- `localDataDir` for linux, darwin and win32, with exact strings;
- `isEmail` on `admin@cetra.local`, `admin@localhost`, `a b@c.d`, `a@b.` and `a@.b` (refused) and `x@y.z`;
- `generatedPassword` length and charset;
- `configToml` exact, including a path with `"` and `\`;
- `passwordSignIn` against a canned server: the requests carry `Origin` and the cookie, the token request body (scope `read`, an `expires_at` 90 days ahead), the key request path (`acme%2Fdemo`) and body, a 429 login message, and a no-project message;
- `startLocal` with `deps.execFile` and `deps.spawn` injected:
  - neither `cetra` nor `docker` is found: the "neither" error and exit 3;
  - no terminal and no `--yes`: the "pass --yes" error;
  - a binary first start whose fake server never becomes ready (with a short wait injected through `deps.sleep`/`deps.now`) removes the data dir before it throws.

  The real binary and Docker are not run in unit tests.

**Done when:** `pnpm --dir node-cli exec vitest run src/local.test.ts` passes; the type check passes; Biome is clean.

**Commit:** `Add the cetra-cli local server start`

---

## C5c — `init`

**Goal:** `cetra-cli init` as design § `init`, step by step.

**Edit:** `node-cli/src/init.ts`, `init.test.ts` (new); `src/cli.ts`.

**Steps.**
1. **`init.ts`:** `export async function init(args: string[], io: Io, deps: Deps = {}): Promise<ExitCode>`.
   - **Options** (`parseOptions`, no positionals): `--url`, `--project <org/project>`, `--dir <path>` (default `io.cwd`; it must exist, else `Exit.Usage`), `--env-file <path>` (relative to `--dir`; default `<dir>/.env`), `--agents <list>` (comma list of `claude`, `agents`; an unknown name gives `Exit.Usage`; default both), `--no-skill`, `--no-browser`, `--yes`, `--json`, `--help`. `--agents` together with `--no-skill` is `Exit.Usage`.
2. **Steps,** in this order:
   1. **The server.** `--url`, else `CETRA_URL`, else the link found from `--dir` (`findLink(dir)`), else `https://app.cetra.sh`, through `serverUrl`. Check here that the server URL is a valid `.env` value (the C2 pattern); otherwise `Exit.Usage`, before any sign-in.
   2. **Not ready.** If `isReady(server)` is false: on a loopback URL, `started = await startLocal(…)` and `server = started.server`; otherwise throw `CliError("no cetra server at <server>", Exit.Unreachable)`.
   3. **Sign in.**
      - On a local **first** start, `passwordSignIn(server, started.email, started.password, localHost())` gives the token, key and project, and `grantServer = server`.
      - Otherwise `deviceSignIn({ server, purpose: "init", project: --project, noBrowser, deps })` gives `grant`, and `grantServer = grant.server`. The device flow's org and project come from `grant.project`.
      - On the local password path `--project` is ignored (the server has one project); say so in `--help`.
   4. **Credentials.** `saveToken(credentialsPath(io.env), server, token)`. It replaces a token saved for that server, including a `read, write` one from `login`; the docs page says so.
   5. **The env file.**
      - Read it if present.
      - When it has a `CETRA_API_KEY` and its `CETRA_BASE_URL` equals `grantServer`, ask "Replace the CETRA_API_KEY in <base name>? [y/N]". `--yes` replaces it. Without a terminal and without `--yes`, keep the old key and print `kept the existing CETRA_API_KEY in <base name>` on stderr.
      - `setVariables`, then `writeEnvFile`. A failure throws `CliError("could not write <base name>", Exit.Write)`.
      - `ignoreInGit(dir, envPath)`; print `Added <relative path> to .gitignore.` when it returns true.
   6. **The link.** `writeLink(dir, { server, org, project, project_id })`. A failure throws `CliError("could not write .cetra/project.json", Exit.Write)`.
   7. **The skill.** Unless `--no-skill`, `installSkill(deps.skillSource ?? bundledSkill(), await skillRoot(dir), targets, VERSION)`. Print `Installed the cetra skill in <path>` for each install, and `warning: <path>: <reason>` for each skip.
   8. **The output.**
      - `tracesUrl` is `<grantServer>/<org>/<project>/traces`.
      - On stderr: a blank line, `Paste this prompt into your coding agent:`, a blank line, the prompt (`agentPrompt`), a blank line, and `Traces: <tracesUrl>`.
      - When init started a server, add `Stop the server: <stop>` and `Start it again: <start>`.
      - On a first start, add `UI: <server>` and `Admin: <email>`. Add `Password: <password>` only when stderr is a terminal; otherwise print `The admin password is in no file; reset it with cetra admin reset-password <email>`, so that an agent's transcript does not keep it.
      - With `--json`, stdout gets exactly one object: `{server: grantServer, project: "<org>/<project>", env_file, link_file, skills: [<absolute paths>], prompt, traces_url}`. It holds no token, key or password.
3. Add `init` to `COMMANDS` in `cli.ts`.

**Tests** (`init.test.ts`): a canned server whose answers come in this order: `GET /readyz` 200, the device code, `authorization_pending`, the grant, then nothing else. Use `XDG_CONFIG_HOME` and `--dir` in temp directories, `deps = { sleep: async () => {}, skillSource: <repo skills/cetra> }` and `--no-browser`.
- **The device path** with `--yes --json`:
  - the stdout object;
  - `.env` content, with mode 0600;
  - the link file;
  - the credentials;
  - both skill targets with the marker;
  - neither the token nor the key appears in stdout or stderr.
- **An existing `.env`** with other lines and a `CETRA_API_KEY` for the same server: kept lines, and the key replaced with `--yes`. Without `--yes` and without a terminal, it is kept and the message is printed.
- **In a temp git repo:** `.env` is added to `.gitignore`.
- **Errors:**
  - an unreachable non-loopback URL, `http://192.0.2.1:9` (a documentation address that never answers; `isReady` gives up after 2 s), gives exit 3;
  - `access_denied` gives exit 4;
  - an env file path that is a directory gives exit 5;
  - `--agents claude --no-skill` gives exit 2, and so does `--agents frob`.

**Done when:** `pnpm --dir node-cli exec vitest run src/init.test.ts` passes; the type check passes; Biome is clean.

**Commit:** `Add cetra-cli init`

---

## C6a — End-to-end test

**Goal:** `just e2e` proves `init` → span → `query` against a real `cetra serve`.

**Edit:** `node-cli/src/e2e.test.ts` (new), `tools/e2e/run.sh`.

**Steps.**
1. **`tools/e2e/run.sh`.** Add `CETRA_PUBLIC_URL="$base"` to the server's environment. `$base` is already computed before the start. After the TypeScript SDK suite, add:

   ```bash
   export CETRA_E2E_ADMIN_EMAIL=ada@example.com CETRA_E2E_ADMIN_PASSWORD=cetra-e2e-1
   pnpm --dir node-cli run build
   pnpm --dir node-cli exec vitest run src/e2e.test.ts
   ```

2. **`src/e2e.test.ts`** runs with `describe.skipIf(!process.env.CETRA_E2E_BASE_URL)`. The test has `{ timeout: 90_000 }`, because the server polls every 5 s.
   1. Temp `dir` and temp `XDG_CONFIG_HOME`. `env = { ...process.env, XDG_CONFIG_HOME }`.
   2. The bin is `fileURLToPath(new URL("../dist/cli.js", import.meta.url))`. Spawn `process.execPath` with `[<that path>, "init", "--url", BASE, "--dir", dir, "--yes", "--no-browser", "--json", "--project", "acme/e2e"]` and that `env`. Collect stdout. Read stderr line by line until a line matches `^Code: ([A-Z]{4}-[A-Z]{4})$`.
   3. **Approve as the admin:**
      - `POST {BASE}/api/auth/login` with the admin email and password and `Origin: BASE`, keeping `cetra_session`;
      - `GET {BASE}/api/v1/orgs/acme/projects`: the answer is `{data: [{id, slug, …}], next_cursor}`; take the `id` of the entry whose `slug` is `e2e`;
      - `POST {BASE}/api/auth/device/<code>/approve` with `{project_id}`, the cookie and `Origin: BASE`. Expect 204.
   4. Wait for the child to exit with code 0. Parse its stdout as JSON; `project` is `acme/e2e`.
   5. Read `CETRA_API_KEY` from `<dir>/.env`. Send one span:
      - `POST {BASE}/v1/traces` with `Content-Type: application/json` and `Authorization: Bearer <key>`;
      - the body is `{"resourceSpans":[{"scopeSpans":[{"spans":[{"traceId":<32 random hex>,"spanId":<16 random hex>,"name":"cli-e2e","startTimeUnixNano":<now in ns as a string>,"endTimeUnixNano":<now + 1 ms in ns as a string>}]}]}]}`;
      - build the nanosecond strings with `BigInt(Date.now()) * 1_000_000n`;
      - expect a 2xx.
   6. Poll, every 1 s for at most 30 s: spawn `process.execPath` with `[cli, "query", "SELECT trace_id FROM traces WHERE name = 'cli-e2e'", "--since", "1h", "--json"]`, `cwd: dir` and the same `env`, until its stdout JSON has a row with the sent `traceId`.
   7. Assert that `<dir>/.claude/skills/cetra/SKILL.md` and `<dir>/.agents/skills/cetra/SKILL.md` exist. `dir` is not in a git repo, so the root is `dir`.

**Done when:** the file is written and committed. It runs in `just e2e` in the final check. Run the type check; it passes.

**Commit:** `Test cetra-cli init end to end`

---

## C6b — Docs and checks

**Goal:** the docs page and the reference page; the checks know the new CLI.

**Edit:**
- `website/src/content/docs/docs/get-started/coding-agent.mdx` (new)
- `website/src/content/docs/docs/reference/cetra-cli.md` (new)
- `website/src/content/docs/docs/get-started/quickstart.mdx`
- `website/src/content/docs/docs/reference/index.mdx`
- `website/astro.config.mjs`
- `website/scripts/check-skill.mjs`, `check-skill.test.mjs`
- `website/scripts/check-env-vars.mjs`, `check-env-vars.test.mjs`
- `CHANGELOG.md`, `AGENTS.md`, `docs/roadmap.md`, `docs/spec/09-api.md`

**Steps.**
1. **`coding-agent.mdx`** (Get started), in STYLE.md's voice. Title "Set up with your coding agent", description "Set an app up for cetra with one command, then give your coding agent the prompt it prints." (under 160 characters, as `check-terms` requires). Sections:
   - **The one-liner** (`npx @cetra-ai/cli@latest init`), and what it asks in the browser.
   - **What it changes:**
     - `.env`;
     - `.gitignore`;
     - `.cetra/project.json`;
     - the credentials file, where its read-only token replaces a token from `login` for the same server;
     - the skill directories.
   - **The prompt**, in a `text` fence.
   - **Self-hosted:** `--url http://localhost:8080` (the default is the cloud), and the local server it can start. Mention the admin password, and that Docker keeps the init variables in the container's configuration.
   - **Without the CLI:** `npx skills add cetra-ai/skills`, setting `CETRA_API_KEY` and `CETRA_BASE_URL` yourself, and the same prompt.
   - **Check it yourself** (`npx @cetra-ai/cli query`).
   - **Tracing the coding agent itself is different:** one sentence and a link to `/docs/integrations/claude-code/`.

   Only `bash` and `text` fences.
2. **`reference/cetra-cli.md`.** Title "cetra-cli", description "Every cetra-cli command and option: init, query and login." Headings: ``### `cetra-cli init` ``, ``### `cetra-cli query` ``, ``### `cetra-cli login` ``. It has:
   - their options, from C3–C5c;
   - the exit-code table;
   - the files `init` writes;
   - the environment variables (`CETRA_URL`, `CETRA_TOKEN` (only with `--url` or `CETRA_URL`), `CETRA_PROJECT`, `XDG_CONFIG_HOME`).
3. **Sidebar** (`astro.config.mjs`): `{ label: "Set up with your coding agent", link: "/docs/get-started/coding-agent/" }` first in Get started; `{ label: "cetra-cli", link: "/docs/reference/cetra-cli/" }` after `CLI` in Reference. **`reference/index.mdx`:** a `LinkCard` for cetra-cli after the CLI card, written in the same multi-line form as its neighbours.
4. **`quickstart.mdx`.** Before `## Run cetra`, add an `<Aside>`: "Working with a coding agent? `npx @cetra-ai/cli@latest init` sets everything up and gives you a prompt for it." plus a link to `/docs/get-started/coding-agent/`. The design's § Docs for agents already says "a short pointer above its tabs".
5. **`check-skill.mjs`:**
   - `export const NODE_CLI_REFERENCE = "src/content/docs/docs/reference/cetra-cli.md";` and `export function nodeCliCommands(text)`, which reads the ``### `cetra-cli <name>` `` headings.
   - In `checkSkill`, on the same code texts as the `cetra` rule, match `/(?:^|[\s`("'])(?:npx @cetra-ai\/cli(?:@[\w.-]+)?|cetra-cli) ([a-z][a-z-]*)/g`. A name not in `ctx.nodeCommands` gives ``unknown command `cetra-cli <name>` ``.
   - `realContext()` adds `nodeCommands`. `main()` fails when it is empty, as it does for `commands`.
   - **Tests:** `npx @cetra-ai/cli@latest init` passes; `npx @cetra-ai/cli frob` is reported; `nodeCliCommands` on the real page gives `init, query, login`.
6. **`check-env-vars.mjs`.** Add `"node-cli/src"` to the scanned code directories. Test it through `checkSite` with a code string, as the existing cases do: `main(root)` reads fixed directories.
7. **`CHANGELOG.md` `[Unreleased]` `### Added`:** "`npx @cetra-ai/cli init` sets an app up for cetra in one command and prints a prompt for your coding agent; `cetra-cli query` checks the traces from the terminal; the cetra agent skill is installed for Claude Code, Codex, Cursor, Gemini CLI and Copilot."
8. **`AGENTS.md`:**
   - Layout: `node-cli/                     the cetra-cli package (npx @cetra-ai/cli): init, query, login (D-AO-2)` after `sdks/typescript/`.
   - Commands: `| \`just check-cli-js\` | cetra-cli: lint, types, build, tests, package lint |` and `| \`just cli-smoke <tarball>\` | install a packed cetra-cli and run --version |`.
   - In the `just check` row, list cetra-cli.
9. **Spec 09 § CLI.** One paragraph after the table: "`npx @cetra-ai/cli` (`cetra-cli`, package `node-cli/`) is the setup CLI: `init`, `query`, `login` (D-AO-2). Its reference is the site page `reference/cetra-cli`."
10. **`docs/roadmap.md`.** The AO row's Status adds `M3 implemented (\`@cetra-ai/cli\`)`.

**Done when:**
- `cd website && node --test scripts/check-skill.test.mjs scripts/check-env-vars.test.mjs` passes;
- `cd website && node scripts/check-skill.mjs && node scripts/check-env-vars.mjs && node scripts/check-terms.mjs` exits 0;
- `cd website && ../node_modules/.bin/biome check --write scripts/` is clean.

**Commit:** `Document cetra-cli`

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
   - **§ Versions, changelog, publication:** list the two files.
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
