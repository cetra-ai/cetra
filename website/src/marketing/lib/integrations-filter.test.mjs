import assert from "node:assert/strict";
import { test } from "node:test";
import { LANGUAGE_CHIPS } from "../data/integrations.mjs";
import { filterIntegrations, LANGUAGE_FILTERS } from "./integrations-filter.mjs";

/** A fixed list covering the shapes the filter distinguishes. */
const items = [
  { group: "Providers", title: "Bedrock", description: "hosted models", languages: ["python"] },
  {
    group: "Frameworks",
    title: "Vercel AI",
    description: "React hooks",
    languages: ["typescript"],
  },
  {
    group: "Frameworks",
    title: "LangGraph",
    description: "agent graphs",
    languages: ["python", "typescript"],
  },
  { group: "Frameworks", title: "Koog", description: "Kotlin agents", languages: ["kotlin"] },
  { group: "Frameworks", title: "Spring AI", description: "Java framework", languages: ["java"] },
  { group: "Coding agents", title: "Claude Code", description: "terminal agent", languages: [] },
];

const run = (filter) =>
  filterIntegrations(items, { group: null, language: "any", query: "", ...filter });

test("no filter shows everything in order", () => {
  assert.deepEqual(run({}), { matched: [0, 1, 2, 3, 4, 5], anyLanguage: [] });
});

test("a group filter keeps that group only", () => {
  assert.deepEqual(run({ group: "Frameworks" }), { matched: [1, 2, 3, 4], anyLanguage: [] });
});

test("the search matches title, description and group, case-insensitively", () => {
  assert.deepEqual(run({ query: "BEDROCK" }).matched, [0]); // title
  assert.deepEqual(run({ query: "hooks" }).matched, [1]); // description
  assert.deepEqual(run({ query: "FRAMEWORKS" }).matched, [1, 2, 3, 4]); // group
});

test("every search word must match", () => {
  assert.deepEqual(run({ query: "spring java" }).matched, [4]);
});

test("blank and whitespace-only searches match everything", () => {
  assert.deepEqual(run({ query: "" }).matched, [0, 1, 2, 3, 4, 5]);
  assert.deepEqual(run({ query: "   " }).matched, [0, 1, 2, 3, 4, 5]);
});

test("a language filter moves language-free items to anyLanguage", () => {
  assert.deepEqual(run({ language: "python" }), { matched: [0, 2], anyLanguage: [5] });
});

test("JVM matches Java and Kotlin", () => {
  assert.deepEqual(run({ language: "jvm" }), { matched: [3, 4], anyLanguage: [5] });
});

test("an item in both Python and TypeScript matches either filter", () => {
  assert.deepEqual(run({ language: "python" }).matched, [0, 2]);
  assert.deepEqual(run({ language: "typescript" }).matched, [1, 2]);
});

test("group, language and search combine", () => {
  assert.deepEqual(run({ group: "Coding agents", language: "python", query: "agent" }), {
    matched: [],
    anyLanguage: [5],
  });
});

test("under 'any', anyLanguage is empty", () => {
  assert.deepEqual(run({ query: "agent" }), { matched: [2, 3, 5], anyLanguage: [] });
});

test("an unknown language id throws", () => {
  assert.throws(() => run({ language: "ruby" }), /unknown language filter: ruby/);
});

test("the language ids are unique and 'any' comes first", () => {
  const ids = LANGUAGE_FILTERS.map((entry) => entry.id);
  assert.equal(ids[0], "any");
  assert.equal(new Set(ids).size, ids.length);
});

test("every filtered language has a chip", () => {
  const chips = new Set(Object.keys(LANGUAGE_CHIPS));
  for (const { matches } of LANGUAGE_FILTERS) {
    for (const value of matches) {
      assert.ok(chips.has(value), value);
    }
  }
  for (const chip of chips) {
    assert.ok(
      LANGUAGE_FILTERS.some(({ matches }) => matches.includes(chip)),
      chip,
    );
  }
});
