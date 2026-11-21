import { openBrowser } from "./browser.js";
import { checkServerUrl, serverUrl } from "./credentials.js";
import { setVariables } from "./envfile.js";
import { CliError, CONTROL, type Deps, Exit } from "./errors.js";
import { localHost } from "./host.js";
import { type JsonAnswer, requestJson } from "./http.js";

/** The device flow of `cetra-cli login` and `init`, as the Rust `login.rs` does it (D-AO-3, spec 08
 *  § Device authorization). The messages are the Rust ones. */

export type Purpose = "login" | "init";

/** The server's grant. `token` and `ingest_key` are secrets: never print them. */
export interface Grant {
  server: string;
  token: string;
  project?: { id: string; org_slug: string; slug: string; name: string };
  ingest_key?: string;
}

export const MIN_INTERVAL_MS = 1000;
export const SLOW_DOWN_STEP_MS = 5000;
export const MAX_EXPIRES_IN_S = 900;
export const DEADLINE_SLACK_MS = 5000;
/** The poll interval never exceeds the code's longest lifetime, so a huge `interval` from the
 *  server cannot overflow `setTimeout` (which would fire at once). */
const MAX_INTERVAL_MS = MAX_EXPIRES_IN_S * 1000;

const EXPIRED = "the sign-in code expired; run the command again";
const INVALID_URL = "the server sent an invalid sign-in URL";
const INVALID_SERVER = "the server sent an invalid sign-in server";
const MOVED_TWICE = "sign-in failed: the server moved the sign-in twice";
const INVALID_CODE = "the server sent an invalid sign-in code";
/** The message for every grant the CLI cannot use, here and in the local password sign-in. */
export const INVALID_GRANT = "the server returned an invalid grant";

/** What one poll answer means for the loop. `moved` names the cell the sign-in continues on. */
export type Step =
  | "wait"
  | "slow_down"
  | "denied"
  | "expired"
  | { moved: string }
  | { failed: number };

/** The meaning of one poll answer. 429 and 502 to 504 keep the code alive; a 400 carries an
 *  RFC 8628 code in its problem body; a 400 `moved` names the cell in its `server` member; anything
 *  else fails the sign-in. */
export function step(status: number, body: unknown): Step {
  if ([429, 502, 503, 504].includes(status)) return "wait";
  if (status !== 400) return { failed: status };
  const code = isRecord(body) && typeof body.code === "string" ? body.code : undefined;
  switch (code) {
    case "authorization_pending":
      return "wait";
    case "slow_down":
      return "slow_down";
    case "access_denied":
      return "denied";
    case "expired_token":
      return "expired";
    case "moved": {
      const server = isRecord(body) ? body.server : undefined;
      return typeof server === "string" ? { moved: server } : { failed: status };
    }
    default:
      return { failed: status };
  }
}

/** `XXXX-XXXX` in ASCII capitals. */
export function isUserCode(text: string): boolean {
  return /^[A-Z]{4}-[A-Z]{4}$/.test(text);
}

/** `pat-cetra-` and 32 ASCII letters or digits. */
export function isPersonalToken(text: string): boolean {
  return /^pat-cetra-[A-Za-z0-9]{32}$/.test(text);
}

/** `sk-cetra-` and 32 ASCII letters or digits. */
export function isSecretKey(text: string): boolean {
  return /^sk-cetra-[A-Za-z0-9]{32}$/.test(text);
}

/** An organization or project slug: lower-case words of letters and digits joined by single `-`,
 *  1 to 48 characters. Slugs are written into the agent prompt and the env files, so nothing else
 *  is accepted. */
export function isSlug(text: string): boolean {
  return text.length <= 48 && /^[a-z0-9]+(-[a-z0-9]+)*$/.test(text);
}

/** A project id: a UUID in the usual 8-4-4-4-12 hexadecimal form. */
export function isUuid(text: string): boolean {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(text);
}

/** Keeps a poll interval within the bounds above. */
function clampInterval(ms: number): number {
  return Math.min(Math.max(ms, MIN_INTERVAL_MS), MAX_INTERVAL_MS);
}

const defaultSleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

/** A finished sign-in: the grant, and the server that answered it (the typed server, or the cell a
 *  `moved` answer named). */
export interface SignInResult {
  grant: Grant;
  server: string;
}

/** Requests a code, prints the URL and the code on stderr, opens the browser unless `noBrowser`,
 *  and polls until the grant arrives, the user denies, or the code expires. One `moved` answer
 *  moves the polling to the cell it names (D-AO-14); a second one fails the sign-in. */
export async function deviceSignIn(options: {
  server: string;
  purpose: Purpose;
  noBrowser: boolean;
  project?: string;
  stderr: (line: string) => void;
  deps?: Deps;
}): Promise<SignInResult> {
  const { server, purpose, noBrowser, project, stderr } = options;
  const deps = options.deps ?? {};
  const sleep = deps.sleep ?? defaultSleep;
  const now = deps.now ?? Date.now;

  const host = localHost();
  const codeBody = host === undefined ? { purpose } : { purpose, host };
  const codeAnswer = await requestJson("POST", `${server}/api/auth/device/code`, {
    body: codeBody,
  });
  if (codeAnswer.status !== 200) {
    throw new CliError(
      `the server refused a sign-in code (HTTP ${codeAnswer.status})`,
      Exit.SignIn,
    );
  }

  // The server's answer is checked before anything is printed or opened.
  const answer = isRecord(codeAnswer.body) ? codeAnswer.body : {};
  const url = signInUrl(answer.verification_uri_complete, project);
  if (
    typeof answer.user_code !== "string" ||
    !isUserCode(answer.user_code) ||
    typeof answer.device_code !== "string" ||
    !isFiniteNumber(answer.expires_in) ||
    !isFiniteNumber(answer.interval)
  ) {
    throw new CliError(INVALID_CODE, Exit.SignIn);
  }
  const deviceCode = answer.device_code;
  stderr(`Open ${url.href} in your browser and approve the code.`);
  stderr(`Code: ${answer.user_code}`);
  if (!noBrowser) openBrowser(url.href, deps);

  let intervalMs = clampInterval(answer.interval * 1000);
  const lifetimeMs = Math.min(answer.expires_in, MAX_EXPIRES_IN_S) * 1000;
  const deadline = now() + lifetimeMs + DEADLINE_SLACK_MS;
  let warned = false;
  // The server the polls go to: the typed one, then the cell of the one `moved` answer.
  let pollServer = server;
  let moved = false;
  for (;;) {
    await sleep(intervalMs);
    if (now() >= deadline) throw new CliError(EXPIRED, Exit.SignIn);
    let poll: JsonAnswer;
    try {
      poll = await requestJson("POST", `${pollServer}/api/auth/device/token`, {
        body: { device_code: deviceCode },
      });
    } catch (error) {
      if (!(error instanceof CliError && error.code === Exit.Unreachable)) throw error;
      if (!warned) {
        stderr("warning: cannot reach the server; still waiting");
        warned = true;
      }
      continue;
    }
    if (poll.status === 200) {
      return { grant: checkGrant(poll.body, purpose), server: pollServer };
    }
    const next = step(poll.status, poll.body);
    if (next === "wait") continue;
    if (next === "slow_down") {
      intervalMs = clampInterval(intervalMs + SLOW_DOWN_STEP_MS);
      continue;
    }
    if (next === "denied") throw new CliError("sign-in denied in the browser", Exit.SignIn);
    if (next === "expired") throw new CliError(EXPIRED, Exit.SignIn);
    if ("moved" in next) {
      if (moved) throw new CliError(MOVED_TWICE, Exit.SignIn);
      pollServer = movedOrigin(next.moved, server);
      moved = true;
      stderr(`Continuing the sign-in at ${pollServer}`);
      continue;
    }
    throw new CliError(`sign-in failed (HTTP ${next.failed})`, Exit.SignIn);
  }
}

/** The origin a `moved` answer names. Only an https origin, or a loopback http origin when the
 *  typed server is loopback too (tests), is followed; the path, query and fragment are dropped.
 *  Loopback is tested here, not with `local.ts`, which imports this module. */
export function movedOrigin(value: string, typed: string): string {
  const invalid = () => new CliError(INVALID_SERVER, Exit.SignIn);
  let target: URL;
  let typedHost: string;
  try {
    checkServerUrl(value);
    target = new URL(value);
    typedHost = new URL(typed).hostname;
  } catch {
    throw invalid();
  }
  if (target.protocol === "https:") return target.origin;
  if (target.protocol === "http:" && isLoopbackHost(target.hostname) && isLoopbackHost(typedHost)) {
    return target.origin;
  }
  throw invalid();
}

/** `localhost`, `127.0.0.0/8` and `[::1]`, as `URL.hostname` spells them. */
function isLoopbackHost(hostname: string): boolean {
  return (
    hostname === "localhost" ||
    hostname === "[::1]" ||
    /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(hostname)
  );
}

/** The sign-in URL the server sent, with `project` added when given. Only http and https. */
function signInUrl(value: unknown, project: string | undefined): URL {
  if (typeof value !== "string") throw new CliError(INVALID_URL, Exit.SignIn);
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new CliError(INVALID_URL, Exit.SignIn);
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new CliError(INVALID_URL, Exit.SignIn);
  }
  if (project !== undefined) url.searchParams.set("project", project);
  return url;
}

/** Checks the grant of a 200 answer and keeps only the fields the CLI uses. The server is stored in
 *  its normalized form. For `init` it must also be a valid `.env` value, since `init` writes it. */
function checkGrant(body: unknown, purpose: Purpose): Grant {
  const invalid = () => new CliError(INVALID_GRANT, Exit.SignIn);
  if (!isRecord(body) || typeof body.server !== "string" || typeof body.token !== "string") {
    throw invalid();
  }
  if (!isPersonalToken(body.token)) throw invalid();
  let server: string;
  try {
    server = serverUrl(body.server);
  } catch {
    throw invalid();
  }
  const project = parseProject(body.project);
  const ingestKey =
    typeof body.ingest_key === "string" && isSecretKey(body.ingest_key)
      ? body.ingest_key
      : undefined;
  if (purpose === "init") {
    try {
      setVariables(undefined, { CETRA_BASE_URL: server });
    } catch {
      throw invalid();
    }
    if (project === undefined || ingestKey === undefined) throw invalid();
  }
  const grant: Grant = { server, token: body.token };
  if (project !== undefined) grant.project = project;
  if (ingestKey !== undefined) grant.ingest_key = ingestKey;
  return grant;
}

/** The grant's project, or undefined when the answer has none. Its ids and slugs are checked
 *  against the server's rules, and `name` loses its control characters: the values reach the
 *  agent prompt and the env file, so a server cannot put other lines or terminal codes there. */
function parseProject(value: unknown): Grant["project"] | undefined {
  if (value === undefined || value === null) return undefined;
  const invalid = () => new CliError(INVALID_GRANT, Exit.SignIn);
  if (!isRecord(value)) throw invalid();
  const { id, org_slug, slug, name } = value;
  if (
    typeof id !== "string" ||
    !isUuid(id) ||
    typeof org_slug !== "string" ||
    !isSlug(org_slug) ||
    typeof slug !== "string" ||
    !isSlug(slug) ||
    typeof name !== "string"
  ) {
    throw invalid();
  }
  return { id, org_slug, slug, name: name.replace(CONTROL, "") };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isFiniteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}
