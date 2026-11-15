import { describe, expect, it } from "vitest";
import { parseJsonField, payloadJson, prettyJson, previewJson } from "./datasets";

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
});

describe("payloadJson", () => {
  it("returns the value of each payload format", () => {
    expect(payloadJson({ format: "messages", value: [] })).toEqual([]);
    expect(payloadJson({ format: "json", value: { a: 1 } })).toEqual({ a: 1 });
    expect(payloadJson({ format: "text", value: "ok" })).toBe("ok");
  });

  it("returns undefined without a payload", () => {
    expect(payloadJson(undefined)).toBeUndefined();
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
