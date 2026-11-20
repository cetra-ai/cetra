import assert from "node:assert/strict";
import { test } from "node:test";
import { COMPETITORS, formatDate, listFiles, loadProduct, validate } from "./compare.mjs";
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
