import { expect, it } from "vitest";
import { parseOnlineRulesSearch } from "./online-rules";

it("keeps valid online rule search and drops the rest", () => {
  expect(
    parseOnlineRulesSearch({
      rule: "01900000-0000-7000-8000-000000000700",
      status: "paused",
      q: "  relevance  ",
      sort: "spend",
      dir: "desc",
      extra: "nope",
    }),
  ).toEqual({
    rule: "01900000-0000-7000-8000-000000000700",
    status: "paused",
    q: "relevance",
    sort: "spend",
    dir: "desc",
  });
});

it("drops the removed failed sort", () => {
  expect(parseOnlineRulesSearch({ sort: "failed" })).toEqual({});
  expect(parseOnlineRulesSearch({ sort: "jobs" })).toEqual({ sort: "jobs" });
});

it("drops unknown buckets, sorts, directions and empty or too long text", () => {
  expect(
    parseOnlineRulesSearch({
      status: "firing",
      sort: "cost",
      dir: "up",
      q: "   ",
      rule: "x".repeat(257),
    }),
  ).toEqual({});
  expect(parseOnlineRulesSearch({ q: "x".repeat(201) })).toEqual({});
  expect(parseOnlineRulesSearch({ q: "x".repeat(200) })).toEqual({ q: "x".repeat(200) });
});
