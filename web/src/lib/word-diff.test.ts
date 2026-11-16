import { describe, expect, it } from "vitest";
import { type DiffPart, MAX_DIFF_WORDS, tokenize, wordDiff } from "./word-diff";

function join(parts: DiffPart[], keep: DiffPart["kind"][]): string {
  return parts
    .filter((part) => keep.includes(part.kind))
    .map((part) => part.text)
    .join("");
}

function words(count: number, change?: { index: number; word: string }): string {
  return Array.from({ length: count }, (_, index) =>
    change?.index === index ? change.word : `w${index}`,
  ).join(" ");
}

describe("tokenize", () => {
  it("keeps the text when the tokens are joined", () => {
    for (const text of ["", "  lead and  trail  ", "one\ntwo\tthree ", "x"]) {
      expect(tokenize(text).join("")).toBe(text);
    }
  });
});

describe("wordDiff", () => {
  it("returns one same part for equal texts", () => {
    expect(wordDiff("the whole text", "the whole text")).toEqual([
      { kind: "same", text: "the whole text" },
    ]);
  });

  it("marks a replaced word as removed then added", () => {
    expect(wordDiff("the cat sat", "the dog sat")).toEqual([
      { kind: "same", text: "the " },
      { kind: "removed", text: "cat " },
      { kind: "added", text: "dog " },
      { kind: "same", text: "sat" },
    ]);
  });

  it("rebuilds both sides from its parts", () => {
    const pairs: [string, string][] = [
      ["", ""],
      ["", "only added here"],
      ["only removed here", ""],
      ["a b c d e f", "a x c y e f g"],
    ];
    for (const [before, after] of pairs) {
      const parts = wordDiff(before, after);
      expect(parts).toBeDefined();
      expect(join(parts ?? [], ["same", "removed"])).toBe(before);
      expect(join(parts ?? [], ["same", "added"])).toBe(after);
    }
  });

  it("returns undefined above the word limit on either side", () => {
    const over = words(MAX_DIFF_WORDS + 1);
    expect(wordDiff(over, "short")).toBeUndefined();
    expect(wordDiff("short", over)).toBeUndefined();
  });

  it("diffs a long text with one changed word quickly", () => {
    const before = words(MAX_DIFF_WORDS);
    const after = words(MAX_DIFF_WORDS, { index: 1000, word: "changed" });
    const started = performance.now();
    const parts = wordDiff(before, after);
    const elapsed = performance.now() - started;
    expect(elapsed).toBeLessThan(1000);
    expect(parts?.filter((part) => part.kind === "removed")).toHaveLength(1);
    expect(parts?.filter((part) => part.kind === "added")).toHaveLength(1);
  });
});
