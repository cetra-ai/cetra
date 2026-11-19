import { CompletionContext } from "@codemirror/autocomplete";
import { EditorState } from "@codemirror/state";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import {
  columnMap,
  completeFilter,
  errorMarkRange,
  facetTarget,
  listTokens,
  listValueText,
  valueSlot,
} from "@/components/filter-bar/logic";

const schema = {
  views: [
    {
      name: "traces",
      description: "Traces.",
      columns: [
        {
          name: "status",
          column_type: "string",
          nullable: false,
          description: "Span status.",
        },
        {
          name: "cost_total",
          column_type: "float",
          nullable: true,
          description: "Sum of span costs, USD.",
        },
        {
          name: "tags",
          column_type: "string_list",
          nullable: false,
          description: "Tags.",
        },
        {
          name: "metadata",
          column_type: "string_map",
          nullable: false,
          description: "Metadata.",
        },
      ],
    },
  ],
  functions: [
    {
      name: "score",
      signature: "score(name: string) -> float",
      description: "Latest numeric score.",
      in_filter: true,
      in_sql: false,
    },
    {
      name: "lower",
      signature: "lower(value: string) -> string",
      description: "Lowercase a string.",
      in_filter: true,
      in_sql: true,
    },
    {
      name: "now",
      signature: "now() -> timestamp",
      description: "Current time.",
      in_filter: false,
      in_sql: true,
    },
  ],
} as Schema<"SchemaResponse">;

function at(text: string) {
  return new CompletionContext(EditorState.create({ doc: text }), text.length, false);
}

function labels(text: string): string[] {
  return completeFilter(at(text), "traces", schema)?.options.map((option) => option.label) ?? [];
}

it("offers columns and in-filter functions from the schema", () => {
  const result = completeFilter(at("cos"), "traces", schema);
  const cost = result?.options.find((option) => option.label === "cost_total");
  expect(cost?.detail).toBe("float");
  expect(cost?.info).toBe("Sum of span costs, USD.");
  const score = completeFilter(at("sc"), "traces", schema)?.options.find(
    (option) => option.label === "score",
  );
  expect(score?.detail).toBe("score(name: string) -> float");
  expect(labels("sc")).not.toContain("now");
});

it("offers operators for the column before the cursor", () => {
  expect(labels("status ")).toEqual(expect.arrayContaining(["=", "LIKE", "IS NULL", "IN"]));
  expect(labels("status ")).not.toContain("@>");
  expect(labels("tags ")).toEqual(["@>"]);
  expect(labels("metadata[")).toEqual(["'"]);
});

it("marks a filter error from the reported column through the token", () => {
  expect(errorMarkRange("nme = 1", 1)).toEqual({ from: 0, to: 3 });
  expect(errorMarkRange("nme = 1", 4)).toEqual({ from: 3, to: 4 });
});

it("finds the value position after a comparison, a list or a list bracket", () => {
  const columns = columnMap(schema, "traces");
  const at = (text: string) => valueSlot(text, text.length, "traces", columns);
  expect(at("status = ")).toMatchObject({
    field: { column: "status" },
    inList: false,
    prefix: "",
    from: 9,
  });
  expect(at("status != ")?.field).toMatchObject({ column: "status" });
  expect(at("status = 'er")).toMatchObject({
    field: { column: "status" },
    inList: false,
    prefix: "er",
    from: 9,
  });
  expect(at("status = er")).toMatchObject({ prefix: "er", from: 9 });
  expect(at("status = 'it''s")).toMatchObject({ prefix: "it's", from: 9 });
  expect(at("status IN (")).toMatchObject({
    field: { column: "status" },
    inList: true,
    prefix: "",
    from: 11,
  });
  expect(at("status NOT IN ('a', 'b")).toMatchObject({
    field: { column: "status" },
    inList: true,
    prefix: "b",
    from: 20,
  });
  expect(at("tags @> [")).toMatchObject({ field: { column: "tags" }, inList: true, from: 9 });
  expect(at("metadata['env'] = 'pr")).toMatchObject({
    field: { column: "metadata", key: "env" },
    inList: false,
    prefix: "pr",
    from: 18,
  });
});

it("finds no value position elsewhere", () => {
  const columns = columnMap(schema, "traces");
  const none = (text: string) => valueSlot(text, text.length, "traces", columns);
  expect(none("status")).toBeNull();
  expect(none("status = 'a'")).toBeNull();
  expect(none("status = 'a' AND ")).toBeNull();
  expect(none("cost_total = 1 ")).toBeNull();
  expect(none("status < ")).toBeNull();
  expect(none("nope = ")).toBeNull();
  expect(none("score('x') = ")).toBeNull();
  expect(none("status IN ('a' ")).toBeNull();
});

it("uses the cursor position, not the end of the text", () => {
  const columns = columnMap(schema, "traces");
  expect(valueSlot("status = 'ok' AND cost_total > 1", 9, "traces", columns)).toMatchObject({
    field: { column: "status" },
    prefix: "",
  });
});

it("says which fields a value is suggested for", () => {
  expect(facetTarget("traces", { column: "status" })).toBe("values");
  expect(facetTarget("traces", { column: "tags" })).toBe("list");
  expect(facetTarget("traces", { column: "has_root" })).toBe("bool");
  expect(facetTarget("traces", { column: "metadata", key: "env" })).toBe("values");
  expect(facetTarget("traces", { column: "metadata", key: "" })).toBeUndefined();
  expect(facetTarget("traces", { column: "metadata" })).toBeUndefined();
  expect(facetTarget("traces", { column: "trace_id" })).toBeUndefined();
  expect(facetTarget("traces", { column: "duration_ms" })).toBeUndefined();
});

it("reads and writes the tokens of a list draft", () => {
  expect(listTokens("IN", "'a', 'b''c'")).toEqual(["a", "b'c"]);
  expect(listTokens("IN", "1, 2")).toEqual(["1", "2"]);
  expect(listTokens("@>", "'x'")).toEqual(["x"]);
  expect(listTokens("NOT IN", "")).toEqual([]);
  expect(listValueText(["a", "b'c"], true)).toBe("'a', 'b''c'");
  expect(listValueText(["true"], false)).toBe("true");
  expect(listTokens("IN", listValueText(["a", "b'c"], true))).toEqual(["a", "b'c"]);
});

it("offers no columns inside a list of values", () => {
  expect(labels("status IN (")).toEqual([]);
  expect(labels("status = ")).toEqual([]);
});

it("takes a list field's value only inside @> [", () => {
  const columns = columnMap(schema, "traces");
  expect(valueSlot("tags = ", 7, "traces", columns)).toBeNull();
  expect(valueSlot("tags != ", 8, "traces", columns)).toBeNull();
  expect(valueSlot("tags IN (", 8, "traces", columns)).toBeNull();
  expect(valueSlot("tags @> ['a', ", 13, "traces", columns)).toMatchObject({
    field: { column: "tags" },
    inList: true,
  });
});

it("takes no value on a field the view cannot facet", () => {
  const columns = columnMap(schema, "traces");
  expect(valueSlot("cost_total = ", 13, "traces", columns)).toBeNull();
  expect(valueSlot("cost_total IN (", 15, "traces", columns)).toBeNull();
});

it("accepts <> and lower-case not in as value positions", () => {
  const columns = columnMap(schema, "traces");
  expect(valueSlot("status <> ", 10, "traces", columns)).toMatchObject({
    field: { column: "status" },
    from: 10,
    to: 10,
  });
  expect(valueSlot("status not in (", 16, "traces", columns)).toMatchObject({
    field: { column: "status" },
    inList: true,
  });
});

it("replaces a closed string through its closing quote", () => {
  const columns = columnMap(schema, "traces");
  const text = "status = 'abc'";
  expect(valueSlot(text, 12, "traces", columns)).toEqual({
    field: { column: "status" },
    inList: false,
    prefix: "ab",
    from: 9,
    to: 14,
  });
  expect(valueSlot(text, 14, "traces", columns)).toBeNull();
});

it("keeps the columns of a field the view cannot facet inside a list (typed prefix)", () => {
  expect(labels("cost_total IN (c")).toContain("cost_total");
});
