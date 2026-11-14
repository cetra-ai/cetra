import { describe, expect, it, vi } from "vitest";

import { QuotaState } from "./quota.js";

const START = Date.UTC(2026, 10, 14, 21, 0, 0);

/** A clock the test moves by hand, in milliseconds. */
function fakeClock(start: number): { now: () => number; advance: (ms: number) => void } {
  let current = start;
  return {
    now: () => current,
    advance: (ms) => {
      current += ms;
    },
  };
}

function spyWarn() {
  return vi.spyOn(console, "warn").mockImplementation(() => {});
}

describe("QuotaState", () => {
  it("is not paused before any 402", () => {
    const clock = fakeClock(START);
    expect(new QuotaState(clock.now).paused()).toBe(false);
  });

  it("pauses for Retry-After seconds and resumes when they have passed", () => {
    const clock = fakeClock(START);
    const warn = spyWarn();
    try {
      const quota = new QuotaState(clock.now);
      quota.on402("60");
      expect(quota.paused()).toBe(true);
      clock.advance(59_999);
      expect(quota.paused()).toBe(true);
      clock.advance(1);
      expect(quota.paused()).toBe(false);
    } finally {
      warn.mockRestore();
    }
  });

  it("caps the pause at one hour", () => {
    const clock = fakeClock(START);
    const warn = spyWarn();
    try {
      const quota = new QuotaState(clock.now);
      quota.on402("86400");
      clock.advance(3_599_999);
      expect(quota.paused()).toBe(true);
      clock.advance(1);
      expect(quota.paused()).toBe(false);
    } finally {
      warn.mockRestore();
    }
  });

  it("pauses for one hour when Retry-After is missing, zero, or not an integer", () => {
    for (const header of [null, "0", "soon", "-5", "1.5", ""]) {
      const clock = fakeClock(START);
      const warn = spyWarn();
      try {
        const quota = new QuotaState(clock.now);
        quota.on402(header);
        clock.advance(3_599_999);
        expect(quota.paused()).toBe(true);
        clock.advance(1);
        expect(quota.paused()).toBe(false);
      } finally {
        warn.mockRestore();
      }
    }
  });

  it("warns once per pause with the reset instant and the exact text", () => {
    const clock = fakeClock(START);
    const warn = spyWarn();
    try {
      const quota = new QuotaState(clock.now);
      quota.on402("60");
      quota.on402("60");
      expect(warn).toHaveBeenCalledTimes(1);
      expect(warn).toHaveBeenCalledWith(
        "llmobs: the organization reached its monthly units cap; telemetry is dropped until 2026-11-14T21:01:00Z (retrying at most every hour)",
      );
    } finally {
      warn.mockRestore();
    }
  });

  it("warns again when a new pause starts after the previous one ended", () => {
    const clock = fakeClock(START);
    const warn = spyWarn();
    try {
      const quota = new QuotaState(clock.now);
      quota.on402("60");
      clock.advance(60_000);
      quota.on402("120");
      expect(warn).toHaveBeenCalledTimes(2);
      expect(warn).toHaveBeenLastCalledWith(
        "llmobs: the organization reached its monthly units cap; telemetry is dropped until 2026-11-14T21:03:00Z (retrying at most every hour)",
      );
    } finally {
      warn.mockRestore();
    }
  });
});
