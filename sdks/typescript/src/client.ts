/**
 * REST client for datasets, experiments and annotation queues (spec 10).
 *
 * `Client` raises on failure: `ConfigError` for a missing key or runtime, and
 * `ApiError` for an error answer after retries (D-J-16). Only idempotent calls
 * are retried; the callers pass `retry` for the rest.
 */

import { AnnotationQueues } from "./annotation-queues.js";
import { ConfigError, resolveConfig } from "./config.js";
import { Datasets } from "./datasets.js";
import { Prompts } from "./prompts.js";
import { defaultSleep, MAX_ATTEMPTS, waitMillis } from "./retry.js";
import { getFetch, timeoutSignal } from "./web.js";

const REQUEST_TIMEOUT_MILLIS = 30_000;

/** The part of `fetch` the REST client uses. */
export type RestFetch = (
  url: string,
  init: { method: string; headers: Record<string, string>; body?: string; signal?: unknown },
) => Promise<{
  status: number;
  ok: boolean;
  headers: { get(name: string): string | null };
  text(): Promise<string>;
}>;

export interface ClientOptions {
  apiKey?: string;
  baseUrl?: string;
  /** Tests inject a fake; the default is the global `fetch`. */
  fetch?: RestFetch;
  /** Tests inject a fake; the default is `setTimeout`. */
  sleep?: (ms: number) => Promise<void>;
}

export interface RequestOptions {
  query?: Record<string, string | number>;
  body?: unknown;
  /** Default: true for GET and DELETE, false otherwise (D-J-16). */
  retry?: boolean;
}

/** An error answer from the cetra API, or a network failure (`status` 0). */
export class ApiError extends Error {
  readonly status: number;
  readonly code: string | undefined;
  readonly detail: string | undefined;
  readonly errors: readonly { field: string; message: string }[];

  constructor(init: {
    method: string;
    template: string;
    status: number;
    code: string | undefined;
    detail: string | undefined;
    errors: readonly { field: string; message: string }[];
  }) {
    let message = `${init.method} ${init.template}: ${String(init.status)}`;
    if (init.code !== undefined) {
      message += ` ${init.code}`;
    }
    if (init.detail !== undefined) {
      message += `: ${init.detail}`;
    }
    super(message);
    this.name = "ApiError";
    this.status = init.status;
    this.code = init.code;
    this.detail = init.detail;
    this.errors = init.errors;
  }
}

export class Client {
  readonly datasets: Datasets;
  readonly prompts: Prompts;
  readonly annotationQueues: AnnotationQueues;
  readonly #apiKey: string;
  readonly #baseUrl: string;
  readonly #fetch: RestFetch;
  readonly #sleep: (ms: number) => Promise<void>;

  /** Resolves the key and base URL as `init()` does; throws `ConfigError` without a key. */
  constructor(options: ClientOptions = {}) {
    const config = resolveConfig({
      apiKey: options.apiKey,
      baseUrl: options.baseUrl,
      enabled: true,
    });
    if (!config.enabled || config.apiKey === undefined) {
      throw new ConfigError("apiKey", "required");
    }
    const fetcher = options.fetch ?? defaultFetch();
    if (fetcher === undefined) {
      throw new ConfigError("fetch", "not available in this runtime");
    }
    this.#apiKey = config.apiKey;
    this.#baseUrl = config.baseUrl;
    this.#fetch = fetcher;
    this.#sleep = options.sleep ?? defaultSleep;
    this.datasets = new Datasets(this);
    this.prompts = new Prompts(this);
    this.annotationQueues = new AnnotationQueues(this);
  }

  /**
   * One API call with the retry policy of D-J-16. `template` names the route
   * in errors (`/datasets/{dataset}/items`); `path` is the filled path with
   * its segments already encoded. Resolves to the decoded JSON, or
   * `undefined` for `204`.
   * @internal
   */
  async request<T>(
    method: string,
    template: string,
    path: string,
    options: RequestOptions = {},
  ): Promise<T> {
    const url = this.#url(path, options.query);
    const retry = options.retry ?? (method === "GET" || method === "DELETE");
    const attempts = retry ? MAX_ATTEMPTS : 1;
    const headers: Record<string, string> = { Authorization: `Bearer ${this.#apiKey}` };
    let body: string | undefined;
    if (options.body !== undefined) {
      headers["Content-Type"] = "application/json";
      body = JSON.stringify(options.body);
    }
    for (let attempt = 0; ; attempt++) {
      const last = attempt + 1 >= attempts;
      let status: number;
      let text: string;
      let retryAfter: string | null;
      let contentType: string | null;
      try {
        const response = await this.#fetch(url, {
          method,
          headers,
          body,
          signal: timeoutSignal(REQUEST_TIMEOUT_MILLIS),
        });
        status = response.status;
        retryAfter = response.headers.get("Retry-After");
        contentType = response.headers.get("Content-Type");
        text = await response.text();
      } catch (error) {
        if (!last) {
          await this.#sleep(waitMillis(null, attempt));
          continue;
        }
        throw new ApiError({
          method,
          template,
          status: 0,
          code: "network",
          detail: errorName(error),
          errors: [],
        });
      }
      if (status >= 200 && status < 300) {
        return decodeBody<T>(text, method, template, status);
      }
      const retryable = status === 429 || status >= 500;
      if (retryable && !last) {
        await this.#sleep(waitMillis(retryAfter, attempt));
        continue;
      }
      const problem = parseProblem(text, contentType);
      throw new ApiError({
        method,
        template,
        status,
        code: problem.code,
        detail: problem.detail,
        errors: problem.errors,
      });
    }
  }

  #url(path: string, query: Record<string, string | number> | undefined): string {
    const base = `${this.#baseUrl}/api/v1/projects/current${path}`;
    if (query === undefined) {
      return base;
    }
    const pairs = Object.entries(query).map(
      ([key, value]) => `${encodeURIComponent(key)}=${encodeURIComponent(String(value))}`,
    );
    return pairs.length === 0 ? base : `${base}?${pairs.join("&")}`;
  }
}

/** A path segment: an id, or a name with no safe characters left unencoded. */
export function segment(value: string): string {
  return encodeURIComponent(value);
}

/**
 * The runtime's `fetch`. It accepts a request without a body, which the
 * `FetchLike` type of `web.ts` (written for the score queue) does not say.
 */
function defaultFetch(): RestFetch | undefined {
  const fetch = getFetch();
  return fetch === undefined ? undefined : (fetch as unknown as RestFetch);
}

function decodeBody<T>(text: string, method: string, template: string, status: number): T {
  if (text === "") {
    return undefined as T;
  }
  try {
    return JSON.parse(text) as T;
  } catch {
    throw new ApiError({
      method,
      template,
      status,
      code: undefined,
      detail: "response is not JSON",
      errors: [],
    });
  }
}

interface Problem {
  code: string | undefined;
  detail: string | undefined;
  errors: { field: string; message: string }[];
}

/** The problem body of an error answer; anything unreadable gives no fields. */
function parseProblem(text: string, contentType: string | null): Problem {
  const empty: Problem = { code: undefined, detail: undefined, errors: [] };
  if (
    contentType === null ||
    !(contentType.includes("application/problem+json") || contentType.includes("application/json"))
  ) {
    return empty;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return empty;
  }
  if (typeof parsed !== "object" || parsed === null) {
    return empty;
  }
  const record = parsed as Record<string, unknown>;
  const errors: { field: string; message: string }[] = [];
  if (Array.isArray(record.errors)) {
    for (const item of record.errors as unknown[]) {
      if (typeof item === "object" && item !== null) {
        const { field, message } = item as Record<string, unknown>;
        if (typeof field === "string" && typeof message === "string") {
          errors.push({ field, message });
        }
      }
    }
  }
  return {
    code: typeof record.code === "string" ? record.code : undefined,
    detail: typeof record.detail === "string" ? record.detail : undefined,
    errors,
  };
}

function errorName(error: unknown): string {
  return error instanceof Error ? error.name : "Error";
}
