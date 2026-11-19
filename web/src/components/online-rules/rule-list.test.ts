import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import {
  borderTone,
  countBuckets,
  filterRules,
  jobTotal,
  sortRules,
  spendToday,
} from "./rule-list";

type Rule = Schema<"OnlineRule">;

function rule(overrides: Partial<Rule> & { name: string }): Rule {
  return {
    id: overrides.name,
    evaluator_id: "ev-1",
    target: "trace",
    variable_mapping: {},
    sample_rate: 1,
    enabled: true,
    jobs_last_24h: { queued: 0, running: 0, done: 0, failed: 0, skipped: 0 },
    dropped_last_hour: 0,
    created_by: "u",
    created_at: "2026-01-01T00:00:00.000Z",
    updated_at: "2026-01-01T00:00:00.000Z",
    ...overrides,
  } as Rule;
}

const names = (rules: readonly Rule[]) => rules.map((item) => item.name);

const CHEAP = rule({
  name: "Cheap",
  budget_usd: 10,
  budget_period: "day",
  spend: { period_start: "x", spent_usd: 1, reserved_usd: 0 },
  jobs_last_24h: { queued: 0, running: 0, done: 4, failed: 1, skipped: 0 },
});
const BUSY = rule({
  name: "Busy",
  budget_usd: 4,
  budget_period: "day",
  spend: { period_start: "x", spent_usd: 3.6, reserved_usd: 0 },
  jobs_last_24h: { queued: 2, running: 0, done: 20, failed: 0, skipped: 0 },
});
const NO_BUDGET = rule({
  name: "Free",
  paused_reason: "errors",
  jobs_last_24h: { queued: 0, running: 0, done: 0, failed: 9, skipped: 0 },
});
const OFF = rule({ name: "Off", enabled: false });

const noScore = () => undefined;
const SCORES: Record<string, number | undefined> = { Busy: 0.4, Cheap: 0.9, Free: undefined };

describe("rule list helpers", () => {
  it("totals the jobs of every status", () => {
    expect(jobTotal(BUSY)).toBe(22);
    expect(jobTotal(NO_BUDGET)).toBe(9);
  });

  it("counts the buckets and sums the daily spend", () => {
    expect(countBuckets([CHEAP, NO_BUDGET, OFF])).toEqual({ active: 1, paused: 1, disabled: 1 });
    expect(spendToday([CHEAP, BUSY, NO_BUDGET])).toBeCloseTo(4.6);
    expect(spendToday([NO_BUDGET])).toBeUndefined();
  });

  it("colours the border by pause reason", () => {
    expect(borderTone(NO_BUDGET)).toBe("border-l-status-error");
    expect(borderTone(rule({ name: "b", paused_reason: "budget" }))).toBe("border-l-status-warn");
    expect(borderTone(CHEAP)).toBe("border-l-transparent");
  });

  it("filters by bucket and by text in name, evaluator or filter", () => {
    const rules = [
      rule({ name: "Relevance", filter: "status = 'ok'" }),
      rule({ name: "Toxicity", evaluator_id: "ev-tox" }),
    ];
    const evaluatorName = (id: string) => (id === "ev-tox" ? "Safety judge" : undefined);
    expect(names(filterRules(rules, { evaluatorName, q: "RELEV" }))).toEqual(["Relevance"]);
    expect(names(filterRules(rules, { evaluatorName, q: "safety" }))).toEqual(["Toxicity"]);
    expect(names(filterRules(rules, { evaluatorName, q: "'ok'" }))).toEqual(["Relevance"]);
    expect(names(filterRules([CHEAP, OFF], { evaluatorName, status: "disabled" }))).toEqual([
      "Off",
    ]);
  });

  it("orders by bucket then name when nothing is sorted", () => {
    expect(names(sortRules([OFF, NO_BUDGET, BUSY, CHEAP], undefined, "asc", noScore))).toEqual([
      "Busy",
      "Cheap",
      "Free",
      "Off",
    ]);
  });

  it("sorts by name in both directions", () => {
    expect(names(sortRules([CHEAP, BUSY, OFF], "name", "asc", noScore))).toEqual([
      "Busy",
      "Cheap",
      "Off",
    ]);
    expect(names(sortRules([CHEAP, BUSY, OFF], "name", "desc", noScore))).toEqual([
      "Off",
      "Cheap",
      "Busy",
    ]);
  });

  it("puts rules without a budget last for spend sorts in both directions", () => {
    const rules = [NO_BUDGET, CHEAP, BUSY];
    expect(names(sortRules(rules, "spend", "desc", noScore))).toEqual(["Busy", "Cheap", "Free"]);
    expect(names(sortRules(rules, "spend", "asc", noScore))).toEqual(["Cheap", "Busy", "Free"]);
  });

  it("sorts by score in both directions, with rules without a score last", () => {
    const rules = [NO_BUDGET, BUSY, CHEAP];
    const scoreOf = (item: Rule) => SCORES[item.name];
    expect(names(sortRules(rules, "score", "desc", scoreOf))).toEqual(["Cheap", "Busy", "Free"]);
    expect(names(sortRules(rules, "score", "asc", scoreOf))).toEqual(["Busy", "Cheap", "Free"]);
  });

  it("sorts by jobs and by status", () => {
    expect(names(sortRules([CHEAP, BUSY, NO_BUDGET], "jobs", "desc", noScore))).toEqual([
      "Busy",
      "Free",
      "Cheap",
    ]);
    expect(names(sortRules([OFF, NO_BUDGET, CHEAP], "status", "asc", noScore))).toEqual([
      "Cheap",
      "Free",
      "Off",
    ]);
  });
});
