/**
 * Bounds of a project's trace completion timeout, in seconds (D-I-8).
 *
 * These mirror `COMPLETION_TIMEOUT_MIN_S` and `COMPLETION_TIMEOUT_MAX_S` in
 * `crates/server/src/meta/projects.rs`. The OpenAPI schema does not carry the bounds, so keep
 * the two in step by hand.
 */
export const COMPLETION_TIMEOUT_MIN_S = 10;
export const COMPLETION_TIMEOUT_MAX_S = 3600;
