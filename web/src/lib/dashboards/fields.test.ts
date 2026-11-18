import { expect, it } from "vitest";
import {
  breakdownFields,
  countDistinctFields,
  fieldKind,
  fieldsForAgg,
  isMapKeyField,
  mapColumns,
  numericFields,
} from "./fields";

// Spec 05 § Dashboard queries, the field table.
it("lists the numeric fields of each source as spec 05 does", () => {
  expect(numericFields("traces")).toEqual([
    "duration_ns",
    "span_count",
    "error_count",
    "llm_count",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "reasoning_tokens",
    "total_tokens",
    "cost_total",
  ]);
  expect(numericFields("spans")).toEqual([
    "duration_ns",
    "ttft_ns",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "reasoning_tokens",
    "total_tokens",
    "cost_input",
    "cost_output",
    "cost_total",
  ]);
  expect(numericFields("scores")).toEqual(["value_num"]);
});

it("lists the breakdown fields of each source without bare map columns", () => {
  expect(breakdownFields("traces")).toEqual([
    "name",
    "status",
    "environment",
    "release",
    "user_id",
    "tags",
    "models",
  ]);
  expect(breakdownFields("spans")).toEqual([
    "name",
    "span_kind",
    "status",
    "service_name",
    "environment",
    "release",
    "provider",
    "model",
    "operation",
    "user_id",
    "tags",
  ]);
  expect(breakdownFields("scores")).toEqual(["name", "source", "data_type", "target", "value_str"]);
  for (const source of ["traces", "spans", "scores"] as const) {
    for (const column of mapColumns(source)) {
      expect(breakdownFields(source)).not.toContain(column);
    }
  }
});

it("names the map columns of each source", () => {
  expect(mapColumns("traces")).toEqual(["metadata"]);
  expect(mapColumns("spans")).toEqual(["metadata", "attributes", "resource_attributes"]);
  expect(mapColumns("scores")).toEqual(["metadata"]);
});

it("drops the list fields from count_distinct and adds the identifiers", () => {
  expect(countDistinctFields("traces")).toEqual([
    "name",
    "status",
    "environment",
    "release",
    "user_id",
    "trace_id",
    "session_id",
  ]);
  expect(countDistinctFields("spans")).not.toContain("tags");
  expect(countDistinctFields("spans")).not.toContain("models");
  expect(countDistinctFields("spans")).toContain("trace_id");
  expect(countDistinctFields("scores")).toContain("session_id");
});

it("offers the fields each aggregate takes", () => {
  expect(fieldsForAgg("traces", "count")).toEqual([]);
  expect(fieldsForAgg("traces", "rate")).toEqual([]);
  expect(fieldsForAgg("traces", "p95")).toEqual(numericFields("traces"));
  expect(fieldsForAgg("spans", "sum")).toEqual(numericFields("spans"));
  expect(fieldsForAgg("traces", "count_distinct")).toEqual(countDistinctFields("traces"));
});

it("accepts map keys only on the map columns of their source", () => {
  expect(isMapKeyField("traces", "metadata['user']")).toBe(true);
  expect(isMapKeyField("traces", "attributes['k']")).toBe(false);
  expect(isMapKeyField("spans", "resource_attributes['service.name']")).toBe(true);
  expect(isMapKeyField("spans", "metadata")).toBe(false);
});

it("checks the length and the quotes of a map key", () => {
  expect(isMapKeyField("traces", `metadata['${"k".repeat(256)}']`)).toBe(true);
  expect(isMapKeyField("traces", `metadata['${"k".repeat(257)}']`)).toBe(false);
  for (const text of ["metadata['']", "metadata['a'b']", "metadata['a'", "metadata[a]"]) {
    expect(isMapKeyField("traces", text), text).toBe(false);
  }
});

it("classifies fields for formatting", () => {
  expect(fieldKind("duration_ns")).toBe("duration");
  expect(fieldKind("ttft_ns")).toBe("duration");
  expect(fieldKind("cost_total")).toBe("cost");
  expect(fieldKind("value_num")).toBe("score");
  expect(fieldKind("total_tokens")).toBe("count");
  expect(fieldKind("error_count")).toBe("count");
  expect(fieldKind("llm_count")).toBe("count");
  expect(fieldKind("span_count")).toBe("count");
  expect(fieldKind("anything_else")).toBe("number");
});
