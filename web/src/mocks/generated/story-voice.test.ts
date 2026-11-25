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

describe("the voice story data", () => {
  it("the availability checks have a median between 1.2 s and 1.7 s", () => {
    for (const facts of [atNow, atLater]) {
      expect(facts.availabilityMedianMs).toBeGreaterThanOrEqual(1_200);
      expect(facts.availabilityMedianMs).toBeLessThanOrEqual(1_700);
    }
  });

  it("the availability checks have a p95 between 3.0 s and 3.7 s", () => {
    for (const facts of [atNow, atLater]) {
      expect(facts.availabilityP95Ms).toBeGreaterThanOrEqual(3_000);
      expect(facts.availabilityP95Ms).toBeLessThanOrEqual(3_700);
    }
  });

  it("71% of the hang-up calls had a slow turn before the last", () => {
    for (const facts of [atNow, atLater]) {
      expect(Math.abs(facts.hangupsAfterSlowTurn - 0.71)).toBeLessThanOrEqual(0.02);
    }
  });

  it("at least 95% of those slow turns ran an availability check of 2 s or more", () => {
    for (const facts of [atNow, atLater]) {
      expect(facts.slowTurnsWithAvailability).toBeGreaterThanOrEqual(0.95);
    }
  });

  it("about 6% of the calls of the last 14 days were interrupted", () => {
    for (const facts of [atNow, atLater]) {
      expect(Math.abs(facts.interruptedCallShare - 0.06)).toBeLessThanOrEqual(0.015);
    }
  });

  it("every hourly p95 of the last 24 hours is above 1.5 s", () => {
    for (const facts of [atNow, atLater]) {
      expect(facts.hourlyP95Ms).toHaveLength(24);
      expect(facts.p95AllDay).toBe(true);
    }
  });
});
