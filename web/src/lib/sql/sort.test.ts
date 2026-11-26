import { expect, it } from "vitest";
import {
  compareSqlValues,
  isMissingSqlValue,
  type SqlSortKind,
  sortKindOf,
  sortSqlRows,
} from "@/lib/sql/sort";

interface Row {
  index: number;
  value: unknown;
}

function rowsOf(values: unknown[]): Row[] {
  return values.map((value, index) => ({ index, value }));
}

function sorted(values: unknown[], kind: SqlSortKind, desc: boolean): unknown[] {
  return sortSqlRows(rowsOf(values), (row) => row.value, kind, desc).map((row) => row.value);
}

it("picks the comparison kind from the column type", () => {
  expect(sortKindOf("integer")).toBe("number");
  expect(sortKindOf("float")).toBe("number");
  expect(sortKindOf("decimal")).toBe("number");
  expect(sortKindOf("string")).toBe("text");
  expect(sortKindOf("timestamp")).toBe("other");
  expect(sortKindOf("boolean")).toBe("other");
});

it("keeps nulls and missing cells last in both directions", () => {
  const values = [3, null, 1, undefined, 2];
  expect(sorted(values, "number", false)).toEqual([1, 2, 3, null, undefined]);
  expect(sorted(values, "number", true)).toEqual([3, 2, 1, null, undefined]);
  const words = [null, "b", undefined, "a"];
  expect(sorted(words, "text", false)).toEqual(["a", "b", null, undefined]);
  expect(sorted(words, "text", true)).toEqual(["b", "a", null, undefined]);
});

it("compares numeric strings numerically", () => {
  expect(sorted(["10", "9", 2.5, "-1"], "number", false)).toEqual(["-1", 2.5, "9", "10"]);
  expect(sorted(["10", "9", 2.5], "number", true)).toEqual(["10", "9", 2.5]);
});

it("sorts an unparseable number with the nulls", () => {
  expect(isMissingSqlValue("x", "number")).toBe(true);
  expect(isMissingSqlValue("x", "text")).toBe(false);
  expect(sorted(["x", 2, null, 1], "number", false)).toEqual([1, 2, "x", null]);
  expect(sorted(["x", 2, null, 1], "number", true)).toEqual([2, 1, "x", null]);
});

it("keeps equal cells in their original order in both directions", () => {
  const rows = rowsOf(["b", "a", "b", "a"]);
  const asc = sortSqlRows(rows, (row) => row.value, "text", false).map((row) => row.index);
  const desc = sortSqlRows(rows, (row) => row.value, "text", true).map((row) => row.index);
  expect(asc).toEqual([1, 3, 0, 2]);
  expect(desc).toEqual([0, 2, 1, 3]);
});

it("compares text with localeCompare", () => {
  expect(compareSqlValues("a", "b", "text")).toBeLessThan(0);
  expect(compareSqlValues("b", "a", "text")).toBeGreaterThan(0);
  expect(sorted(["b", "a", "c"], "text", false)).toEqual(["a", "b", "c"]);
});

it("compares timestamps as RFC 3339 strings", () => {
  const stamps = [
    "2026-11-17T10:00:00.000000002Z",
    "2026-11-16T23:59:59.000000000Z",
    "2026-11-17T09:00:00.000000001Z",
  ];
  expect(sorted(stamps, "other", false)).toEqual([
    "2026-11-16T23:59:59.000000000Z",
    "2026-11-17T09:00:00.000000001Z",
    "2026-11-17T10:00:00.000000002Z",
  ]);
});

it("orders booleans false before true", () => {
  expect(sorted([true, false, null, true], "other", false)).toEqual([false, true, true, null]);
  expect(sorted([true, false], "other", true)).toEqual([true, false]);
});

it("orders objects and arrays by their JSON text", () => {
  expect(sorted([{ b: 1 }, { a: 1 }], "other", false)).toEqual([{ a: 1 }, { b: 1 }]);
  expect(sorted([["y"], ["x"]], "other", false)).toEqual([["x"], ["y"]]);
});
