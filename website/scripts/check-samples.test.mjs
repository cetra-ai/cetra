import assert from "node:assert/strict";
import { test } from "node:test";
import { checkFile } from "./check-samples.mjs";

const fence = (marker, language, body = "x = 1") =>
  `${marker}${marker ? "\n" : ""}\`\`\`${language}\n${body}\n\`\`\`\n`;

test("marked blocks pass", () => {
  for (const text of [
    fence("{/* test */}", "python"),
    fence("{/* test:otel */}", "python"),
    fence("{/* example:quick-start */}", "ts"),
    fence("{/* untested: needs a GPU */}", "tsx"),
    `{/* test */}\n\n${fence("", "python")}`,
  ]) {
    assert.deepEqual(checkFile("a.mdx", text), []);
  }
});

test("an unmarked python block fails", () => {
  const findings = checkFile("a.mdx", `text\n\n${fence("", "python")}`);
  assert.deepEqual(findings, ["a.mdx:3: python block without a test marker (D-W-6)"]);
});

test("untested without a reason fails", () => {
  assert.equal(checkFile("a.mdx", fence("{/* untested: */}", "js")).length, 1);
});

test("a test marker before a ts fence fails", () => {
  assert.equal(checkFile("a.mdx", fence("{/* test */}", "ts")).length, 1);
  assert.equal(checkFile("a.mdx", fence("{/* example:a */}", "python")).length, 1);
});

test("other fences need nothing", () => {
  for (const language of ["bash", "toml", "yaml", "go", "java"]) {
    assert.deepEqual(checkFile("a.mdx", fence("", language)), []);
  }
});

test("a fence inside a fence is not looked at", () => {
  const text = "````md\n```python\nx = 1\n```\n````\n";
  assert.deepEqual(checkFile("a.mdx", text), []);
});
