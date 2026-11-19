import { describe, expect, it } from "vitest";
import { changedFields, type VersionLike, versionDiff } from "@/lib/evaluator-diff";

/** A version as the API returns it: the fields the diff reads, with the version and its time. */
type Sample = VersionLike & { version: number; created_at: string };

function judge(version: number, prompt: string, extra: Record<string, unknown> = {}): Sample {
  return {
    version,
    name: "Relevance",
    kind: "llm_judge",
    config: { model: "gpt-5-mini", prompt, temperature: null, output: { max: 1 }, ...extra },
    output_score: { name: "relevance", data_type: "numeric" },
    created_at: "2026-01-01T09:00:00.000000000Z",
  };
}

function webhook(version: number): Sample {
  return {
    version,
    name: "Hook",
    kind: "webhook",
    config: { url_host: "hooks.example.com", timeout_ms: 10000, header_names: ["X-Team"] },
    output_score: { name: "hook", data_type: "numeric" },
    created_at: "2026-01-01T09:00:00.000000000Z",
  };
}

describe("changedFields", () => {
  it("reports a nested change as one changed field", () => {
    expect(changedFields({ output: { max: 1, min: 0 } }, { output: { max: 5, min: 0 } })).toEqual([
      { path: "output.max", kind: "changed", before: 1, after: 5 },
    ]);
  });

  it("reports an added key and a removed key", () => {
    expect(changedFields({ a: 1 }, { b: 2 })).toEqual([
      { path: "a", kind: "removed", before: 1, after: undefined },
      { path: "b", kind: "added", before: undefined, after: 2 },
    ]);
  });

  it("treats null to a value as one changed leaf", () => {
    expect(changedFields({ temperature: null }, { temperature: 0.2 })).toEqual([
      { path: "temperature", kind: "changed", before: null, after: 0.2 },
    ]);
  });

  it("treats null to an object as one changed leaf, without recursing", () => {
    expect(changedFields({ output: null }, { output: { max: 1 } })).toEqual([
      { path: "output", kind: "changed", before: null, after: { max: 1 } },
    ]);
  });

  it("treats an array change as one leaf", () => {
    expect(changedFields({ header_names: ["A"] }, { header_names: ["A", "B"] })).toEqual([
      { path: "header_names", kind: "changed", before: ["A"], after: ["A", "B"] },
    ]);
  });

  it("hides a skipped root key but not a nested one with the same name", () => {
    expect(
      changedFields(
        { prompt: "old", output: { prompt: "a" } },
        { prompt: "new", output: { prompt: "b" } },
        ["prompt"],
      ),
    ).toEqual([{ path: "output.prompt", kind: "changed", before: "a", after: "b" }]);
  });

  it("gives nothing for identical values and for two empty objects", () => {
    expect(changedFields({ a: [1] }, { a: [1] })).toEqual([]);
    expect(changedFields({}, {})).toEqual([]);
  });

  it("sorts the result by path", () => {
    const paths = changedFields({ b: 1, a: 1 }, { b: 2, a: 2 }).map((change) => change.path);
    expect(paths).toEqual(["a", "b"]);
  });
});

describe("versionDiff", () => {
  it("shows the first version as added", () => {
    const diff = versionDiff(undefined, judge(1, "Line one\nLine two"));
    expect(diff.name?.kind).toBe("added");
    expect(diff.nothingVisible).toBe(false);
    expect(diff.prompt?.every((line) => line.op === "added")).toBe(true);
  });

  it("reports a webhook pair with identical config as nothing visible", () => {
    expect(versionDiff(webhook(1), webhook(2)).nothingVisible).toBe(true);
  });

  it("reports a judge pair whose only change is the prompt", () => {
    const diff = versionDiff(judge(1, "Old line"), judge(2, "New line"));
    expect(diff.config).toEqual([]);
    expect(diff.output).toEqual([]);
    expect(diff.prompt).toEqual([
      { op: "removed", text: "Old line" },
      { op: "added", text: "New line" },
    ]);
    expect(diff.nothingVisible).toBe(false);
  });

  it("lists a changed config field next to an unchanged prompt", () => {
    const changed = judge(2, "Same", { output: { max: 5 } });
    expect(versionDiff(judge(1, "Same"), changed).config).toEqual([
      { path: "output.max", kind: "changed", before: 1, after: 5 },
    ]);
  });
});
