import { reportUnauthorized } from "@/api/session";

export type StreamBlock = { event: string; data: string };

/** Problem body or network failure before the stream starts. */
export class StreamRequestError extends Error {
  readonly status: number; // 0 for a network failure
  readonly problem: unknown; // the parsed Problem body, or undefined
  readonly retryAfter?: number; // seconds, from a `Retry-After` header holding an integer

  constructor(status: number, problem: unknown, retryAfter?: number) {
    super(`The request failed with status ${status}.`);
    this.name = "StreamRequestError";
    this.status = status;
    this.problem = problem;
    if (retryAfter !== undefined) {
      this.retryAfter = retryAfter;
    }
  }
}

/**
 * POSTs `body` as JSON to `url` (same origin, `credentials: "same-origin"`, `Accept: text/event-stream`).
 * A non-2xx answer: calls `reportUnauthorized(pathname)` on 401, then throws `StreamRequestError`.
 * A 2xx answer: reads the body, splits blocks on a blank line, joins `data:` lines with "\n",
 * takes the last `event:` line (default "message"), ignores comment lines (`:`) and `id:`/`retry:`,
 * and calls `onBlock` for each block in order. Resolves when the body ends.
 * Aborting `signal` rejects with the abort's `DOMException` (`name === "AbortError"`).
 */
export async function postEventStream(
  url: string,
  body: unknown,
  options: { signal: AbortSignal; onBlock: (block: StreamBlock) => void },
): Promise<void> {
  let response: Response;
  try {
    response = await fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/json", Accept: "text/event-stream" },
      credentials: "same-origin",
      body: JSON.stringify(body),
      signal: options.signal,
    });
  } catch (error) {
    if (isAbortError(error)) {
      throw error;
    }
    throw new StreamRequestError(0, undefined);
  }

  if (!response.ok) {
    if (response.status === 401) {
      reportUnauthorized(new URL(url, window.location.origin).pathname);
    }
    const problem = await readProblem(response);
    throw new StreamRequestError(response.status, problem, retryAfterOf(response));
  }

  if (!response.body) {
    return;
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  for (;;) {
    const { done, value } = await reader.read();
    if (done) {
      break;
    }
    buffer += decoder.decode(value, { stream: true });
    buffer = drain(buffer, options);
  }
  buffer += decoder.decode();
  // A block at the end of the body still counts when it carries data.
  const block = parseBlock(normalizeLineEnds(buffer));
  if (block !== undefined) {
    options.signal.throwIfAborted();
    options.onBlock(block);
  }
}

function isAbortError(error: unknown): boolean {
  return error instanceof Error && error.name === "AbortError";
}

async function readProblem(response: Response): Promise<unknown> {
  const contentType = response.headers.get("Content-Type") ?? "";
  if (!contentType.includes("json")) {
    return undefined;
  }
  try {
    return (await response.json()) as unknown;
  } catch {
    return undefined;
  }
}

function retryAfterOf(response: Response): number | undefined {
  const value = response.headers.get("Retry-After");
  if (value === null || !/^\d+$/.test(value.trim())) {
    return undefined;
  }
  return Number(value.trim());
}

/** Emits every complete block in `buffer` and returns the unterminated tail. */
function drain(
  buffer: string,
  options: { signal: AbortSignal; onBlock: (block: StreamBlock) => void },
): string {
  // A lone trailing "\r" stays in the tail until the next chunk shows whether "\n" follows.
  const parts = normalizeLineEnds(buffer).split("\n\n");
  const rest = parts.pop() ?? "";
  for (const part of parts) {
    const block = parseBlock(part);
    if (block !== undefined) {
      options.signal.throwIfAborted();
      options.onBlock(block);
    }
  }
  return rest;
}

function normalizeLineEnds(text: string): string {
  return text.replace(/\r\n/g, "\n");
}

/** One block's lines. `undefined` when it has no `data:` line. */
function parseBlock(text: string): StreamBlock | undefined {
  let event = "message";
  const data: string[] = [];
  for (const line of text.split("\n")) {
    if (line === "" || line.startsWith(":")) {
      continue;
    }
    const colon = line.indexOf(":");
    const field = colon === -1 ? line : line.slice(0, colon);
    let value = colon === -1 ? "" : line.slice(colon + 1);
    if (value.startsWith(" ")) {
      value = value.slice(1);
    }
    if (field === "event") {
      event = value;
    } else if (field === "data") {
      data.push(value);
    }
  }
  if (data.length === 0) {
    return undefined;
  }
  return { event, data: data.join("\n") };
}
