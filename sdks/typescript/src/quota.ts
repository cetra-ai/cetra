/**
 * The 402 pause shared by the trace exporter and the score queue (D-P-34).
 *
 * A `402` means the organization reached its monthly units cap. Both
 * transports then drop data for `Retry-After` seconds, at most one hour at a
 * time, and send again afterwards. One warning is logged per pause start.
 */

import { warn } from "./log.js";

const MAX_PAUSE_MILLIS = 3_600_000;
const DEFAULT_PAUSE_SECONDS = 3_600;
const NON_NEGATIVE_INTEGER = /^[0-9]+$/;

export class QuotaState {
  private readonly now: () => number;
  private until = Number.NEGATIVE_INFINITY;

  /** Tests inject a clock; the default is `Date.now`. */
  constructor(now: () => number = Date.now) {
    this.now = now;
  }

  /** True while a pause set by `on402` has not yet ended. */
  paused(): boolean {
    return this.now() < this.until;
  }

  /**
   * Pause for `Retry-After` seconds (at most one hour). A missing or unusable
   * header pauses for one hour. Warns once when a pause starts.
   */
  on402(retryAfter: string | null): void {
    const seconds = parseSeconds(retryAfter) ?? DEFAULT_PAUSE_SECONDS;
    const wasPaused = this.paused();
    const start = this.now();
    this.until = start + Math.min(seconds * 1000, MAX_PAUSE_MILLIS);
    if (!wasPaused) {
      const resetAt = new Date(start + seconds * 1000);
      // A huge Retry-After is outside the Date range; fall back to the pause length.
      const reset = Number.isNaN(resetAt.getTime())
        ? new Date(start + MAX_PAUSE_MILLIS).toISOString()
        : resetAt.toISOString();
      const resetText = `${reset.slice(0, 19)}Z`;
      warn(
        `llmobs: the organization reached its monthly units cap; telemetry is dropped until ${resetText} (retrying at most every hour)`,
      );
    }
  }
}

/** A non-negative integer `Retry-After` that is greater than zero, else undefined. */
function parseSeconds(retryAfter: string | null): number | undefined {
  const header = retryAfter === null ? "" : retryAfter.trim();
  if (!NON_NEGATIVE_INTEGER.test(header)) {
    return undefined;
  }
  const seconds = Number(header);
  return seconds > 0 ? seconds : undefined;
}
