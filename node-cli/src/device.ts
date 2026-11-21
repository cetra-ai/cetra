import { openBrowser } from "./browser.js";
import { serverUrl } from "./credentials.js";
import { setVariables } from "./envfile.js";
import { CliError, type Deps, Exit } from "./errors.js";
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

const EXPIRED = "the sign-in code expired; run the command again";
const INVALID_URL = "the server sent an invalid sign-in URL";
const INVALID_CODE = "the server sent an invalid sign-in code";
const INVALID_GRANT = "the server returned an invalid grant";

/** What one poll answer means for the loop. */
export type Step = "wait" | "slow_down" | "denied" | "expired" | { failed: number };

/** The meaning of one poll answer. 429 and 502 to 504 keep the code alive; a 400 carries an
 *  RFC 8628 code in its problem body; anything else fails the sign-in. */
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

const defaultSleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

/** Requests a code, prints the URL and the code on stderr, opens the browser unless `noBrowser`,
 *  and polls until the grant arrives, the user denies, or the code expires. */
export async function deviceSignIn(options: {
  server: string;
  purpose: Purpose;
  noBrowser: boolean;
  project?: string;
  stderr: (line: string) => void;
  deps?: Deps;
}): Promise<Grant> {
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

  let intervalMs = Math.max(answer.interval * 1000, MIN_INTERVAL_MS);
  const lifetimeMs = Math.min(answer.expires_in, MAX_EXPIRES_IN_S) * 1000;
  const deadline = now() + lifetimeMs + DEADLINE_SLACK_MS;
  let warned = false;
  for (;;) {
    await sleep(intervalMs);
    if (now() >= deadline) throw new CliError(EXPIRED, Exit.SignIn);
    let poll: JsonAnswer;
    try {
      poll = await requestJson("POST", `${server}/api/auth/device/token`, {
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
    if (poll.status === 200) return checkGrant(poll.body, purpose);
    const next = step(poll.status, poll.body);
    if (next === "wait") continue;
    if (next === "slow_down") {
      intervalMs += SLOW_DOWN_STEP_MS;
      continue;
    }
    if (next === "denied") throw new CliError("sign-in denied in the browser", Exit.SignIn);
    if (next === "expired") throw new CliError(EXPIRED, Exit.SignIn);
    throw new CliError(`sign-in failed (HTTP ${next.failed})`, Exit.SignIn);
  }
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

/** Checks the grant of a 200 answer and keeps only the fields the CLI uses. `server` must be a
 *  valid server URL and a valid `.env` value (checked by the env file writer, which `init` uses). */
function checkGrant(body: unknown, purpose: Purpose): Grant {
  const invalid = () => new CliError(INVALID_GRANT, Exit.SignIn);
  if (!isRecord(body) || typeof body.server !== "string" || typeof body.token !== "string") {
    throw invalid();
  }
  if (!isPersonalToken(body.token)) throw invalid();
  try {
    serverUrl(body.server);
    setVariables(undefined, { CETRA_BASE_URL: body.server });
  } catch {
    throw invalid();
  }
  const project = parseProject(body.project);
  const ingestKey =
    typeof body.ingest_key === "string" && isSecretKey(body.ingest_key)
      ? body.ingest_key
      : undefined;
  if (purpose === "init" && (project === undefined || ingestKey === undefined)) throw invalid();
  const grant: Grant = { server: body.server, token: body.token };
  if (project !== undefined) grant.project = project;
  if (ingestKey !== undefined) grant.ingest_key = ingestKey;
  return grant;
}

function parseProject(value: unknown): Grant["project"] | undefined {
  if (!isRecord(value)) return undefined;
  const { id, org_slug, slug, name } = value;
  if (
    typeof id !== "string" ||
    typeof org_slug !== "string" ||
    typeof slug !== "string" ||
    typeof name !== "string"
  ) {
    return undefined;
  }
  return { id, org_slug, slug, name };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isFiniteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}
