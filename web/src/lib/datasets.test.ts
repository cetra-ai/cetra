import { describe, expect, it } from "vitest";
import {
  defaultTarget,
  importFormatOf,
  mappingProblems,
  parseDatasetSearch,
  parseJsonField,
  prettyJson,
  previewJson,
  sameJson,
} from "./datasets";

describe("parseJsonField", () => {
  it("accepts an empty field when it is not required", () => {
    expect(parseJsonField("", { required: false })).toEqual({ ok: true, value: undefined });
  });

  it("requires a non-blank field when required", () => {
    expect(parseJsonField("  ", { required: true })).toEqual({ ok: false, message: "Required" });
  });

  it("rejects text that is not JSON", () => {
    expect(parseJsonField("{", { required: true })).toEqual({
      ok: false,
      message: "Not valid JSON",
    });
  });

  it("requires a JSON object when asked", () => {
    expect(parseJsonField("[1]", { required: true, object: true })).toEqual({
      ok: false,
      message: "Must be a JSON object",
    });
    expect(parseJsonField("null", { required: true, object: true })).toEqual({
      ok: false,
      message: "Must be a JSON object",
    });
    expect(parseJsonField('{"a":1}', { required: true, object: true })).toEqual({
      ok: true,
      value: { a: 1 },
    });
  });

  it("accepts any JSON value when no object is asked", () => {
    expect(parseJsonField('"hi"', { required: true })).toEqual({ ok: true, value: "hi" });
  });
});

describe("previewJson", () => {
  it("shows a string as itself", () => {
    expect(previewJson("hello")).toBe("hello");
  });

  it("shows other values as compact JSON", () => {
    expect(previewJson({ a: 1 })).toBe('{"a":1}');
  });

  it("cuts a long value with an ellipsis", () => {
    const long = "x".repeat(200);
    expect(previewJson(long, 10)).toBe(`${"x".repeat(10)}…`);
  });

  it("shows a missing value as a dash", () => {
    expect(previewJson(undefined)).toBe("—");
  });

  it("cuts a large object without serializing all of it", () => {
    const large = { rows: Array.from({ length: 100_000 }, (_, index) => ({ index })) };
    const preview = previewJson(large, 200);
    expect(preview.length).toBeLessThanOrEqual(201);
    expect(preview.endsWith("…")).toBe(true);
    expect(preview.startsWith('{"rows":[{"index":0},{"index":1}')).toBe(true);
  });

  it("cuts a large array with an ellipsis", () => {
    const preview = previewJson(
      Array.from({ length: 10_000 }, () => "abcdef"),
      50,
    );
    expect(preview.length).toBeLessThanOrEqual(51);
    expect(preview.endsWith("…")).toBe(true);
  });

  it("shows a short object exactly as JSON.stringify does", () => {
    const value = { a: [1, null, "two", true], b: { c: undefined, d: "e" } };
    expect(previewJson(value, 1000)).toBe(JSON.stringify(value));
  });

  it("follows JSON.stringify for values JSON cannot hold", () => {
    const value = { when: new Date(0), skipped: () => 1, list: [undefined, () => 1] };
    expect(previewJson(value, 1000)).toBe(JSON.stringify(value));
  });

  it("does not stringify a long string", () => {
    expect(previewJson("y".repeat(5000), 3)).toBe("yyy…");
  });
});

describe("prettyJson", () => {
  it("is empty for undefined", () => {
    expect(prettyJson(undefined)).toBe("");
  });

  it("indents two spaces", () => {
    expect(prettyJson({ a: 1 })).toBe('{\n  "a": 1\n}');
  });
});

describe("sameJson", () => {
  it("compares the JSON text of two values", () => {
    expect(sameJson({ a: 1 }, { a: 1 })).toBe(true);
    expect(sameJson({ a: 1 }, { a: 2 })).toBe(false);
    expect(sameJson(undefined, undefined)).toBe(true);
    expect(sameJson(undefined, {})).toBe(false);
  });
});

describe("parseDatasetSearch", () => {
  it("keeps a known tab and drops an unknown one", () => {
    expect(parseDatasetSearch({ tab: "versions" })).toEqual({ tab: "versions" });
    expect(parseDatasetSearch({ tab: "x" })).toEqual({});
  });

  it("parses a version given as a string or a number", () => {
    expect(parseDatasetSearch({ version: "3" })).toEqual({ version: 3 });
    expect(parseDatasetSearch({ version: 3 })).toEqual({ version: 3 });
  });

  it("drops a version that is not a non-negative integer", () => {
    expect(parseDatasetSearch({ version: "-1" })).toEqual({});
    expect(parseDatasetSearch({ version: "1.5" })).toEqual({});
    expect(parseDatasetSearch({ version: "a" })).toEqual({});
  });
});

describe("importFormatOf", () => {
  it("reads the format from the extension, case-insensitively", () => {
    expect(importFormatOf("a.CSV")).toBe("csv");
    expect(importFormatOf("b.jsonl")).toBe("jsonl");
    expect(importFormatOf("c.ndjson")).toBe("jsonl");
    expect(importFormatOf("d.json")).toBeUndefined();
  });
});

describe("defaultTarget", () => {
  it("maps the well-known column names", () => {
    expect(defaultTarget("Input")).toBe("input");
    expect(defaultTarget("expected_output")).toBe("expected");
    expect(defaultTarget("metadata")).toBe("ignore");
    expect(defaultTarget("question")).toBe("input.question");
  });
});

describe("mappingProblems", () => {
  it("is empty for a valid mapping", () => {
    expect(
      mappingProblems([
        { column: "q", target: "input.q" },
        { column: "a", target: "expected" },
      ]),
    ).toEqual([]);
  });

  it("asks for an input", () => {
    expect(mappingProblems([{ column: "a", target: "expected" }])).toEqual([
      "Map at least one column to the input",
    ]);
  });

  it("refuses the whole input and its keys together", () => {
    expect(
      mappingProblems([
        { column: "x", target: "input" },
        { column: "q", target: "input.q" },
      ]),
    ).toEqual(["Map the whole input or input keys, not both"]);
  });

  it("refuses the whole expected output and its keys together", () => {
    expect(
      mappingProblems([
        { column: "q", target: "input.q" },
        { column: "a", target: "expected" },
        { column: "b", target: "expected.b" },
      ]),
    ).toEqual(["Map the whole expected output or its keys, not both"]);
  });

  it("names a target that two columns use", () => {
    expect(
      mappingProblems([
        { column: "q", target: "input" },
        { column: "r", target: "input" },
      ]),
    ).toEqual(["Two columns map to input"]);
  });

  it("names a keyed target with an empty key", () => {
    expect(
      mappingProblems([
        { column: "q", target: "input." },
        { column: "a", target: "expected" },
      ]),
    ).toEqual(["q: the key is empty"]);
  });

  it("flags a key that is only whitespace", () => {
    expect(
      mappingProblems([
        { column: "q", target: "input.   " },
        { column: "a", target: "expected" },
      ]),
    ).toEqual(["q: the key is empty"]);
  });

  it("flags a key longer than 200 characters", () => {
    expect(
      mappingProblems([
        { column: "q", target: `input.${"k".repeat(201)}` },
        { column: "a", target: "expected" },
      ]),
    ).toEqual(["q: the key is longer than 200 characters"]);
  });

  it("accepts a key of exactly 200 characters", () => {
    expect(
      mappingProblems([
        { column: "q", target: `input.${"k".repeat(200)}` },
        { column: "a", target: "expected" },
      ]),
    ).toEqual([]);
  });

  it("counts characters, not UTF-16 units", () => {
    expect(
      mappingProblems([
        { column: "q", target: `input.${"é".repeat(200)}` },
        { column: "a", target: "expected" },
      ]),
    ).toEqual([]);
  });
});
