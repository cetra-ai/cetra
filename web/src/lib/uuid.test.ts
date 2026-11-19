import { afterEach, describe, expect, it, vi } from "vitest";
import { uuidv7 } from "./uuid";

afterEach(() => {
  vi.restoreAllMocks();
});

describe("uuidv7", () => {
  it("is a lowercase UUID version 7, RFC variant", () => {
    expect(uuidv7()).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
    );
  });

  it("uuidv7 encodes the current time in its first 48 bits", () => {
    const now = 1_750_000_123_456;
    vi.spyOn(Date, "now").mockReturnValue(now);
    const id = uuidv7();
    expect(Number.parseInt(id.replace(/-/g, "").slice(0, 12), 16)).toBe(now);
  });

  it("sorts in the order of the time it was made", () => {
    const earlier = uuidv7(1_700_000_000_000);
    const later = uuidv7(1_700_000_000_001);
    expect(earlier < later).toBe(true);
    const millis = Number.parseInt(earlier.replace(/-/g, "").slice(0, 12), 16);
    expect(millis).toBe(1_700_000_000_000);
  });
});

// crypto.randomUUID is undefined over plain http, e.g. a dev server reached by host name.
it("no web source calls crypto.randomUUID", () => {
  const sources = import.meta.glob(["/src/**/*.{ts,tsx}", "!/src/**/*.test.{ts,tsx}"], {
    query: "?raw",
    import: "default",
    eager: true,
  });
  expect(Object.keys(sources).length).toBeGreaterThan(100);
  const callers = Object.entries(sources)
    .filter(([, text]) => typeof text === "string" && text.includes("randomUUID("))
    .map(([path]) => path);
  expect(callers).toEqual([]);
});
