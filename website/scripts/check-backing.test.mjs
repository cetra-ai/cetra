import assert from "node:assert/strict";
import { test } from "node:test";
import { checkFile } from "./check-backing.mjs";

const exists = (relative) =>
  ["openinference/openai-chat-tool", "genai/tool-span"].includes(relative);

test("an existing case has no finding", () => {
  const text = '<Backing fixtures={["openinference/openai-chat-tool"]} />';
  assert.deepEqual(checkFile("p.mdx", text, exists), []);
});

test("a missing case is a finding with its line", () => {
  const text = 'a\n\n<Backing\n  fixtures={[\n    "genai/tool-span",\n    "genai/nope",\n  ]}\n/>';
  assert.deepEqual(checkFile("p.mdx", text, exists), [
    "p.mdx:3: no fixture case fixtures/otlp/genai/nope",
  ]);
});

test("a wildcard or a group alone is a finding", () => {
  const text = '<Backing fixtures={["vercel/*", "genai"]} />';
  assert.equal(checkFile("p.mdx", text, exists).length, 2);
});

test("a box with tests only is fine", () => {
  const text = '<Backing tests={["sdks/python/tests/test_instrument.py::test_x"]} />';
  assert.deepEqual(checkFile("p.mdx", text, exists), []);
});

test("an empty box is a finding", () => {
  assert.equal(checkFile("p.mdx", "<Backing />", exists).length, 1);
});
