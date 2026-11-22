import assert from "node:assert/strict";
import { test } from "node:test";
import { outputPath } from "./md-pages.mjs";

test("outputPath writes an index page to the root of its directory", () => {
  assert.equal(outputPath("index.mdx"), "index.md");
  assert.equal(outputPath("a/index.mdx"), "a/index.md");
});

test("outputPath gives a page its own directory with an index.md", () => {
  assert.equal(outputPath("a/b.md"), "a/b/index.md");
  assert.equal(outputPath("a/b.mdx"), "a/b/index.md");
  assert.equal(outputPath("reference/cli.md"), "reference/cli/index.md");
});
