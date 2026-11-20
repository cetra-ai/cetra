/**
 * Prompts over REST and the in-process prompt cache (spec 10, D-V-12, D-V-14,
 * D-V-15).
 *
 * `getPrompt` answers from one cache per process, keyed by base URL, name and
 * label or version. A version never expires; a label is fresh for
 * `cacheTtlS` seconds, then served stale while one background refresh runs.
 * A failed fetch falls back to the stale entry, else to the call's fallback,
 * else raises `PromptNotAvailable`. `client.prompts` is the REST sub-client
 * for scripts and CI: it is not cached and raises `ApiError`.
 */

import { ApiError, type Client, type RestFetch, segment } from "./client.js";
import type { Config } from "./config.js";
import { debug, warn } from "./log.js";
import { Prompt, PromptNotAvailable } from "./prompt.js";
import { defaultSleep, NON_NEGATIVE_INTEGER } from "./retry.js";
import { currentRuntime } from "./runtime.js";
import { type ChatEntry, type ChatMessage, PromptCompileError } from "./template.js";
import { getFetch, timeoutSignal } from "./web.js";

const DEFAULT_LABEL = "production";
const DEFAULT_TTL_S = 60;
const FETCH_TIMEOUT_MILLIS = 5_000;
/** One attempt plus two retries (D-V-14). */
const ATTEMPTS = 3;
const RETRY_AFTER_MAX_MILLIS = 5_000;
const CACHE_LIMIT = 1_000;
const WARN_INTERVAL_MILLIS = 60_000;
const LIST_PAGE_SIZE = 1_000;

export interface GetPromptOptions {
  readonly label?: string;
  readonly version?: number;
  /** Used when no prompt can be fetched: a text template or a chat template. */
  readonly fallback?: string | readonly ChatMessage[];
  /** Seconds a label stays fresh; 0 turns the cache off. Default 60. */
  readonly cacheTtlS?: number;
}

/** What a fetch asks for: a label (default `production`), or a version. */
type Target =
  | { readonly label: string; readonly version: undefined }
  | { readonly label: undefined; readonly version: number };

/** Checks the arguments of a fetch; a programming error throws `TypeError`. */
function target(name: string, label: string | undefined, version: number | undefined): Target {
  if (name === "") {
    throw new TypeError("prompt name is empty");
  }
  if (label !== undefined && version !== undefined) {
    throw new TypeError("give a label or a version, not both");
  }
  if (version !== undefined) {
    if (!Number.isInteger(version) || version < 1) {
      throw new TypeError("prompt version must be an integer of at least 1");
    }
    return { label: undefined, version };
  }
  return { label: label || DEFAULT_LABEL, version: undefined };
}

/** A cache entry: the prompt, its `ETag` and when it was fetched (cache clock). */
interface Entry {
  readonly prompt: Prompt;
  readonly etag: string | undefined;
  readonly fetchedAt: number;
}

type Outcome =
  | { readonly kind: "fetched"; readonly prompt: Prompt; readonly etag: string | undefined }
  | { readonly kind: "not_modified" }
  | { readonly kind: "failed"; readonly reason: string };

/**
 * The prompt cache (D-V-14): an LRU of at most 1 000 entries, one background
 * refresh per key, and the fetch with its retries. The seams are for tests;
 * the defaults are the real clock and the runtime's `fetch`.
 */
export class PromptCache {
  readonly #now: () => number;
  readonly #fetch: RestFetch | undefined;
  readonly #sleep: (ms: number) => Promise<void>;
  /** Insertion order is the LRU order: the first key is the oldest. */
  readonly #entries = new Map<string, Entry>();
  readonly #refreshing = new Map<string, Promise<void>>();
  #warnedAt: number | undefined;

  constructor(
    seams: {
      now?: () => number;
      fetch?: RestFetch;
      sleep?: (ms: number) => Promise<void>;
    } = {},
  ) {
    this.#now = seams.now ?? (() => Date.now());
    this.#fetch = seams.fetch ?? defaultFetch();
    this.#sleep = seams.sleep ?? defaultSleep;
  }

  now(): number {
    return this.#now();
  }

  /** The entry for `key`, moved to the newest position. */
  get(key: string): Entry | undefined {
    const entry = this.#entries.get(key);
    if (entry !== undefined) {
      this.#entries.delete(key);
      this.#entries.set(key, entry);
    }
    return entry;
  }

  store(key: string, entry: Entry): void {
    this.#entries.delete(key);
    this.#entries.set(key, entry);
    if (this.#entries.size > CACHE_LIMIT) {
      const oldest = this.#entries.keys().next().value;
      if (oldest !== undefined) {
        this.#entries.delete(oldest);
      }
    }
  }

  /** Starts `task` unless one is already running for `key`. Never rejects. */
  refreshLater(key: string, task: () => Promise<void>): void {
    if (this.#refreshing.has(key)) {
      return;
    }
    const running = task()
      .catch((error: unknown) => {
        debug(`cetra: prompt refresh failed (${errorName(error)})`);
      })
      .finally(() => {
        this.#refreshing.delete(key);
      });
    this.#refreshing.set(key, running);
  }

  /** Resolves when the background refreshes running now have finished. For tests. */
  async settle(): Promise<void> {
    await Promise.all([...this.#refreshing.values()]);
  }

  /** One warning a minute for the whole cache, naming the prompt and the reason. */
  warnFailure(name: string, reason: string): void {
    const now = this.#now();
    if (this.#warnedAt === undefined || now - this.#warnedAt >= WARN_INTERVAL_MILLIS) {
      this.#warnedAt = now;
      warn(`cetra: prompt ${name} fetch failed (${reason})`);
    }
  }

  /**
   * GET the resolve route with up to three attempts: network errors, 429 and
   * 5xx are retried, with `Retry-After` (digits only, at most 5 s) or 0.5 s,
   * then 1 s. A 404 is "not found"; any other failure is `status <n>`.
   */
  async fetchResolve(
    config: Config,
    name: string,
    t: Target,
    etag: string | undefined,
  ): Promise<Outcome> {
    const fetcher = this.#fetch;
    if (fetcher === undefined) {
      return { kind: "failed", reason: "network" };
    }
    const selector =
      t.version !== undefined ? `version=${t.version}` : `label=${encodeURIComponent(t.label)}`;
    const url = `${config.baseUrl}/api/v1/projects/current/prompts/${segment(name)}/resolve?${selector}`;
    const headers: Record<string, string> = { Authorization: `Bearer ${config.apiKey ?? ""}` };
    if (etag !== undefined) {
      headers["If-None-Match"] = etag;
    }
    for (let attempt = 0; ; attempt += 1) {
      const last = attempt + 1 >= ATTEMPTS;
      let status: number;
      let retryAfter: string | null;
      let etagHeader: string | null;
      let text = "";
      try {
        const response = await fetcher(url, {
          method: "GET",
          headers,
          signal: timeoutSignal(FETCH_TIMEOUT_MILLIS),
        });
        status = response.status;
        retryAfter = response.headers.get("Retry-After");
        etagHeader = response.headers.get("ETag");
        if (status === 200) {
          text = await response.text();
        }
      } catch {
        if (!last) {
          await this.#sleep(retryWaitMillis(null, attempt));
          continue;
        }
        return { kind: "failed", reason: "network" };
      }
      if (status === 200) {
        let body: unknown;
        try {
          body = JSON.parse(text);
        } catch {
          return { kind: "failed", reason: "invalid response" };
        }
        const prompt = parsePrompt(body);
        if (prompt === undefined) {
          return { kind: "failed", reason: "invalid response" };
        }
        return { kind: "fetched", prompt, etag: etagHeader ?? undefined };
      }
      if (status === 304 && etag !== undefined) {
        return { kind: "not_modified" };
      }
      if ((status === 429 || status >= 500) && !last) {
        await this.#sleep(retryWaitMillis(retryAfter, attempt));
        continue;
      }
      return { kind: "failed", reason: status === 404 ? "not found" : `status ${String(status)}` };
    }
  }
}

/** `Retry-After` seconds (digits only, at most 5 s), else 0.5 s after the first attempt and 1 s after the second. */
function retryWaitMillis(retryAfter: string | null, attempt: number): number {
  const header = retryAfter?.trim();
  if (header !== undefined && NON_NEGATIVE_INTEGER.test(header)) {
    return Math.min(Number(header) * 1000, RETRY_AFTER_MAX_MILLIS);
  }
  return attempt === 0 ? 500 : 1_000;
}

/** The one process-wide cache, created on first use. */
let cache: PromptCache | undefined;

function promptCache(): PromptCache {
  if (cache === undefined) {
    cache = new PromptCache();
  }
  return cache;
}

/** Replaces the process cache; tests only. `undefined` drops it. */
export function setPromptCacheForTests(next: PromptCache | undefined): void {
  cache = next;
}

/**
 * The prompt `name` at its label (default `production`) or version. Answers
 * from the cache when it can (D-V-14); throws `TypeError` for bad arguments
 * and `PromptNotAvailable` when nothing can answer and there is no fallback.
 * Before `init()`, or with the SDK disabled, it behaves like a failed fetch.
 */
export async function getPrompt(name: string, options: GetPromptOptions = {}): Promise<Prompt> {
  const t = target(name, options.label, options.version);
  const ttl = options.cacheTtlS ?? DEFAULT_TTL_S;
  if (typeof ttl !== "number" || !Number.isFinite(ttl) || ttl < 0) {
    throw new TypeError("cacheTtlS must be a finite number of at least 0");
  }
  const config = currentRuntime()?.config;
  if (config === undefined || config.apiKey === undefined) {
    return unavailable(name, options.fallback, "sdk disabled");
  }
  const store = promptCache();
  const key = JSON.stringify([
    config.baseUrl,
    name,
    t.version === undefined ? "label" : "version",
    t.version ?? t.label,
  ]);
  const entry = store.get(key);
  // A version never changes, so its entry is fresh whatever the TTL is.
  const fresh =
    entry !== undefined &&
    (t.version !== undefined || (ttl > 0 && store.now() - entry.fetchedAt < ttl * 1000));
  if (entry !== undefined && fresh) {
    return entry.prompt;
  }
  if (entry !== undefined && ttl > 0) {
    // Stale label: serve it now and refresh behind the call (D-V-14).
    store.refreshLater(key, () => revalidate(store, key, config, name, t, entry));
    return entry.prompt;
  }
  const outcome = await store.fetchResolve(config, name, t, undefined);
  if (outcome.kind === "fetched") {
    if (ttl > 0) {
      store.store(key, {
        prompt: outcome.prompt,
        etag: outcome.etag,
        fetchedAt: store.now(),
      });
    }
    return outcome.prompt;
  }
  // Without an ETag a 304 cannot come back, so only a failure is left here.
  const reason = outcome.kind === "failed" ? outcome.reason : "invalid response";
  if (entry !== undefined) {
    store.warnFailure(name, reason);
    return entry.prompt;
  }
  return unavailable(name, options.fallback, reason);
}

/** The background refresh of a stale label entry (D-V-14). */
async function revalidate(
  store: PromptCache,
  key: string,
  config: Config,
  name: string,
  t: Target,
  entry: Entry,
): Promise<void> {
  const outcome = await store.fetchResolve(config, name, t, entry.etag);
  if (outcome.kind === "fetched") {
    store.store(key, { prompt: outcome.prompt, etag: outcome.etag, fetchedAt: store.now() });
  } else if (outcome.kind === "not_modified") {
    store.store(key, { ...entry, fetchedAt: store.now() });
  } else {
    // Keep the prompt and restart its age: a failing server is retried once per TTL.
    store.store(key, { ...entry, fetchedAt: store.now() });
    store.warnFailure(name, outcome.reason);
  }
}

/** The fallback as a prompt, or `PromptNotAvailable` when there is none. */
function unavailable(
  name: string,
  fallback: string | readonly ChatMessage[] | undefined,
  reason: string,
): Prompt {
  if (fallback === undefined) {
    throw new PromptNotAvailable(name, reason);
  }
  debug(`cetra: prompt ${name} not available (${reason}); using the fallback`);
  try {
    return new Prompt({
      name,
      version: undefined,
      config: {},
      labels: [],
      isFallback: true,
      template: fallback,
    });
  } catch (error) {
    if (error instanceof PromptCompileError) {
      throw new PromptCompileError(error.kind, {
        variable: error.variable,
        reason: error.reason,
        line: error.line,
        column: error.column,
        prompt: name,
      });
    }
    throw error;
  }
}

/**
 * A prompt from a resolve or create answer, or `undefined` when the body does
 * not have the shape of a prompt (or its template does not render).
 */
export function parsePrompt(body: unknown): Prompt | undefined {
  if (!isRecord(body)) {
    return undefined;
  }
  const { name, type, version, template, config, labels } = body;
  if (
    typeof name !== "string" ||
    name === "" ||
    typeof version !== "number" ||
    !Number.isInteger(version) ||
    version < 1 ||
    !isRecord(config) ||
    !isStringArray(labels)
  ) {
    return undefined;
  }
  let shape: string | ChatEntry[];
  if (type === "text" && typeof template === "string") {
    shape = template;
  } else if (type === "chat" && Array.isArray(template) && template.every(isFlatEntry)) {
    shape = template as ChatEntry[];
  } else {
    return undefined;
  }
  try {
    return new Prompt({ name, version, config, labels, isFallback: false, template: shape });
  } catch (error) {
    if (error instanceof PromptCompileError) {
      return undefined;
    }
    throw error;
  }
}

/** `client.prompts`: the REST operations on prompts (spec 10, D-V-12). */
export class Prompts {
  readonly #client: Client;

  /** @internal */
  constructor(client: Client) {
    this.#client = client;
  }

  /** The prompt at its label (default `production`) or version. Not cached. */
  async get(name: string, options: { label?: string; version?: number } = {}): Promise<Prompt> {
    const t = target(name, options.label, options.version);
    const body = await this.#client.request<unknown>(
      "GET",
      "/prompts/{prompt}/resolve",
      `/prompts/${segment(name)}/resolve`,
      { query: t.version !== undefined ? { version: t.version } : { label: t.label } },
    );
    return promptFrom("GET", "/prompts/{prompt}/resolve", body);
  }

  /** The names of every prompt in the project. */
  async list(): Promise<string[]> {
    const names: string[] = [];
    let cursor: string | undefined;
    for (;;) {
      const query: Record<string, string | number> = { limit: LIST_PAGE_SIZE };
      if (cursor !== undefined) {
        query.cursor = cursor;
      }
      const page = await this.#client.request<unknown>("GET", "/prompts", "/prompts", { query });
      if (!isRecord(page) || !Array.isArray(page.data)) {
        throw invalidResponse("GET", "/prompts");
      }
      for (const item of page.data as unknown[]) {
        if (!isRecord(item) || typeof item.name !== "string") {
          throw invalidResponse("GET", "/prompts");
        }
        names.push(item.name);
      }
      if (page.next_cursor === null || page.next_cursor === undefined) {
        return names;
      }
      if (typeof page.next_cursor !== "string") {
        throw invalidResponse("GET", "/prompts");
      }
      cursor = page.next_cursor;
    }
  }

  /** Create a prompt with its version 1. Sent once: a retry could create a second one. */
  async create(input: {
    name: string;
    type: "text" | "chat";
    template: string | readonly ChatEntry[];
    config?: Record<string, unknown>;
    labels?: readonly string[];
    commitMessage?: string;
    description?: string;
    tags?: readonly string[];
  }): Promise<Prompt> {
    if (input.name === "") {
      throw new TypeError("prompt name is empty");
    }
    const body: Record<string, unknown> = {
      name: input.name,
      type: input.type,
      ...versionFields(input),
    };
    if (input.description !== undefined) {
      body.description = input.description;
    }
    if (input.tags !== undefined) {
      body.tags = [...input.tags];
    }
    const answer = await this.#client.request<unknown>("POST", "/prompts", "/prompts", {
      body,
      retry: false,
    });
    return promptFrom("POST", "/prompts", answer);
  }

  /** Add a version to a prompt. Sent once, for the same reason as `create`. */
  async createVersion(
    name: string,
    input: {
      template: string | readonly ChatEntry[];
      config?: Record<string, unknown>;
      labels?: readonly string[];
      commitMessage?: string;
    },
  ): Promise<Prompt> {
    if (name === "") {
      throw new TypeError("prompt name is empty");
    }
    const route = "/prompts/{prompt}/versions";
    const answer = await this.#client.request<unknown>(
      "POST",
      route,
      `/prompts/${segment(name)}/versions`,
      { body: versionFields(input), retry: false },
    );
    return promptFrom("POST", route, answer);
  }

  /** Move `label` to `version`. Sent once. */
  async setLabel(name: string, label: string, version: number): Promise<void> {
    if (name === "" || label === "") {
      throw new TypeError("prompt name and label must not be empty");
    }
    await this.#client.request<void>(
      "PUT",
      "/prompts/{prompt}/labels/{label}",
      `/prompts/${segment(name)}/labels/${segment(label)}`,
      { body: { version }, retry: false },
    );
  }
}

/** The request fields of a version, with the optional ones left out when not given. */
function versionFields(input: {
  template: string | readonly ChatEntry[];
  config?: Record<string, unknown>;
  labels?: readonly string[];
  commitMessage?: string;
}): Record<string, unknown> {
  const body: Record<string, unknown> = { template: input.template };
  if (input.config !== undefined) {
    body.config = input.config;
  }
  if (input.labels !== undefined) {
    body.labels = [...input.labels];
  }
  if (input.commitMessage !== undefined) {
    body.commit_message = input.commitMessage;
  }
  return body;
}

function promptFrom(method: string, template: string, body: unknown): Prompt {
  const prompt = parsePrompt(body);
  if (prompt === undefined) {
    throw invalidResponse(method, template);
  }
  return prompt;
}

function invalidResponse(method: string, template: string): ApiError {
  return new ApiError({
    method,
    template,
    status: 200,
    code: "invalid_response",
    detail: "invalid response",
    errors: [],
  });
}

/**
 * A chat entry as the wire has it: a message with exactly string `role` and
 * `content`, or a placeholder with exactly `type: "placeholder"` and a string
 * `name`. Anything else (a non-string value, an extra key) is not a prompt.
 */
function isFlatEntry(entry: unknown): boolean {
  if (!isRecord(entry)) {
    return false;
  }
  const keys = Object.keys(entry).sort().join(",");
  if (entry.type === "placeholder") {
    return keys === "name,type" && typeof entry.name === "string";
  }
  return (
    keys === "content,role" && typeof entry.role === "string" && typeof entry.content === "string"
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isStringArray(value: unknown): value is string[] {
  return Array.isArray(value) && value.every((item: unknown) => typeof item === "string");
}

/** The runtime's `fetch` as the REST client types it; the same as `client.ts`' default. */
function defaultFetch(): RestFetch | undefined {
  const fetch = getFetch();
  return fetch === undefined ? undefined : (fetch as unknown as RestFetch);
}

function errorName(error: unknown): string {
  return error instanceof Error ? error.name : typeof error;
}
