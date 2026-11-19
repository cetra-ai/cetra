import type { Schema } from "@/api/types";

/** A user's average score: a percentage for booleans, two decimals for numbers. */
export function scoreText(score: Schema<"UserScore">): string {
  return score.data_type === "boolean" ? `${Math.round(score.avg * 100)}%` : score.avg.toFixed(2);
}

/** Stored metadata values are JSON-encoded; strings and numbers are shown decoded. */
export function decodeValue(raw: string): string {
  try {
    const value: unknown = JSON.parse(raw);
    if (typeof value === "string" || typeof value === "number") {
      return String(value);
    }
  } catch {
    // Not JSON: the stored text is shown as it is.
  }
  return raw;
}

export type UserIdentity = { name?: string; email?: string; plan?: string };

/**
 * `name`, `email` and `plan` from a user's metadata (the API strips the `user.` prefix), decoded;
 * empty strings count as absent.
 */
export function userIdentity(metadata: Record<string, string> | undefined): UserIdentity {
  const identity: UserIdentity = {};
  for (const field of ["name", "email", "plan"] as const) {
    const raw = metadata?.[field];
    const text = raw === undefined ? "" : decodeValue(raw).trim();
    if (text !== "") {
      identity[field] = text;
    }
  }
  return identity;
}
