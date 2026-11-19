import { describe, expect, it } from "vitest";
import { diffLines } from "@/lib/line-diff";

const ops = (before: string, after: string) => diffLines(before, after).map((line) => line.op);

describe("diffLines", () => {
  it("marks identical texts all same", () => {
    expect(ops("a\nb", "a\nb")).toEqual(["same", "same"]);
  });

  it("shows one changed middle line as removed then added", () => {
    expect(diffLines("a\nold\nc", "a\nnew\nc")).toEqual([
      { op: "same", text: "a" },
      { op: "removed", text: "old" },
      { op: "added", text: "new" },
      { op: "same", text: "c" },
    ]);
  });

  it("shows an appended line at the end", () => {
    expect(diffLines("a", "a\nb")).toEqual([
      { op: "same", text: "a" },
      { op: "added", text: "b" },
    ]);
  });

  it("shows a deleted line at the start", () => {
    expect(diffLines("a\nb", "b")).toEqual([
      { op: "removed", text: "a" },
      { op: "same", text: "b" },
    ]);
  });

  it("gives nothing for two empty texts", () => {
    expect(diffLines("", "")).toEqual([]);
  });

  it("adds one line for an empty text against one line", () => {
    expect(diffLines("", "a")).toEqual([{ op: "added", text: "a" }]);
  });

  it("splits on newlines, so a trailing newline is an empty line", () => {
    // "a\n" is two lines: "a" and "".
    expect(diffLines("a\n", "a")).toEqual([
      { op: "same", text: "a" },
      { op: "removed", text: "" },
    ]);
  });

  it("lists removed lines before added lines inside a changed block", () => {
    expect(diffLines("x\ny\nz", "p\nq\nz")).toEqual([
      { op: "removed", text: "x" },
      { op: "removed", text: "y" },
      { op: "added", text: "p" },
      { op: "added", text: "q" },
      { op: "same", text: "z" },
    ]);
  });

  it("does not build a table for two unrelated large texts", () => {
    const before = Array.from({ length: 2500 }, (_, index) => `before ${index}`).join("\n");
    const after = Array.from({ length: 2500 }, (_, index) => `after ${index}`).join("\n");
    const started = performance.now();
    const lines = diffLines(before, after);
    expect(performance.now() - started).toBeLessThan(1000);
    expect(lines).toHaveLength(5000);
    expect(lines.slice(0, 2500).every((line) => line.op === "removed")).toBe(true);
    expect(lines.slice(2500).every((line) => line.op === "added")).toBe(true);
  });
});
