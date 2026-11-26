import { expect, it } from "vitest";
import {
  isBooleanType,
  isDateType,
  isNestedType,
  isNumericType,
  isTextType,
  isTimestampType,
} from "@/lib/sql/types";

it("recognises numeric column types", () => {
  for (const type of ["integer", "float", "decimal"]) {
    expect(isNumericType(type), type).toBe(true);
  }
  for (const type of [
    "string",
    "timestamp",
    "date",
    "boolean",
    "interval",
    "binary",
    "string_list",
    "string_map",
    "list",
    "json",
    "null",
    "Int64",
  ]) {
    expect(isNumericType(type), type).toBe(false);
  }
});

it("recognises timestamp column types", () => {
  expect(isTimestampType("timestamp")).toBe(true);
  for (const type of ["date", "interval", "string", 'Timestamp(Nanosecond, Some("UTC"))']) {
    expect(isTimestampType(type), type).toBe(false);
  }
});

it("recognises date column types", () => {
  expect(isDateType("date")).toBe(true);
  for (const type of ["timestamp", "string", "Date32"]) {
    expect(isDateType(type), type).toBe(false);
  }
});

it("recognises text column types", () => {
  expect(isTextType("string")).toBe(true);
  for (const type of ["integer", "string_list", "json", "Utf8"]) {
    expect(isTextType(type), type).toBe(false);
  }
});

it("recognises boolean column types", () => {
  expect(isBooleanType("boolean")).toBe(true);
  for (const type of ["integer", "string", "Boolean"]) {
    expect(isBooleanType(type), type).toBe(false);
  }
});

it("recognises nested column types", () => {
  for (const type of ["string_list", "string_map", "list", "json"]) {
    expect(isNestedType(type), type).toBe(true);
  }
  for (const type of [
    "string",
    "boolean",
    "date",
    "timestamp",
    "integer",
    "interval",
    "binary",
    "null",
    "List(Int64)",
  ]) {
    expect(isNestedType(type), type).toBe(false);
  }
});
