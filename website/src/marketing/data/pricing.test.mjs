import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import {
  monthlyPrice,
  OVERAGE_PER_100K,
  PLANS,
  SELF_HOSTED,
  suggestedPlan,
  TEAMS_ADDON,
} from "./pricing.mjs";

const decisions = readFileSync(
  new URL("../../../../docs/mvp-decisions.md", import.meta.url),
  "utf8",
);
const d177 = decisions.split("\n").find((line) => line.startsWith("| D177 |"));

const [free, starter, pro] = PLANS;

test("D177 is found", () => {
  assert.ok(d177, "docs/mvp-decisions.md has no D177 row");
});

test("Free matches D177", () => {
  assert.ok(d177.includes("Free: $0, 100k units/month"));
  assert.ok(d177.includes("30 days data access"));
  assert.equal(free.priceMonthly, 0);
  assert.equal(free.includedUnits, 100_000);
  assert.equal(free.dataAccess, "30 days");
});

test("Starter matches D177", () => {
  assert.ok(d177.includes("Starter: $30/month, 200k units/month included then $4 per 100k"));
  assert.ok(d177.includes("120 days data access"));
  assert.equal(starter.priceMonthly, 30);
  assert.equal(starter.includedUnits, 200_000);
  assert.equal(starter.dataAccess, "120 days");
  assert.equal(OVERAGE_PER_100K, 4);
});

test("Pro matches D177", () => {
  assert.ok(d177.includes("Pro: $150/month"));
  assert.ok(d177.includes("5 years data access"));
  assert.equal(pro.priceMonthly, 150);
  assert.equal(pro.includedUnits, starter.includedUnits);
  assert.equal(pro.dataAccess, "5 years");
});

test("Teams matches D177", () => {
  assert.ok(d177.includes("Teams add-on (on Pro): $300/month"));
  assert.equal(TEAMS_ADDON.priceMonthly, 300);
});

test("monthly price", () => {
  assert.equal(monthlyPrice(starter, 0), 30);
  assert.equal(monthlyPrice(starter, 200_000), 30);
  assert.equal(monthlyPrice(starter, 300_000), 34);
  assert.equal(monthlyPrice(starter, 1_200_000), 70);
  assert.equal(monthlyPrice(pro, 1_200_000), 190);
  assert.equal(monthlyPrice(free, 100_000), 0);
  assert.equal(monthlyPrice(free, 100_001), null);
});

test("suggested plan", () => {
  assert.equal(suggestedPlan(50_000), free);
  assert.equal(suggestedPlan(100_000), free);
  assert.equal(suggestedPlan(100_001), starter);
});

test("licence features match the docs", () => {
  const mdx = readFileSync("src/content/docs/docs/platform/plans-and-licence.mdx", "utf8");
  const keys = mdx
    .split("\n")
    .filter((line) => line.startsWith("| `"))
    .map((line) => /^\| `([^`]+)`/.exec(line)?.[1]);
  assert.deepEqual([...keys].sort(), ["org_sso", "project_roles", "sso_enforcement"]);
  assert.equal(SELF_HOSTED.licence.features.length, 3);
});
