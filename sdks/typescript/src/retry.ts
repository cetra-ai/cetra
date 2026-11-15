/**
 * Retry policy shared by the REST clients and the background senders (D-F-8,
 * D-J-16): the backoff schedule, `Retry-After` handling and the sleep.
 */

import { runtimeGlobals } from "./web.js";

/** 1 attempt plus 3 retries. */
export const MAX_ATTEMPTS = 4;
export const RETRY_AFTER_MAX_MILLIS = 30_000;
export const NON_NEGATIVE_INTEGER = /^[0-9]+$/;

/** Backoff after the attempt with index `attempt` (0-based): 1 s, 2 s, 4 s. */
export function backoffMillis(attempt: number): number {
  switch (attempt) {
    case 0:
      return 1_000;
    case 1:
      return 2_000;
    default:
      return 4_000;
  }
}

/** `Retry-After` seconds when a non-negative integer (capped at 30 s), else the backoff. */
export function waitMillis(retryAfter: string | null, attempt: number): number {
  const header = retryAfter === null ? undefined : retryAfter.trim();
  if (header !== undefined && NON_NEGATIVE_INTEGER.test(header)) {
    return Math.min(Number(header) * 1000, RETRY_AFTER_MAX_MILLIS);
  }
  return backoffMillis(attempt);
}

export function defaultSleep(ms: number): Promise<void> {
  const { setTimeout } = runtimeGlobals();
  if (setTimeout === undefined) {
    // No timers in this runtime: continue without the wait.
    return Promise.resolve();
  }
  return new Promise<void>((resolve) => {
    setTimeout(resolve, ms);
  });
}
