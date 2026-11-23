import assert from "node:assert/strict";
import { test } from "node:test";
import { checkIndex } from "./check-integrations.mjs";

const origins = {
  "openinference/groq-chat": "captured",
  "vercel/v4-args-result": "hand_written",
};
const caseOrigin = (name) => origins[name] ?? null;

const row = (slug, tier) => `| [Title](/docs/integrations/${slug}/) | ✓ | — | How | ${tier} |`;
const index = (...rows) => ["## Section", "", "| A | B |", "|---|---|", ...rows].join("\n");
const captured = '<Backing fixtures={["openinference/groq-chat"]} />';
const handWritten = '<Backing fixtures={["vercel/v4-args-result"]} />';
const sdkTest = '<Backing tests={["sdks/python/tests/test_instrument.py::test_x"]} />';

test("a captured row with a captured fixture has no finding", () => {
  assert.deepEqual(checkIndex(index(row("p", "captured")), { p: captured }, caseOrigin), []);
});

test("a captured row whose fixture is hand-written is a finding", () => {
  const findings = checkIndex(index(row("p", "captured")), { p: handWritten }, caseOrigin);
  assert.deepEqual(findings, ['index.mdx:5: p is "captured" but its page does not show that']);
});

test("a setup-only row whose page has a box is a finding", () => {
  const findings = checkIndex(index(row("p", "setup only")), { p: captured }, caseOrigin);
  assert.equal(findings.length, 1);
  assert.match(findings[0], /is "setup only"/);
});

test("an SDK test row without tests is a finding", () => {
  const findings = checkIndex(index(row("p", "SDK test")), { p: captured }, caseOrigin);
  assert.equal(findings.length, 1);
  assert.match(findings[0], /is "SDK test"/);
});

test("two parts with languages both pass", () => {
  const page = `${captured}\n${sdkTest}`;
  assert.deepEqual(
    checkIndex(
      index(row("p", "captured (Python), SDK test (TypeScript)")),
      { p: page },
      caseOrigin,
    ),
    [],
  );
});

test("an unknown tier is a finding", () => {
  const findings = checkIndex(index(row("p", "verified")), { p: captured }, caseOrigin);
  assert.deepEqual(findings, ['index.mdx:5: unknown tier "verified" for p']);
});

test("a page without a row is a finding", () => {
  const findings = checkIndex(index(), { p: captured }, caseOrigin);
  assert.deepEqual(findings, ["p.mdx:1: has 0 rows in the index, expected 1"]);
});

test("a row without a page is a finding", () => {
  const findings = checkIndex(index(row("ghost", "setup only")), {}, caseOrigin);
  assert.deepEqual(findings, ["index.mdx:5: no page for ghost"]);
});
