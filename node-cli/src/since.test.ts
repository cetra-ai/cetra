import { describe, expect, it } from "vitest";
import { CliError, Exit } from "./errors.js";
import { sinceWindow } from "./since.js";

const NOW = new Date("2026-01-02T00:00:00.000Z");

describe("sinceWindow", () => {
  it("returns [now - duration, now) as RFC 3339 with nine fractional digits", () => {
    expect(sinceWindow("24h", NOW)).toEqual({
      from: "2026-01-01T00:00:00.000000000Z",
      to: "2026-01-02T00:00:00.000000000Z",
    });
    expect(sinceWindow("30m", NOW).from).toBe("2026-01-01T23:30:00.000000000Z");
    expect(sinceWindow("45s", NOW).from).toBe("2026-01-01T23:59:15.000000000Z");
    expect(sinceWindow("7d", NOW).from).toBe("2025-12-26T00:00:00.000000000Z");
  });

  it("accepts up to nine digits", () => {
    expect(() => sinceWindow("999999999s", NOW)).not.toThrow();
  });

  it("refuses bad input with the usage message and Exit.Usage", () => {
    for (const text of [
      "0h",
      "5x",
      "h",
      "01h",
      "-1h",
      "+1h",
      "1.5h",
      "",
      "1234567890h",
      "99999d",
      "9000000000000s",
    ]) {
      expect(() => sinceWindow(text, NOW), text).toThrow(CliError);
      expect(() => sinceWindow(text, NOW), text).toThrow(
        "--since expects a duration such as 30m, 24h or 7d",
      );
      try {
        sinceWindow(text, NOW);
      } catch (error) {
        expect((error as CliError).code, text).toBe(Exit.Usage);
      }
    }
  });
});
