import { expect, it } from "vitest";
import {
  parseUserPageSearch,
  parseUsersSearch,
  rangeParams,
  userPageRange,
  userTraceFilter,
} from "./users";

it("keeps valid user search and drops the rest", () => {
  expect(
    parseUsersSearch({
      sort: "cost_total",
      order: "asc",
      eval: "1",
      extra: "nope",
      search: "refund",
    }),
  ).toEqual({
    sort: "cost_total",
    order: "asc",
    eval: 1,
  });
  expect(parseUsersSearch({ sort: "duration" })).toEqual({});
});

it("keeps known column ids and drops the rest", () => {
  expect(parseUsersSearch({ cols: "nope, user, last_seen, user" })).toEqual({
    cols: "user,last_seen",
  });
  expect(parseUsersSearch({ cols: "score:resolved, score:, activity" })).toEqual({
    cols: "score:resolved,activity",
  });
});

it("keeps the exclusion flags on the user page", () => {
  expect(parseUserPageSearch({ eval: 1, exp: "1", sort: "cost_total" })).toEqual({
    eval: 1,
    exp: 1,
  });
});

it("keeps the open user and drops an empty one", () => {
  expect(parseUsersSearch({ user: " user-42 " })).toEqual({ user: "user-42" });
  expect(parseUsersSearch({ user: "   " })).toEqual({});
});

it("builds the trace filter of one user with quoted id", () => {
  expect(userTraceFilter("o'brien")).toBe("user_id = 'o''brien'");
});

it("keeps the page range, tab and valid flags and drops the rest", () => {
  expect(parseUserPageSearch({ range: "7d", tab: "traces" })).toEqual({
    range: "7d",
    tab: "traces",
  });
  expect(parseUserPageSearch({ tab: "bogus" })).toEqual({});
  expect(parseUserPageSearch({ from: "2026-11-01T00:00:00Z" })).toEqual({});
  expect(parseUserPageSearch({ from: "2026-11-02T00:00:00Z", to: "2026-11-01T00:00:00Z" })).toEqual(
    {},
  );
  expect(parseUserPageSearch({ from: "2026-11-01T00:00:00Z", to: "2026-11-02T00:00:00Z" })).toEqual(
    { from: "2026-11-01T00:00:00Z", to: "2026-11-02T00:00:00Z" },
  );
});

it("defaults the page range to the last 30 days", () => {
  expect(userPageRange({})).toEqual({ kind: "preset", preset: "30d" });
  expect(userPageRange({ range: "7d" })).toEqual({ kind: "preset", preset: "7d" });
  const absolute = {
    from: "2026-11-01T00:00:00.000Z",
    to: "2026-11-02T00:00:00.000Z",
  };
  expect(userPageRange(absolute)).toEqual({ kind: "absolute", ...absolute });
});

it("writes a range as explicit URL params", () => {
  expect(rangeParams({ kind: "preset", preset: "24h" })).toEqual({ range: "24h" });
  expect(
    rangeParams({
      kind: "absolute",
      from: "2026-11-01T00:00:00.000Z",
      to: "2026-11-02T00:00:00.000Z",
    }),
  ).toEqual({ from: "2026-11-01T00:00:00.000Z", to: "2026-11-02T00:00:00.000Z" });
});
