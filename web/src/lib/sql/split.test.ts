import { afterEach, expect, it } from "vitest";
import { DEFAULT_SQL_SPLIT, readSqlSplit, SQL_SPLIT_KEY, writeSqlSplit } from "@/lib/sql/split";

afterEach(() => {
  localStorage.clear();
});

it("reads nothing when no split is stored", () => {
  expect(readSqlSplit()).toBeNull();
});

it("writes a split and reads it back", () => {
  writeSqlSplit({ editor: 30, results: 70 });
  expect(localStorage.getItem(SQL_SPLIT_KEY)).toBe(JSON.stringify({ editor: 30, results: 70 }));
  expect(readSqlSplit()).toEqual({ editor: 30, results: 70 });
});

it.each([
  ["not json", "{"],
  ["null", "null"],
  ["an array", "[40, 60]"],
  ["a missing key", JSON.stringify({ editor: 40 })],
  ["a string", JSON.stringify({ editor: "40", results: "60" })],
  ["zero", JSON.stringify({ editor: 0, results: 100 })],
  ["one hundred", JSON.stringify({ editor: 100, results: 0 })],
  ["not summing to 100", JSON.stringify({ editor: 40, results: 40 })],
])("falls back for a stored value that is %s", (_label, raw) => {
  localStorage.setItem(SQL_SPLIT_KEY, raw);
  expect(readSqlSplit()).toBeNull();
});

it("accepts a split that is off by rounding", () => {
  localStorage.setItem(SQL_SPLIT_KEY, JSON.stringify({ editor: 33.4, results: 66.3 }));
  expect(readSqlSplit()).toEqual({ editor: 33.4, results: 66.3 });
});

it("has the default 40/60 split", () => {
  expect(DEFAULT_SQL_SPLIT).toEqual({ editor: 40, results: 60 });
});
