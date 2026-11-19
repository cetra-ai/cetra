import assert from "node:assert/strict";
import { test } from "node:test";
import { checkFile } from "./check-terms.mjs";

const front = "---\ntitle: T\ndescription: A page.\n---\n";
const check = (body) => checkFile("p.mdx", front + body);

for (const [word, label] of [
  ["llmobs", "old name"],
  ["Cetra", "capitalised name"],
  ["CETRA", "upper-case name"],
  ["seamless", "marketing word"],
  ["powerful", "marketing word"],
  ["simply", "simply"],
  ["Open Telemetry", "split OpenTelemetry"],
  ["otel", "otel"],
  ["on-prem", "on-prem"],
  ["on-premise", "on-premise"],
]) {
  test(`finds ${label} outside a fence`, () => {
    const f = check(`Use ${word} here.\n`);
    assert.equal(f.length, 1);
    assert.match(f[0], /^p\.mdx:5: /);
  });
  test(`ignores ${label} inside a fence`, () => {
    assert.deepEqual(check(`\`\`\`bash\nuse ${word} here\n\`\`\`\n`), []);
  });
}

test("allows Cetra inside backticks", () => {
  assert.deepEqual(check("The `Cetra` class.\n"), []);
});

test("finds a missing description", () => {
  const f = checkFile("p.mdx", "---\ntitle: T\n---\nText.\n");
  assert.equal(f.length, 1);
  assert.match(f[0], /missing description/);
});

test("finds an over-long description", () => {
  const f = checkFile("p.mdx", `---\ntitle: T\ndescription: ${"x".repeat(161)}\n---\n`);
  assert.equal(f.length, 1);
  assert.match(f[0], /161 characters/);
});

test("a clean file has no finding", () => {
  assert.deepEqual(check("cetra records traces.\n"), []);
});

test("ignores an MDX sample marker comment", () => {
  assert.deepEqual(check("{/* test:otel */}\n"), []);
});
