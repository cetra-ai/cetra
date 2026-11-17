import type { Schema } from "@/api/types";
import type { PillTone } from "@/components/settings/status-pill";

export type TokenAccess = "read" | "read-write";
export type TokenExpiry = "never" | "30" | "90" | "365";

export const EXPIRY_OPTIONS: readonly { value: TokenExpiry; label: string }[] = [
  { value: "30", label: "30 days" },
  { value: "90", label: "90 days" },
  { value: "365", label: "1 year" },
  { value: "never", label: "Never" },
];

/** `["read"]` → "Read only"; `["read", "write"]` (any order) → "Read and write". */
export function accessLabel(scopes: readonly Schema<"PersonalTokenScope">[]): string {
  return scopes.includes("write") ? "Read and write" : "Read only";
}

export function scopesFor(access: TokenAccess): Schema<"PersonalTokenScope">[] {
  return access === "read-write" ? ["read", "write"] : ["read"];
}

/** RFC 3339 instant `days` after `now` (ms since epoch), or undefined for "never". */
export function expiresAt(expiry: TokenExpiry, now: number): string | undefined {
  if (expiry === "never") {
    return undefined;
  }
  return new Date(now + Number(expiry) * 86_400_000).toISOString();
}

const EXPIRES_SOON_MS = 7 * 86_400_000;

/** A token's state at `now` (ms since epoch): expired, expiring within a week, or active. */
export function tokenStatus(
  expiresAt: string | undefined,
  now: number,
): { tone: PillTone; label: string } {
  if (expiresAt === undefined) {
    return { tone: "ok", label: "Active" };
  }
  const left = Date.parse(expiresAt) - now;
  if (left <= 0) {
    return { tone: "error", label: "Expired" };
  }
  if (left <= EXPIRES_SOON_MS) {
    return { tone: "warn", label: "Expires soon" };
  }
  return { tone: "ok", label: "Active" };
}
