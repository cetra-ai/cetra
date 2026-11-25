import { describe, expect, it } from "vitest";
import { createDataset } from "./dataset";
import { VOICE_STORY_PROFILE, type VoiceFacts, voiceFacts } from "./story-voice";

const NOW = Date.parse("2026-11-19T12:00:00Z");
const LATER = Date.parse("2026-12-03T09:30:00Z");
const SEED = 1;
const TRACES = 2000;

function factsAt(now: number): VoiceFacts {
  return voiceFacts(
    createDataset({ seed: SEED, now, traces: TRACES, profile: VOICE_STORY_PROFILE }),
  );
}

const atNow = factsAt(NOW);
const atLater = factsAt(LATER);

// The pinned values are the screenshot clock's (NOW). A NaN (an empty set) fails every pin, so none can pass vacuously.
describe("the voice story data at the screenshot clock", () => {
  it("the availability checks have a median of 1,486 ms", () => {
    expect(atNow.availabilityMedianMs).toBe(1_486);
  });

  it("the availability checks have a p95 of 3,500 ms", () => {
    expect(atNow.availabilityP95Ms).toBe(3_500);
  });

  it("12 of the 17 hang-up calls (70.6%) had a slow turn before the last", () => {
    expect(atNow.hangupsAfterSlowTurn).toBeCloseTo(12 / 17, 10);
  });

  it("every slow turn before a hang-up ran an availability check of 2 s or more", () => {
    expect(atNow.slowTurnsWithAvailability).toBe(1);
  });

  it("1 of the 17 calls of the last 14 days was interrupted (5.9%)", () => {
    expect(atNow.interruptedCallShare).toBeCloseTo(1 / 17, 10);
  });

  it("the hourly p95 of the last 24 hours has a minimum of 3,335 ms", () => {
    expect(atNow.hourlyP95Ms).toHaveLength(24);
    expect(Math.min(...atNow.hourlyP95Ms)).toBe(3_335);
  });
});

describe("the voice story data keeps its ranges at the later clock", () => {
  it("the availability checks have a median between 1.2 s and 1.7 s", () => {
    expect(atLater.availabilityMedianMs).toBeGreaterThanOrEqual(1_200);
    expect(atLater.availabilityMedianMs).toBeLessThanOrEqual(1_700);
  });

  it("the availability checks have a p95 between 3.0 s and 3.7 s", () => {
    expect(atLater.availabilityP95Ms).toBeGreaterThanOrEqual(3_000);
    expect(atLater.availabilityP95Ms).toBeLessThanOrEqual(3_700);
  });

  it("71% of the hang-up calls had a slow turn before the last", () => {
    expect(Math.abs(atLater.hangupsAfterSlowTurn - 0.71)).toBeLessThanOrEqual(0.02);
  });

  it("at least 95% of those slow turns ran an availability check of 2 s or more", () => {
    expect(atLater.slowTurnsWithAvailability).toBeGreaterThanOrEqual(0.95);
  });

  it("about 6% of the calls of the last 14 days were interrupted", () => {
    expect(Math.abs(atLater.interruptedCallShare - 0.06)).toBeLessThanOrEqual(0.015);
  });

  it("every hourly p95 of the last 24 hours is above 1.5 s", () => {
    expect(atLater.hourlyP95Ms).toHaveLength(24);
    expect(atLater.p95AllDay).toBe(true);
  });
});
