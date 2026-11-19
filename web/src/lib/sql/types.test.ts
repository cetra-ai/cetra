import { expect, it } from "vitest";
import {
  isBooleanType,
  isDateType,
  isNestedType,
  isNumericType,
  isTextType,
  isTimestampType,
} from "@/lib/sql/types";

it("recognises numeric Arrow types", () => {
  for (const type of [
    "Int8",
    "Int64",
    "UInt8",
    "UInt64",
    "Float32",
    "Float64",
    "Decimal128(38, 18)",
    "Decimal256(76, 10)",
  ]) {
    expect(isNumericType(type), type).toBe(true);
  }
  for (const type of [
    "Utf8",
    'Timestamp(Nanosecond, Some("UTC"))',
    "Boolean",
    "Date32",
    "List(Int64)",
    "Interval(MonthDayNano)",
  ]) {
    expect(isNumericType(type), type).toBe(false);
  }
});

it("recognises timestamp Arrow types", () => {
  expect(isTimestampType('Timestamp(Nanosecond, Some("UTC"))')).toBe(true);
  expect(isTimestampType('Timestamp(ns, "UTC")')).toBe(true);
  expect(isTimestampType("Date32")).toBe(false);
});

it("recognises date Arrow types", () => {
  expect(isDateType("Date32")).toBe(true);
  expect(isDateType("Date64")).toBe(true);
  expect(isDateType("Utf8")).toBe(false);
});

it("recognises text Arrow types", () => {
  for (const type of ["Utf8", "LargeUtf8", "Utf8View"]) {
    expect(isTextType(type), type).toBe(true);
  }
  expect(isTextType("Int64")).toBe(false);
});

it("recognises boolean Arrow types", () => {
  expect(isBooleanType("Boolean")).toBe(true);
  expect(isBooleanType("Int64")).toBe(false);
});

it("recognises nested Arrow types", () => {
  for (const type of [
    "List(Int64)",
    "LargeList(Utf8)",
    "FixedSizeList(Float32, 3)",
    "Struct(a: Int64)",
    "Map(Utf8, Int64)",
  ]) {
    expect(isNestedType(type), type).toBe(true);
  }
  for (const type of ["Utf8", "Boolean", "Date32"]) {
    expect(isNestedType(type), type).toBe(false);
  }
});
