import { describe, expect, it } from "vitest";
import {
  defaultTarget,
  importFormatOf,
  itemShortId,
  mappingProblems,
  parseDatasetSearch,
  parseJsonField,
  prettyJson,
  previewFields,
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
    expect(parseDatasetSearch({ tab: "experiments" })).toEqual({ tab: "experiments" });
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

  it("keeps a non-empty item id and drops an empty or non-string one", () => {
    expect(parseDatasetSearch({ item: "abc" })).toEqual({ item: "abc" });
    expect(parseDatasetSearch({ item: "" })).toEqual({});
    expect(parseDatasetSearch({ item: 3 })).toEqual({});
  });

  it("trims the item search text and keeps it when it is 1 to 200 characters", () => {
    expect(parseDatasetSearch({ q: "  refund  " })).toEqual({ q: "refund" });
    expect(parseDatasetSearch({ q: "x".repeat(200) })).toEqual({ q: "x".repeat(200) });
    // Characters, as the server counts them: an emoji is one character.
    expect(parseDatasetSearch({ q: "😀".repeat(200) })).toEqual({ q: "😀".repeat(200) });
  });

  it("drops a blank, too long or non-string item search text", () => {
    expect(parseDatasetSearch({ q: "   " })).toEqual({});
    expect(parseDatasetSearch({ q: "" })).toEqual({});
    expect(parseDatasetSearch({ q: "x".repeat(201) })).toEqual({});
    expect(parseDatasetSearch({ q: "😀".repeat(201) })).toEqual({});
    expect(parseDatasetSearch({ q: 3 })).toEqual({});
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

describe("previewFields", () => {
  it("shows a string on one line, whitespace collapsed", () => {
    expect(previewFields("  hello \n   world")).toBe("hello world");
  });

  it("drops the Markdown markers that start a line", () => {
    expect(previewFields("## Question\n\n- Check `id`\n> quoted\n1. first")).toBe(
      "Question Check `id` quoted first",
    );
  });

  it("shows a blank top-level value as a dash", () => {
    expect(previewFields("   ")).toBe("—");
    expect(previewFields(null)).toBe("—");
    expect(previewFields(undefined)).toBe("—");
  });

  it("shows a top-level number or boolean as JSON", () => {
    expect(previewFields(3)).toBe("3");
    expect(previewFields(true)).toBe("true");
  });

  it("shows top-level keys as key: value pairs joined by the separator", () => {
    expect(previewFields({ q: "why?", n: 2 })).toBe("q: why? · n: 2");
  });

  it("shows nested objects as {…}, nested arrays as [n] and nested null as null", () => {
    expect(previewFields({ a: { x: 1 }, b: [1, 2, 3], c: null })).toBe("a: {…} · b: [3] · c: null");
  });

  it("shows empty objects and arrays", () => {
    expect(previewFields({})).toBe("{}");
    expect(previewFields([])).toBe("[0]");
    expect(previewFields({ a: {}, b: [] })).toBe("a: {} · b: [0]");
  });

  it("quotes a string field only when it is empty or contains the separator", () => {
    expect(previewFields({ a: "x · y" })).toBe('a: "x · y"');
    expect(previewFields({ a: "" })).toBe('a: ""');
    expect(previewFields({ a: "plain" })).toBe("a: plain");
  });

  it("joins the lines of a string field", () => {
    expect(previewFields({ a: "line one\nline two" })).toBe("a: line one line two");
  });

  it("shows a top-level chat as the last user message", () => {
    const chat = [
      { role: "system", content: "be brief" },
      { role: "user", content: "hi\nthere" },
      { role: "assistant", content: "yo" },
    ];
    expect(previewFields(chat)).toBe("hi there");
  });

  it("shows a chat under a key as the last user message", () => {
    const messages = [
      { role: "user", content: "first" },
      { role: "assistant", content: "answer" },
      { role: "user", content: "second" },
    ];
    expect(previewFields({ messages })).toBe("messages: second");
  });

  it("shows the last string content of a chat without a user message", () => {
    expect(previewFields([{ role: "assistant", content: "a" }])).toBe("a");
  });

  it("shows a chat without string content as its length", () => {
    expect(previewFields([{ role: "user", content: { x: 1 } }])).toBe("[1]");
  });

  it("shows a top-level array as its length and first element", () => {
    expect(previewFields(["a", "b"])).toBe("[2] a");
    expect(previewFields([{ q: "x" }, { q: "y" }])).toBe("[2] q: x");
  });

  it("cuts the preview to max characters, the ellipsis included", () => {
    const preview = previewFields("x".repeat(200), 10);
    expect(preview).toBe(`${"x".repeat(9)}…`);
    expect(previewFields({ a: "y".repeat(300) })).toHaveLength(160);
  });
});

describe("itemShortId", () => {
  it("shows the last 8 hex digits", () => {
    expect(itemShortId("0190a1b2-0000-7000-8000-00000000abcd")).toBe("0000abcd");
  });
});
