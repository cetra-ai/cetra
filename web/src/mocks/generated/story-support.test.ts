import { describe, expect, it } from "vitest";
import { createDataset, type Dataset, visibleTraces } from "./dataset";
import {
  OLD_PRICING_TAG,
  SSO_TURN,
  SUPPORT_STORY_PROFILE,
  type SupportFacts,
  supportFacts,
} from "./story-support";

const NOW = Date.parse("2026-11-19T12:00:00Z");
/** Another clock: the dev UI shows the same story at any `now` (P2). */
const OTHER_NOW = Date.parse("2026-12-03T09:30:00Z");
const SEED = 1;
const TRACES = 5_000;

function build(now: number): Dataset {
  return createDataset({ seed: SEED, now, traces: TRACES, profile: SUPPORT_STORY_PROFILE });
}

const dataset = build(NOW);
const facts: SupportFacts = supportFacts(dataset);

describe("support story facts at the screenshot clock", () => {
  it("has 66 downvotes since Tuesday", () => {
    expect(facts.downvotesSinceTuesday).toBe(66);
  });

  it("has 41 traces that quote the old pricing", () => {
    expect(facts.quotingOldPricing).toBe(41);
  });

  it("has 41 of the 66 downvotes quoting the old pricing (62%)", () => {
    expect(facts.quotingShare).toBeCloseTo(41 / 66, 10);
    expect(Math.round(facts.quotingShare * 100)).toBe(62);
  });

  it("has a downvote rate after Tuesday about twice the rate before it", () => {
    expect(facts.downvoteRateAfter / facts.downvoteRateBefore).toBeGreaterThanOrEqual(1.8);
    expect(facts.downvoteRateAfter / facts.downvoteRateBefore).toBeLessThanOrEqual(2.2);
  });

  it("has 214 SSO traces this week", () => {
    expect(facts.ssoThisWeek).toBe(214);
  });

  it("has 30 SSO traces the week before", () => {
    expect(facts.ssoPreviousWeek).toBe(30);
  });

  it("has a mean correctness of at least 0.83 before the dip", () => {
    expect(facts.correctnessBeforeDip).toBeGreaterThanOrEqual(0.83);
  });

  it("has a mean correctness of at most 0.6 after the dip", () => {
    expect(facts.correctnessAfterDip).toBeLessThanOrEqual(0.6);
  });

  it("has the four turns of sess-0142, the third downvoted with correctness 0.3", () => {
    expect(facts.sessionTurns.map((turn) => turn.input)).toEqual([
      "How can I export my traces to a CSV file?",
      "What's the limit for one export?",
      "Can I schedule it every week?",
      "Okay, thanks.",
    ]);
    expect(facts.sessionTurns[2]).toEqual({
      input: "Can I schedule it every week?",
      correctness: 0.3,
      userFeedback: false,
    });
    expect(facts.sessionTurns[0]?.userFeedback).toBeUndefined();
  });
});

describe("support story facts at another clock", () => {
  const other = supportFacts(build(OTHER_NOW));

  it("keeps the counts", () => {
    expect(other.downvotesSinceTuesday).toBe(facts.downvotesSinceTuesday);
    expect(other.quotingOldPricing).toBe(facts.quotingOldPricing);
    expect(other.ssoThisWeek).toBe(facts.ssoThisWeek);
    expect(other.ssoPreviousWeek).toBe(facts.ssoPreviousWeek);
  });

  it("keeps the ratio and the means within their bounds", () => {
    expect(other.downvoteRateAfter / other.downvoteRateBefore).toBeGreaterThanOrEqual(1.8);
    expect(other.downvoteRateAfter / other.downvoteRateBefore).toBeLessThanOrEqual(2.2);
    expect(other.correctnessBeforeDip).toBeGreaterThanOrEqual(0.83);
    expect(other.correctnessAfterDip).toBeLessThanOrEqual(0.6);
  });
});

describe("support story filters", () => {
  it("finds the old-pricing traces with the release filter", () => {
    const found = visibleTraces(dataset.traces, false, false).filter(
      (trace) => trace.release === "v2.14" && trace.tags.includes(OLD_PRICING_TAG),
    );
    expect(found).toHaveLength(41);
  });

  it("gives the SSO traces the SSO question as their preview", () => {
    const sso = dataset.traces.filter((trace) => trace.input_preview === SSO_TURN.question);
    expect(sso).toHaveLength(facts.ssoThisWeek + facts.ssoPreviousWeek);
  });
});
