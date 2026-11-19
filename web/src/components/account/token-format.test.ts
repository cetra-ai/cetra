import { expect, it } from "vitest";
import { accessLabel, expiresAt, scopesFor, tokenStatus } from "@/components/account/token-format";

it("labels the access level", () => {
  expect(accessLabel(["read"])).toBe("Read only");
  expect(accessLabel(["read", "write"])).toBe("Read and write");
  expect(accessLabel(["write", "read"])).toBe("Read and write");
});

it("maps the access level to scopes", () => {
  expect(scopesFor("read")).toEqual(["read"]);
  expect(scopesFor("read-write")).toEqual(["read", "write"]);
});

it("computes the expiry instant", () => {
  expect(expiresAt("never", 0)).toBeUndefined();
  expect(expiresAt("30", Date.UTC(2026, 0, 1))).toBe("2026-01-31T00:00:00.000Z");
  expect(expiresAt("365", Date.UTC(2026, 0, 1))).toBe("2027-01-01T00:00:00.000Z");
});

it("tells whether a token is active, expiring soon or expired", () => {
  const now = Date.UTC(2026, 0, 1);
  expect(tokenStatus(undefined, now)).toEqual({ tone: "ok", label: "Active" });
  expect(tokenStatus("2026-01-20T00:00:00.000Z", now)).toEqual({ tone: "ok", label: "Active" });
  expect(tokenStatus("2026-01-05T00:00:00.000000000Z", now)).toEqual({
    tone: "warn",
    label: "Expires soon",
  });
  expect(tokenStatus("2025-12-31T00:00:00.000Z", now)).toEqual({ tone: "error", label: "Expired" });
});
