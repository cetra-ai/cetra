import assert from "node:assert/strict";
import { test } from "node:test";
import { toMarkdown } from "./markdown.mjs";
import { diffFiles, expectedFiles, header, validateList } from "./skill-references.mjs";

const exists = () => true;

test("validateList accepts a valid list", () => {
  const list = [
    { name: "openai", page: "integrations/openai.mdx" },
    { name: "attributes", page: "reference/attributes.md" },
  ];
  assert.deepEqual(validateList(list, exists), []);
});

test("validateList reports a duplicate name", () => {
  const list = [
    { name: "openai", page: "integrations/openai.mdx" },
    { name: "openai", page: "integrations/openai.mdx" },
  ];
  assert.deepEqual(validateList(list, exists), ["references[1]: duplicate name openai"]);
});

test("validateList reports a name with an underscore or uppercase", () => {
  const list = [
    { name: "open_ai", page: "integrations/openai.mdx" },
    { name: "OpenAI", page: "integrations/openai.mdx" },
  ];
  const errors = validateList(list, exists);
  assert.equal(errors.length, 2);
  assert.match(errors[0], /^references\[0\]: name "open_ai" must match /);
  assert.match(errors[1], /^references\[1\]: name "OpenAI" must match /);
});

test("validateList reports a page without .md or .mdx", () => {
  const list = [{ name: "openai", page: "integrations/openai.html" }];
  assert.deepEqual(validateList(list, exists), [
    'references[0]: page "integrations/openai.html" must end in .md or .mdx',
  ]);
});

test("validateList reports a page that does not exist", () => {
  const list = [{ name: "openai", page: "integrations/openai.mdx" }];
  assert.deepEqual(
    validateList(list, () => false),
    ["references[0]: page integrations/openai.mdx does not exist"],
  );
});

test("expectedFiles returns the header plus the converted page", () => {
  const source = "---\ntitle: OpenAI\ndescription: Trace OpenAI calls.\n---\n\nSome text.\n";
  const files = expectedFiles([{ name: "openai", page: "integrations/openai.mdx" }], () => source);
  assert.deepEqual(
    [...files.entries()],
    [["openai.md", header("integrations/openai.mdx") + toMarkdown(source)]],
  );
});

test("expectedFiles names the page when the converter fails", () => {
  const source = "---\ntitle: OpenAI\n---\n\n<Unknown>text</Unknown>\n";
  assert.throws(
    () => expectedFiles([{ name: "openai", page: "integrations/openai.mdx" }], () => source),
    /^Error: integrations\/openai\.mdx: /,
  );
});

test("diffFiles reports stale, missing and extra files, sorted", () => {
  const expected = new Map([
    ["b.md", "B"],
    ["a.md", "A"],
    ["c.md", "C"],
  ]);
  const actual = new Map([
    ["a.md", "old"],
    ["d.md", "D"],
    ["c.md", "C"],
  ]);
  assert.deepEqual(diffFiles(expected, actual), [
    "skills/cetra/references/a.md is stale: run `just docs-gen`",
    "skills/cetra/references/b.md is missing: run `just docs-gen`",
    "skills/cetra/references/d.md is not in skills/references.json: run `just docs-gen`",
  ]);
});

test("diffFiles returns no messages for equal maps", () => {
  const expected = new Map([["a.md", "A"]]);
  assert.deepEqual(diffFiles(expected, new Map([["a.md", "A"]])), []);
});

test("header names the page and the generator", () => {
  assert.equal(
    header("integrations/openai.mdx"),
    "<!-- Generated from website/src/content/docs/docs/integrations/openai.mdx by website/scripts/skill-references.mjs. Do not edit: run `just docs-gen`. -->\n\n",
  );
});
