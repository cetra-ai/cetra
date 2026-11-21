import { type ChildProcess, execFile, spawn } from "node:child_process";
import { randomInt } from "node:crypto";
import { closeSync, openSync } from "node:fs";
import { access, chmod, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { basename, dirname, join, posix, win32 } from "node:path";
import { INVALID_GRANT, isPersonalToken, isSecretKey, isSlug, isUuid } from "./device.js";
import { CliError, type Deps, Exit, type Io } from "./errors.js";
import { problemMessage, requestJson } from "./http.js";
import { yes as affirmative, ask, interactive } from "./prompt-io.js";
import { VERSION } from "./version.js";

/** The local server `init` starts when none answers (D-AO-11): `cetra serve` if it is on PATH,
 *  else Docker, else install steps. The server's first start creates the admin from the
 *  `CETRA_INIT_*` variables; the generated password is used once, by `passwordSignIn`. */

export const DEFAULT_ADMIN_EMAIL = "admin@cetra.local"; // the server requires a dotted domain

const CONTAINER = "cetra-local";
const IMAGE = `ghcr.io/cetra-ai/cetra:${VERSION}`;
const READY_TIMEOUT_MS = 30_000;
const READY_POLL_MS = 500;
const READY_CHECK_MS = 2_000;
const TOKEN_LIFETIME_MS = 90 * 24 * 60 * 60 * 1000;
const PASSWORD_LENGTH = 24;
const PASSWORD_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const INSTALL_HINT =
  "Install cetra: curl -fsSL https://github.com/cetra-ai/cetra/releases/latest/download/install.sh | sh";
const DOCKER_NOT_STARTED = `cetra did not start; see docker logs ${CONTAINER}`;

/** Whether the URL points at this machine: localhost, 127.0.0.0/8 or [::1] (URL keeps the brackets). */
export function isLoopback(url: URL): boolean {
  const host = url.hostname;
  return host === "localhost" || /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(host) || host === "[::1]";
}

/** The server URL a local start uses: `http://localhost:<port>`, whatever loopback spelling was typed.
 *  The server only speaks plain http, and a port of 80 (or none, which is 80) reads as the default
 *  port, so both are refused. */
export function localServer(url: URL): string {
  if (url.protocol !== "http:") {
    throw new CliError("a local server is started over http; use an http:// --url", Exit.Usage);
  }
  if (url.port === "") {
    throw new CliError("--url needs an explicit port other than 80 for a local server", Exit.Usage);
  }
  return `http://localhost:${url.port}`;
}

/** Whether the server answers `GET /readyz` with 200 within two seconds. Never throws. */
export async function isReady(server: string): Promise<boolean> {
  try {
    const response = await fetch(`${server}/readyz`, {
      redirect: "manual",
      signal: AbortSignal.timeout(READY_CHECK_MS),
    });
    const ready = response.status === 200;
    await response.text();
    return ready;
  } catch {
    return false;
  }
}

/** The local data directory of the server, as the platform's data dir with `cetra/local` added.
 *  Linux: `$XDG_DATA_HOME` when absolute, else `~/.local/share`; macOS: `~/Library/Application
 *  Support`; Windows: `%LOCALAPPDATA%`. `platform` and `home` are parameters so tests can check
 *  every platform. */
export function localDataDir(
  env: NodeJS.ProcessEnv,
  platform: NodeJS.Platform = process.platform,
  home: string = homedir(),
): string {
  if (platform === "win32") {
    const local = env.LOCALAPPDATA;
    if (local === undefined || local === "") {
      throw new CliError("could not find the data directory", Exit.Error);
    }
    return win32.join(local, "cetra", "local");
  }
  if (platform === "darwin") {
    return posix.join(home, "Library", "Application Support", "cetra", "local");
  }
  const xdg = env.XDG_DATA_HOME;
  const base =
    xdg !== undefined && posix.isAbsolute(xdg) ? xdg : posix.join(home, ".local", "share");
  return posix.join(base, "cetra", "local");
}

/** Whether the text is an email the server accepts: one `@`, a local part of 1 to 64 characters,
 *  a domain with a `.` and no empty label, no whitespace, at most 254 characters. */
export function isEmail(text: string): boolean {
  if (text.length > 254 || /\s/.test(text)) return false;
  const parts = text.split("@");
  if (parts.length !== 2) return false;
  const local = parts[0] ?? "";
  const domain = parts[1] ?? "";
  if (local.length < 1 || local.length > 64) return false;
  if (!domain.includes(".")) return false;
  return domain.split(".").every((label) => label !== "");
}

/** A random password of 24 letters and digits, from `crypto.randomInt`. */
export function generatedPassword(): string {
  let password = "";
  for (let i = 0; i < PASSWORD_LENGTH; i += 1) {
    password += PASSWORD_ALPHABET.charAt(randomInt(PASSWORD_ALPHABET.length));
  }
  return password;
}

/** The `cetra serve` config of a local start. The data dir is a TOML basic string: `\` and `"`
 *  are escaped. Every listener is on 127.0.0.1. */
export function configToml(dataDir: string, port: number): string {
  const escaped = dataDir.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
  return [
    `data_dir = "${escaped}"`,
    `public_url = "http://localhost:${port}"`,
    `http_addr = "127.0.0.1:${port}"`,
    'otlp_http_addr = "127.0.0.1:4318"',
    'grpc_addr = "127.0.0.1:4317"',
    "",
  ].join("\n");
}

/** What `startLocal` started. `email` and `password` are set on a first start only; the password
 *  is a secret: never print it. */
export interface Started {
  firstStart: boolean;
  server: string;
  stop: string;
  start: string;
  email?: string;
  password?: string;
}

/** Starts the local server when none answers at `url`, and waits until it is ready (30 s). A
 *  first start that fails removes what it created, so the next run is a first start again. */
export async function startLocal(
  url: URL,
  dir: string,
  options: { yes: boolean; io: Io; deps?: Deps },
): Promise<Started> {
  const deps = options.deps ?? {};
  const server = localServer(url);
  const port = Number(url.port);

  const binary = (await run(deps, "cetra", ["version"])).ok;
  const docker = binary ? false : (await run(deps, "docker", ["version"])).ok;
  if (!binary && !docker) {
    throw new CliError(
      `no cetra server at ${server}, and neither cetra nor docker is installed. ${INSTALL_HINT}`,
      Exit.Unreachable,
    );
  }
  if (!options.yes && !interactive()) {
    throw new CliError(`no cetra server at ${server}; pass --yes to start one`, Exit.Unreachable);
  }
  if (!options.yes) {
    const how = binary ? "`cetra serve`" : "Docker";
    const answer = await ask(`No cetra server at ${server}. Start one with ${how}? [Y/n] `, "");
    if (!affirmative(answer, true)) {
      throw new CliError(`no cetra server at ${server}`, Exit.Unreachable);
    }
  }

  if (binary) return startBinary({ server, port, dir, options, deps });
  return startDocker({ server, port, dir, options, deps });
}

interface StartContext {
  server: string;
  port: number;
  dir: string;
  options: { yes: boolean; io: Io; deps?: Deps };
  deps: Deps;
}

/** The `cetra serve` path: a detached child with its output in `server.log`. */
async function startBinary(context: StartContext): Promise<Started> {
  const { server, port, dir, options, deps } = context;
  const dataDir = localDataDir(options.io.env);
  const firstStart = !(await exists(join(dataDir, "meta.db")));
  const admin = firstStart ? await adminAnswers(options.yes, dir) : undefined;
  const toml = join(dirname(dataDir), "local.toml");
  const log = join(dataDir, "server.log");

  let child: ChildProcess | undefined;
  try {
    // The data dir holds the database and every trace payload: owner only, also when it exists.
    await mkdir(dataDir, { recursive: true, mode: 0o700 });
    if (process.platform !== "win32") await chmod(dataDir, 0o700);
    // The config is written on a first start, and on a later start only when its port changed.
    if (firstStart || !(await configKeepsPort(toml, port))) {
      await writeFile(toml, configToml(dataDir, port), "utf8");
    }
    const fd = openSync(log, "a", 0o600);
    let spawned: ChildProcess;
    try {
      spawned = (deps.spawn ?? spawn)("cetra", ["serve", "--config", toml], {
        detached: true,
        stdio: ["ignore", fd, fd],
        env: childEnvironment(admin?.variables),
      });
    } finally {
      closeSync(fd);
    }
    child = spawned;
    let spawnFailed = false;
    spawned.on("error", () => {
      spawnFailed = true;
    });
    spawned.unref();
    if (spawned.pid !== undefined) await writeFile(join(dataDir, "server.pid"), `${spawned.pid}\n`);

    await waitReady(
      server,
      deps,
      () => spawnFailed,
      `cetra did not start; see ${log} (another program may use port ${port}, 4317 or 4318)`,
    );
    const pid = spawned.pid;
    if (pid === undefined) {
      throw new CliError(`cetra did not start; see ${log}`, Exit.Unreachable);
    }
    return {
      firstStart,
      server,
      stop: `kill ${pid}`,
      start: `cetra serve --config ${toml}`,
      ...admin?.started,
    };
  } catch (error) {
    if (firstStart) {
      killChild(child);
      await rm(dataDir, { recursive: true, force: true });
    }
    throw error;
  }
}

/** Whether the config file exists and listens on `port`. A missing or unreadable file is no. */
async function configKeepsPort(path: string, port: number): Promise<boolean> {
  try {
    const lines = (await readFile(path, "utf8")).split("\n");
    return lines.includes(`http_addr = "127.0.0.1:${port}"`);
  } catch {
    return false;
  }
}

/** Kills a first start's child before its data dir is removed. A child that has already exited
 *  makes `process.kill` throw, which is ignored. */
function killChild(child: ChildProcess | undefined): void {
  if (child?.pid === undefined) return;
  try {
    process.kill(child.pid);
  } catch {
    // The child has already exited.
  }
}

/** The Docker path: the existing `cetra-local` container, else a new one on the `cetra-local`
 *  volume. The init values reach the container by name only (`-e NAME`); their values go in the
 *  environment of the `docker` process, so they are not in the process list. */
async function startDocker(context: StartContext): Promise<Started> {
  const { server, port, dir, options, deps } = context;
  const listing = await run(deps, "docker", [
    "ps",
    "-a",
    "--filter",
    `name=^${CONTAINER}$`,
    "--format",
    "{{.Names}}",
  ]);
  const existing = listing.stdout.split("\n").some((line) => line.trim() === CONTAINER);

  if (existing) {
    const started = await run(deps, "docker", ["start", CONTAINER]);
    if (!started.ok) {
      throw new CliError(
        `docker could not start cetra: ${firstLine(started.stderr)}`,
        Exit.Unreachable,
      );
    }
    const hostPort = await publishedPort(deps, port);
    if (hostPort !== port) {
      // The container's public_url is fixed at its creation, so the URL the user passes must change.
      throw new CliError(
        `the cetra-local container listens on port ${hostPort}; run again with --url http://localhost:${hostPort}`,
        Exit.Usage,
      );
    }
    await waitReady(server, deps, () => false, DOCKER_NOT_STARTED);
    return {
      firstStart: false,
      server,
      stop: `docker stop ${CONTAINER}`,
      start: `docker start ${CONTAINER}`,
    };
  }

  const volume = await run(deps, "docker", ["volume", "inspect", CONTAINER]);
  const firstStart = !volume.ok;
  const admin = firstStart ? await adminAnswers(options.yes, dir) : undefined;
  const names = admin === undefined ? [] : Object.keys(admin.variables);
  const args = [
    "run",
    "-d",
    "--name",
    CONTAINER,
    "-p",
    `127.0.0.1:${port}:8080`,
    "-p",
    "127.0.0.1:4317:4317",
    "-p",
    "127.0.0.1:4318:4318",
    "-v",
    `${CONTAINER}:/data`,
    "-e",
    `CETRA_PUBLIC_URL=${server}`,
  ];
  for (const name of names) args.push("-e", name);
  args.push(IMAGE);

  const ran = await run(deps, "docker", args, childEnvironment(admin?.variables));
  if (!ran.ok) {
    if (firstStart) await cleanupContainer(deps);
    if (/pull access denied|not found|unauthorized/i.test(ran.stderr)) {
      throw new CliError(
        `docker could not pull ${IMAGE} (the image is public from the first release); install the binary instead: curl -fsSL https://github.com/cetra-ai/cetra/releases/latest/download/install.sh | sh`,
        Exit.Unreachable,
      );
    }
    throw new CliError(`docker could not start cetra: ${firstLine(ran.stderr)}`, Exit.Unreachable);
  }

  try {
    await waitReady(server, deps, () => false, DOCKER_NOT_STARTED);
  } catch (error) {
    if (firstStart) await cleanupContainer(deps);
    throw error;
  }
  return {
    firstStart,
    server,
    stop: `docker stop ${CONTAINER}`,
    start: `docker start ${CONTAINER}`,
    ...admin?.started,
  };
}

/** The first-start answers: the admin email (asked unless `--yes`, which takes the default) and a
 *  generated password, with the `CETRA_INIT_*` values the child needs. */
async function adminAnswers(
  yes: boolean,
  dir: string,
): Promise<{
  variables: Record<string, string>;
  started: { email: string; password: string };
}> {
  const answer = yes
    ? DEFAULT_ADMIN_EMAIL
    : await ask(`Admin email for the local server [${DEFAULT_ADMIN_EMAIL}]: `, DEFAULT_ADMIN_EMAIL);
  if (!isEmail(answer)) throw new CliError("invalid email", Exit.Usage);
  const password = generatedPassword();
  const name = basename(dir).slice(0, 100);
  return {
    variables: {
      CETRA_INIT_ADMIN_EMAIL: answer,
      CETRA_INIT_ADMIN_PASSWORD: password,
      CETRA_INIT_ORG: "local",
      CETRA_INIT_PROJECT: name === "" ? "default" : name,
    },
    started: { email: answer, password },
  };
}

/** The child's environment: the parent's, without any inherited `CETRA_INIT_*` name, plus the
 *  first-start values when there are some. */
function childEnvironment(variables: Record<string, string> | undefined): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env };
  for (const name of Object.keys(env)) {
    if (name.startsWith("CETRA_INIT_")) delete env[name];
  }
  return { ...env, ...variables };
}

/** Polls `isReady` every 500 ms for up to 30 s. `failed` ends the wait at once (a spawn error);
 *  both ends throw `message`. */
async function waitReady(
  server: string,
  deps: Deps,
  failed: () => boolean,
  message: string,
): Promise<void> {
  const sleep = deps.sleep ?? defaultSleep;
  const now = deps.now ?? Date.now;
  const started = now();
  for (;;) {
    if (failed()) throw new CliError(message, Exit.Unreachable);
    if (await isReady(server)) return;
    if (now() - started >= READY_TIMEOUT_MS) throw new CliError(message, Exit.Unreachable);
    await sleep(READY_POLL_MS);
  }
}

/** The host port Docker publishes for the container's 8080, from `docker port`; `fallback` when
 *  the answer has no port. */
async function publishedPort(deps: Deps, fallback: number): Promise<number> {
  const answer = await run(deps, "docker", ["port", CONTAINER, "8080"]);
  const match = /:(\d+)$/.exec(firstLine(answer.stdout));
  if (match === null || match[1] === undefined) return fallback;
  return Number(match[1]);
}

/** Removes a first start's container and volume. Errors are ignored: the start has already failed. */
async function cleanupContainer(deps: Deps): Promise<void> {
  await run(deps, "docker", ["rm", "-f", CONTAINER]);
  await run(deps, "docker", ["volume", "rm", CONTAINER]);
}

/** Signs in to a local server with its password and creates the two secrets `init` writes: a
 *  `read` personal token (90 days) and an `ingest` secret key on the first project of the first
 *  organization. Every request carries `Origin: <server>`, which the server requires. */
export async function passwordSignIn(
  server: string,
  email: string,
  password: string,
  host?: string,
): Promise<{ token: string; key: string; org: string; project: string; projectId: string }> {
  const signIn = "could not sign in to the local server";
  const login = await requestJson("POST", `${server}/api/auth/login`, {
    body: { email, password },
    origin: server,
  });
  if (login.status === 429) {
    throw new CliError(
      "the local server refused the sign-in for now; try again in a minute",
      Exit.SignIn,
    );
  }
  if (login.status !== 200) throw new CliError(signIn, Exit.SignIn);
  const cookie = sessionCookie(login.headers);
  if (cookie === undefined) throw new CliError(signIn, Exit.SignIn);

  const me = await requestJson("GET", `${server}/api/auth/me`, { cookie, origin: server });
  if (me.status !== 200) throw new CliError(signIn, Exit.SignIn);
  const org = firstOrganization(me.body);
  if (org === undefined) throw new CliError("the local server has no organization", Exit.SignIn);

  const projects = await requestJson(
    "GET",
    `${server}/api/v1/orgs/${encodeURIComponent(org)}/projects?limit=1000`,
    { cookie, origin: server },
  );
  if (projects.status !== 200) throw new CliError(signIn, Exit.SignIn);
  const project = firstProject(projects.body);
  if (project === undefined) throw new CliError("the local server has no project", Exit.SignIn);

  const name = host === undefined ? "CLI init" : `CLI init on ${host}`;
  const expires = new Date(Date.now() + TOKEN_LIFETIME_MS).toISOString();
  const token = await requestJson("POST", `${server}/api/v1/me/tokens`, {
    body: { name, scopes: ["read"], expires_at: expires },
    cookie,
    origin: server,
  });
  if (token.status !== 201) {
    throw new CliError(problemMessage(token.status, token.body, server), Exit.SignIn);
  }

  const key = await requestJson(
    "POST",
    `${server}/api/v1/projects/${encodeURIComponent(`${org}/${project.slug}`)}/api-keys`,
    {
      body: { kind: "secret", name, scopes: ["ingest"] },
      cookie,
      origin: server,
    },
  );
  if (key.status !== 201) {
    throw new CliError(problemMessage(key.status, key.body, server), Exit.SignIn);
  }

  // The session is no longer needed; a failed logout does not matter.
  await requestJson("POST", `${server}/api/auth/logout`, { cookie, origin: server }).catch(
    () => undefined,
  );

  const tokenSecret = secretOf(token.body);
  const keySecret = secretOf(key.body);
  if (
    tokenSecret === undefined ||
    keySecret === undefined ||
    !isPersonalToken(tokenSecret) ||
    !isSecretKey(keySecret)
  ) {
    throw new CliError("the server returned an invalid grant", Exit.SignIn);
  }
  return {
    token: tokenSecret,
    key: keySecret,
    org,
    project: project.slug,
    projectId: project.id,
  };
}

/** The `cetra_session=<value>` pair of the login answer, as a `Cookie` header. */
function sessionCookie(headers: Headers): string | undefined {
  for (const line of headers.getSetCookie()) {
    const pair = (line.split(";")[0] ?? "").trim();
    if (pair.startsWith("cetra_session=") && pair.length > "cetra_session=".length) return pair;
  }
  return undefined;
}

/** The slug of the first organization that is not the demo one. A slug the server's rule refuses
 *  is an invalid grant: the slug is written into the prompt and the env file. */
function firstOrganization(body: unknown): string | undefined {
  if (!isRecord(body) || !Array.isArray(body.orgs)) return undefined;
  for (const org of body.orgs) {
    if (isRecord(org) && org.demo === false && typeof org.slug === "string") {
      if (!isSlug(org.slug)) throw new CliError(INVALID_GRANT, Exit.SignIn);
      return org.slug;
    }
  }
  return undefined;
}

/** The id and slug of the first project of a project page; undefined when the page is empty. The
 *  id must be a UUID and the slug must follow the server's rule, as for the device grant. */
function firstProject(body: unknown): { id: string; slug: string } | undefined {
  if (!isRecord(body) || !Array.isArray(body.data)) return undefined;
  const first: unknown = body.data[0];
  if (first === undefined) return undefined;
  if (
    !isRecord(first) ||
    typeof first.id !== "string" ||
    !isUuid(first.id) ||
    typeof first.slug !== "string" ||
    !isSlug(first.slug)
  ) {
    throw new CliError(INVALID_GRANT, Exit.SignIn);
  }
  return { id: first.id, slug: first.slug };
}

function secretOf(body: unknown): string | undefined {
  if (!isRecord(body) || typeof body.secret !== "string") return undefined;
  return body.secret;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** The first line of a command's output, trimmed. */
function firstLine(text: string): string {
  return (text.split("\n")[0] ?? "").trim();
}

interface RunResult {
  ok: boolean;
  stdout: string;
  stderr: string;
}

/** Runs a command without a shell, through `deps.execFile` when given. `ok` is false for a missing
 *  program and for a non-zero exit. */
function run(
  deps: Deps,
  file: string,
  args: string[],
  env: NodeJS.ProcessEnv = process.env,
): Promise<RunResult> {
  const exec = deps.execFile ?? execFile;
  return new Promise((resolve) => {
    exec(file, args, { env, encoding: "utf8" }, (error, stdout, stderr) => {
      resolve({ ok: error === null, stdout: String(stdout), stderr: String(stderr) });
    });
  });
}

async function exists(path: string): Promise<boolean> {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}

const defaultSleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));
