import assert from "node:assert/strict";
import { test } from "node:test";
import {
  COMPETITORS,
  formatDate,
  listFiles,
  loadProduct,
  strengthProblems,
  validate,
} from "./compare.mjs";
import { formatUnits, OVERAGE_PER_100K, PLANS } from "./pricing.mjs";

const DAY = 86_400_000;
const SLUGS = ["cetra", ...COMPETITORS];
const today = () => new Date();

/** A fresh copy of a valid competitor file, to break in one way per test. */
const langfuse = () => structuredClone(loadProduct("langfuse"));

test("the files are exactly cetra plus the nine competitors", () => {
  assert.deepEqual(listFiles(), SLUGS.map((s) => `${s}.json`).sort());
});

test("every file is valid today", () => {
  for (const slug of SLUGS) {
    assert.deepEqual(validate(loadProduct(slug), slug, today()), [], slug);
  }
});

test("a stale file is reported", () => {
  const p = langfuse();
  const now = new Date(new Date(`${p.checkedOn}T00:00:00Z`).getTime() + 200 * DAY);
  assert.deepEqual(validate(p, "langfuse", now), [
    `checked on ${p.checkedOn}, more than 183 days ago: re-check the facts`,
  ]);
});

test("a date in the future is reported, a date one day ahead is not", () => {
  const now = new Date("2026-11-20T00:00:00Z");
  const p = langfuse();
  p.checkedOn = "2026-11-25";
  assert.deepEqual(validate(p, "langfuse", now), ["checkedOn is in the future"]);
  p.checkedOn = "2026-11-21";
  assert.deepEqual(validate(p, "langfuse", now), []);
});

test("an empty value is reported", () => {
  const p = langfuse();
  p.facts.otel.value = "";
  assert.deepEqual(validate(p, "langfuse", today()), ["otel: empty value"]);
});

test("a long chooseThem item is reported", () => {
  const p = langfuse();
  p.chooseThem = ["a".repeat(121)];
  assert.deepEqual(validate(p, "langfuse", today()), [
    "chooseThem item is 121 characters, the limit is 120",
  ]);
});

test("a missing source is reported", () => {
  const p = langfuse();
  p.facts.otel.sources = [];
  assert.deepEqual(validate(p, "langfuse", today()), ["otel: no source"]);
});

test("a source that is neither https nor a cetra.sh path is reported", () => {
  const p = langfuse();
  p.facts.otel.sources = ["http://example.com/docs"];
  assert.deepEqual(validate(p, "langfuse", today()), [
    "otel: source must be https or a cetra.sh path",
  ]);
});

test("a long value is reported", () => {
  const p = langfuse();
  p.facts.otel.value = "a".repeat(121);
  assert.deepEqual(validate(p, "langfuse", today()), [
    "otel: value is 121 characters, the limit is 120",
  ]);
});

test("an unknown area is reported", () => {
  const p = langfuse();
  p.facts.bogus = { value: "Something", sources: ["https://example.com"] };
  assert.deepEqual(validate(p, "langfuse", today()), ["facts has an unknown area: bogus"]);
});

test("a missing area is reported", () => {
  const p = langfuse();
  delete p.facts.sql;
  assert.deepEqual(validate(p, "langfuse", today()), ["sql missing"]);
});

test("a banned word is reported", () => {
  const p = langfuse();
  p.facts.otel.value = "A powerful exporter";
  assert.deepEqual(validate(p, "langfuse", today()), ["otel: banned word powerful"]);
});

test("the product name in capitals is reported in the one-line description", () => {
  const p = langfuse();
  p.oneLine = "Open-source Cetra alternative";
  assert.deepEqual(validate(p, "langfuse", today()), ["oneLine: write cetra in lower case"]);
});

test("slug must equal the file name", () => {
  assert.deepEqual(validate({ ...langfuse(), slug: "other" }, "langfuse", today()), [
    "slug must equal the file name",
  ]);
});

test("a date that is not a real calendar date is reported", () => {
  const p = langfuse();
  p.checkedOn = "2026-02-30";
  assert.deepEqual(validate(p, "langfuse", today()), ["checkedOn must be YYYY-MM-DD"]);
});

test("cetra's chooseUs must be empty and a competitor's must not", () => {
  const cetra = loadProduct("cetra");
  cetra.chooseUs = ["Something"];
  assert.deepEqual(validate(cetra, "cetra", today()), ["chooseUs must be empty"]);

  const competitor = langfuse();
  competitor.chooseUs = [];
  assert.deepEqual(validate(competitor, "langfuse", today()), [
    "chooseUs has 0 items, 1 to 4 allowed",
  ]);
});

test("cetra's prices match the pricing data", () => {
  const cetra = loadProduct("cetra");
  assert.ok(cetra.facts.cloudFree.value.includes(formatUnits(PLANS[0].includedUnits)));
  assert.ok(cetra.facts.cloudPaidFrom.value.includes(`$${PLANS[1].priceMonthly}`));
  assert.ok(cetra.facts.cloudPaidFrom.value.includes(`$${OVERAGE_PER_100K} per 100,000`));
});

test("formatDate", () => {
  assert.equal(formatDate("2026-11-20"), "20 November 2026");
});

test("every competitor's strengths are areas where cetra is ahead", () => {
  const cetra = loadProduct("cetra");
  for (const slug of COMPETITORS) {
    assert.deepEqual(strengthProblems(loadProduct(slug), cetra), [], slug);
  }
});

test("a status outside the list, or unknown on a known value, is reported", () => {
  const p = langfuse();
  p.facts.otel.status = "maybe";
  assert.deepEqual(validate(p, "langfuse", today()), [
    "otel: status must be one of yes, partial, no, unknown",
  ]);
  p.facts.otel.status = "unknown";
  assert.deepEqual(validate(p, "langfuse", today()), [
    "otel: status is unknown exactly when the value is Unknown",
  ]);
});

test("a missing or long short label is reported", () => {
  const p = langfuse();
  p.facts.otel.short = "";
  assert.deepEqual(validate(p, "langfuse", today()), ["otel: short is required"]);
  p.facts.otel.short = "a".repeat(37);
  assert.deepEqual(validate(p, "langfuse", today()), [
    "otel: short is 37 characters, the limit is 36",
  ]);
});

test("strengths must be three different areas that can lead a page", () => {
  const p = langfuse();
  p.strengths = ["sql", "sql", "gateway"];
  assert.deepEqual(validate(p, "langfuse", today()), [
    "strengths must be 3 different areas",
    "strengths: gateway cannot be a strength",
  ]);
  p.strengths = ["sql"];
  assert.deepEqual(validate(p, "langfuse", today()), ["strengths must be 3 areas"]);
});

test("cetra's stack is itself and it has no strengths", () => {
  const p = structuredClone(loadProduct("cetra"));
  p.stack = ["cetra", "PostgreSQL"];
  p.strengths = ["sql", "mcp", "otel"];
  assert.deepEqual(validate(p, "cetra", today()), [
    'stack must be ["cetra"]',
    "cetra has no strengths",
  ]);
});

test("a strength where the competitor is as good, or a small stack, is reported", () => {
  const cetra = loadProduct("cetra");
  const p = langfuse();
  p.strengths = ["selfHostStack", "otel", "sql"];
  p.stack = ["Web", "PostgreSQL"];
  p.facts.otel.status = "yes";
  p.facts.sql.status = "no";
  assert.deepEqual(strengthProblems(p, cetra), [
    "langfuse: selfHostStack is a strength only against 3 or more services",
    "langfuse: otel is not a strength: their status is yes too",
  ]);
});
