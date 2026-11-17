import { expect, it } from "vitest";
import { FIT_ROWS, fitWidth, formatSqlCell } from "@/lib/sql/widths";

it("formats one JSON cell for display", () => {
  expect(formatSqlCell(null)).toBe("NULL");
  expect(formatSqlCell(undefined)).toBe("NULL");
  expect(formatSqlCell("x")).toBe("x");
  expect(formatSqlCell(3.5)).toBe("3.5");
  expect(formatSqlCell(false)).toBe("false");
  expect(formatSqlCell({ k: "v" })).toBe('{"k":"v"}');
  expect(formatSqlCell([1, 2])).toBe("[1,2]");
});

it("clamps the width to its minimum for short content", () => {
  expect(fitWidth("x", [])).toBe(80);
  expect(fitWidth("", ["a", null])).toBe(80);
});

it("clamps the width to its maximum for long content", () => {
  expect(fitWidth("x", ["y".repeat(100)])).toBe(480);
});

it("sizes to the content between the bounds", () => {
  // 10 characters: 10 * 7 + 32.
  expect(fitWidth("x", ["y".repeat(10)])).toBe(102);
});

it("uses the header when it is longer than every value", () => {
  // 23 characters: 23 * 7 + 32.
  expect(fitWidth("a very long header name", ["x"])).toBe(193);
});

it("measures the display text, so NULL and JSON count by their shown length", () => {
  // `{"a":1}` is 7 characters, `NULL` is 4: 7 * 7 + 32.
  expect(fitWidth("x", [null, { a: 1 }])).toBe(81);
});

it("looks at only the first 200 values", () => {
  const values = [...Array.from({ length: FIT_ROWS }, () => "a"), "y".repeat(300)];
  expect(fitWidth("h", values)).toBe(80);
});
