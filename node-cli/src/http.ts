import { CliError, CONTROL, Exit } from "./errors.js";
import { VERSION } from "./version.js";

/** The problem+json body of a non-2xx answer; only the fields the CLI shows. */
export interface Problem {
  title?: string;
  status?: number;
  code?: string;
  detail?: string;
  errors?: { field: string; message: string; position?: { line: number; column: number } }[];
}

export interface JsonAnswer {
  status: number;
  headers: Headers;
  /** The parsed JSON body, or undefined when the text is not JSON. */
  body: unknown;
  text: string;
}

/** The most of an error body that is read (bytes past this are dropped). */
const PROBLEM_LIMIT = 64 * 1024;
const DEFAULT_TIMEOUT_MS = 30_000;

/** One JSON request. Never sends `Origin` unless `origin` is given; never logs. */
export async function requestJson(
  method: "GET" | "POST" | "DELETE",
  url: string,
  options: {
    body?: unknown;
    token?: string;
    cookie?: string;
    origin?: string;
    timeoutMs?: number;
  } = {},
): Promise<JsonAnswer> {
  const headers: Record<string, string> = {
    Accept: "application/json",
    "User-Agent": `cetra-cli/${VERSION}`,
  };
  if (options.body !== undefined) headers["Content-Type"] = "application/json";
  if (options.token !== undefined) headers.Authorization = `Bearer ${options.token}`;
  if (options.cookie !== undefined) headers.Cookie = options.cookie;
  if (options.origin !== undefined) headers.Origin = options.origin;

  const server = new URL(url).origin;
  let response: Response;
  let text: string;
  try {
    response = await fetch(url, {
      method,
      headers,
      body: options.body === undefined ? undefined : JSON.stringify(options.body),
      redirect: "manual",
      signal: AbortSignal.timeout(options.timeoutMs ?? DEFAULT_TIMEOUT_MS),
    });
    text = response.ok ? await response.text() : await readLimited(response);
  } catch {
    throw new CliError(`could not reach ${server}`, Exit.Unreachable);
  }
  return { status: response.status, headers: response.headers, body: parseJson(text), text };
}

async function readLimited(response: Response): Promise<string> {
  const reader = response.body?.getReader();
  if (reader === undefined) return "";
  const chunks: Uint8Array[] = [];
  let total = 0;
  while (total < PROBLEM_LIMIT) {
    const { done, value } = await reader.read();
    if (done) break;
    const room = PROBLEM_LIMIT - total;
    const kept = value.length > room ? value.subarray(0, room) : value;
    chunks.push(kept);
    total += kept.length;
  }
  await reader.cancel();
  return new TextDecoder().decode(Buffer.concat(chunks));
}

function parseJson(text: string): unknown {
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

/** Replaces control characters with spaces, so a server text cannot write terminal codes. */
function clean(text: string): string {
  return text.replace(CONTROL, " ");
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** The message for a non-2xx answer, as the Rust CLI words it: `"{title} ({code})"`, `": {detail}"`,
 *  one `"\n  {field}: {message}"` per field error (with ` (line L, column C)`), control characters
 *  replaced by spaces. 401 names the sign-in command; any other body that is not a problem gives
 *  "the server answered <status>". */
export function problemMessage(status: number, body: unknown, server: string): string {
  if (status === 401) {
    return `the server rejected the token; run \`cetra-cli login --url ${server}\``;
  }
  if (isRecord(body) && typeof body.title === "string") {
    const code = typeof body.code === "string" ? body.code : "";
    let text = `${clean(body.title)} (${clean(code)})`;
    if (typeof body.detail === "string") text += `: ${clean(body.detail)}`;
    if (Array.isArray(body.errors)) {
      for (const entry of body.errors) {
        if (
          !isRecord(entry) ||
          typeof entry.field !== "string" ||
          typeof entry.message !== "string"
        ) {
          continue;
        }
        text += `\n  ${clean(entry.field)}: ${clean(entry.message)}`;
        const position = entry.position;
        if (
          isRecord(position) &&
          typeof position.line === "number" &&
          typeof position.column === "number"
        ) {
          text += ` (line ${position.line}, column ${position.column})`;
        }
      }
    }
    return text;
  }
  return `the server answered ${status}`;
}
