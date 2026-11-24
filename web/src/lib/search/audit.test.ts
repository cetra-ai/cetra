import { describe, expect, it } from "vitest";
import { auditQuery, parseAuditSearch } from "@/lib/search/audit";

const NOW = new Date("2026-01-10T12:00:00.000Z");

describe("parseAuditSearch", () => {
  it("defaults the period to seven days and leaves the filters out", () => {
    expect(parseAuditSearch({})).toEqual({ period: "7d" });
  });

  it("keeps a valid period and drops an unknown one", () => {
    expect(parseAuditSearch({ period: "90d" })).toEqual({ period: "90d" });
    expect(parseAuditSearch({ period: "1y" })).toEqual({ period: "7d" });
  });

  it("trims text, drops empty text and text over 200 characters", () => {
    expect(parseAuditSearch({ action: "  member. ", actor: "   " })).toEqual({
      period: "7d",
      action: "member.",
    });
    expect(parseAuditSearch({ target: "x".repeat(201) })).toEqual({ period: "7d" });
    expect(parseAuditSearch({ target: "x".repeat(200) })).toEqual({
      period: "7d",
      target: "x".repeat(200),
    });
  });

  it("accepts only success and failure as outcomes", () => {
    expect(parseAuditSearch({ outcome: "failure" })).toEqual({ period: "7d", outcome: "failure" });
    expect(parseAuditSearch({ outcome: "denied" })).toEqual({ period: "7d" });
  });

  it("keeps the open record id", () => {
    expect(parseAuditSearch({ open: "01900000-0000-7000-8000-000000001d01" })).toEqual({
      period: "7d",
      open: "01900000-0000-7000-8000-000000001d01",
    });
  });
});

describe("auditQuery", () => {
  it("starts the window period before now as RFC 3339", () => {
    expect(auditQuery({ period: "24h" }, NOW)).toEqual({ from: "2026-01-09T12:00:00.000Z" });
    expect(auditQuery({ period: "7d" }, NOW)).toEqual({ from: "2026-01-03T12:00:00.000Z" });
  });

  it("has no from for all time", () => {
    expect(auditQuery({ period: "all" }, NOW)).toEqual({});
  });

  it("maps the filters to the API names", () => {
    expect(
      auditQuery(
        {
          period: "all",
          action: "member.,auth.login",
          actor: "a1",
          target: "t1",
          outcome: "success",
          open: "x",
        },
        NOW,
      ),
    ).toEqual({
      action: "member.,auth.login",
      actor_id: "a1",
      target_id: "t1",
      outcome: "success",
    });
  });
});
