import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import {
  estimateText,
  formatMoney,
  formatUnits,
  MAX_UNITS,
  monthlyPrice,
  OVERAGE_PER_100K,
  PLANS,
  parseUnits,
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

test("the Free plan's new limits match the decisions", () => {
  const line = (id) => decisions.split("\n").find((l) => l.startsWith(`| ${id} |`));
  assert.ok(line("D-AQ-10")?.includes("Free 1 queue per org"), "D-AQ-10 has no Free queue limit");
  assert.ok(line("D-IC-13")?.includes("Free 1"), "D-IC-13 has no Free issue source limit");
  assert.ok(free.rules.includes("1 annotation queue"));
  assert.ok(free.rules.includes("1 issue source"));
});

test("the Starter caps match D178", () => {
  const d178 = decisions.split("\n").find((line) => line.startsWith("| D178 |"));
  assert.ok(
    d178?.includes(
      "Starter gets 20 alert rules, 10 active online-evaluation rules, 3 annotation queues and 3 issue sources",
    ),
    "D178 has no Starter caps",
  );
  assert.equal(
    starter.rules,
    "20 alert rules, 10 online evaluation rules, 3 annotation queues and 3 issue sources",
  );
  assert.equal(pro.rules, "Unlimited rules");
});

test("seats and projects match D177", () => {
  assert.ok(d177.includes("1 seat, 1 project"));
  assert.ok(d177.includes("unlimited seats"));
  assert.equal(free.seats, "1 seat");
  assert.equal(free.projects, "1 project");
  assert.equal(starter.seats.toLowerCase(), "unlimited seats");
  assert.equal(starter.projects, null);
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
  assert.ok(d177.includes("Pro: $150/month, 400k units/month included"));
  assert.equal(pro.includedUnits, 400_000);
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
  assert.equal(monthlyPrice(pro, 400_000), 150);
  assert.equal(monthlyPrice(pro, 1_200_000), 182);
  assert.equal(monthlyPrice(free, 100_000), 0);
  assert.equal(monthlyPrice(free, 100_001), null);
});

test("formatUnits", () => {
  assert.equal(formatUnits(100_000), "100,000");
  assert.equal(formatUnits(0), "0");
});

test("formatMoney rounds up to the dollar", () => {
  assert.equal(formatMoney(30.4), "$31");
  assert.equal(formatMoney(30), "$30");
  assert.equal(formatMoney(1184), "$1,184");
  assert.equal(formatMoney(0), "$0");
});

test("parseUnits", () => {
  assert.equal(parseUnits(""), 0);
  assert.equal(parseUnits("-5"), 0);
  assert.equal(parseUnits("abc"), 0);
  assert.equal(parseUnits("100000.7"), 100_000);
  assert.equal(parseUnits("1e400"), MAX_UNITS);
  assert.equal(parseUnits(250_000), 250_000);
});

test("estimateText", () => {
  assert.equal(estimateText(free, 210_000), "Not available above 100,000 a month");
  assert.equal(estimateText(starter, 210_000), "$31");
  assert.equal(estimateText(starter, MAX_UNITS), "Talk to us");
});

test("suggested plan", () => {
  assert.equal(suggestedPlan(50_000), free);
  assert.equal(suggestedPlan(100_000), free);
  assert.equal(suggestedPlan(100_001), starter);
});

test("licence features match the docs", () => {
  const mdx = readFileSync(
    new URL("../../content/docs/docs/platform/plans-and-licence.mdx", import.meta.url),
    "utf8",
  );
  const keys = mdx
    .split("\n")
    .filter((line) => line.startsWith("| `"))
    .map((line) => /^\| `([^`]+)`/.exec(line)?.[1]);
  assert.deepEqual([...keys].sort(), [
    "audit_log",
    "org_sso",
    "project_roles",
    "scim",
    "sso_enforcement",
  ]);
  assert.equal(SELF_HOSTED.licence.features.length, 5);
});

test("licence support matches D197 and the docs", () => {
  const d197 = decisions.split("\n").find((line) => line.startsWith("| D197 |"));
  assert.ok(
    d197?.includes(`"${SELF_HOSTED.licence.support}."`),
    "D197 does not quote the support line",
  );
  const mdx = readFileSync(
    new URL("../../content/docs/docs/platform/plans-and-licence.mdx", import.meta.url),
    "utf8",
  );
  assert.ok(
    mdx.includes("email support from the cetra team, with a reply within 2 business days"),
    "plans-and-licence.mdx does not state the licence support",
  );
});
