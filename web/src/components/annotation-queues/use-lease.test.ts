import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { canLease } from "./use-lease";

type Item = Schema<"AnnotationQueueItem">;

const ME = "01900000-0000-7000-8000-000000000100";
const BOB = "01900000-0000-7000-8000-000000000101";

const base: Item = {
  id: "01900000-0000-7000-8000-000000000a11",
  queue_id: "01900000-0000-7000-8000-000000000a01",
  trace_id: "a1000000000000000000000000000001",
  target_time: "2026-01-01T09:00:00.000000000Z",
  status: "pending",
  added_by: "user",
  added_by_id: ME,
  created_at: "2026-01-01T09:00:00.000000000Z",
};

describe("canLease", () => {
  it("takes a free pending item", () => {
    expect(canLease(base, ME)).toBe(true);
  });

  it("renews the caller's own lease", () => {
    const item = { ...base, leased_by_user_id: ME, lease_expires_at: "2026-01-01T10:03:00Z" };
    expect(canLease(item, ME)).toBe(true);
  });

  it("refuses a lease held by another reviewer, whatever the client clock says", () => {
    const item = { ...base, leased_by_user_id: BOB, lease_expires_at: "2026-01-01T09:59:00Z" };
    expect(canLease(item, ME)).toBe(false);
  });

  it("refuses an item that is not pending", () => {
    expect(canLease({ ...base, status: "completed" }, ME)).toBe(false);
    expect(canLease({ ...base, status: "skipped" }, ME)).toBe(false);
  });
});
