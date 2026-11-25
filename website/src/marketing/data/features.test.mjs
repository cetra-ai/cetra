import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { test } from "node:test";
import {
  CLOUD_COLUMNS,
  docHref,
  FEATURE_GROUPS,
  groupsFor,
  SELF_HOSTED_COLUMNS,
  TEAMS_CELL,
} from "./features.mjs";
import { formatMoney, formatUnits, OVERAGE_PER_100K, PLANS, SELF_HOSTED } from "./pricing.mjs";

const decisions = readFileSync(
  new URL("../../../../docs/mvp-decisions.md", import.meta.url),
  "utf8",
).split("\n");
const docsRoot = new URL("../../content/docs/docs/", import.meta.url);

/** @param {string} doc */
function docPageExists(doc) {
  return [`${doc}.md`, `${doc}.mdx`, `${doc}/index.md`, `${doc}/index.mdx`].some((file) =>
    existsSync(new URL(file, docsRoot)),
  );
}

/** @param {string} label */
function limitRow(label) {
  const row = FEATURE_GROUPS.find((group) => group.id === "limits")?.rows.find(
    (r) => r.label === label,
  );
  assert.ok(row, `no limits row labelled "${label}"`);
  return row;
}

const allRows = FEATURE_GROUPS.flatMap((group) =>
  group.rows.map((row) => ({ group: group.id, row })),
);

test("every row is backed by a docs page or a decision", () => {
  for (const { row } of allRows) {
    const hasDoc = typeof row.doc === "string" && row.doc.length > 0;
    if (hasDoc) {
      assert.equal(row.source, undefined, `"${row.label}" has a doc and a source`);
      assert.equal(row.claim, undefined, `"${row.label}" has a doc and a claim`);
    } else {
      assert.equal(row.doc, null, `"${row.label}" has an empty doc`);
      assert.ok(row.source && row.source.length > 0, `"${row.label}" has no source`);
      assert.ok(row.claim && row.claim.length > 0, `"${row.label}" has no claim`);
    }
  }
});

test("every docs page exists", () => {
  for (const { row } of allRows) {
    if (row.doc === null) continue;
    assert.ok(docPageExists(row.doc), `row "${row.label}" links to missing docs page "${row.doc}"`);
  }
});

test("every decision source exists and states the claim", () => {
  for (const { row } of allRows) {
    if (row.source === undefined) continue;
    assert.match(row.source, /^D[\w-]*\d+$/, `"${row.label}" has a malformed source ${row.source}`);
    const line = decisions.find((l) => l.startsWith(`| ${row.source} |`));
    assert.ok(line, `row "${row.label}": ${row.source} has no line in mvp-decisions.md`);
    assert.ok(
      line.includes(row.claim),
      `row "${row.label}": ${row.source} does not state the claim "${row.claim}"`,
    );
  }
});

test("every row is in at least one table, with the right number of cells", () => {
  for (const { row } of allRows) {
    assert.ok(
      row.cloud !== undefined || row.selfHosted !== undefined,
      `"${row.label}" is in neither table`,
    );
    if (row.cloud !== undefined) {
      assert.equal(row.cloud.length, CLOUD_COLUMNS.length, `"${row.label}" cloud cells`);
    }
    if (row.selfHosted !== undefined) {
      assert.equal(
        row.selfHosted.length,
        SELF_HOSTED_COLUMNS.length,
        `"${row.label}" self-hosted cells`,
      );
    }
  }
});

test("cells are booleans or short strings", () => {
  for (const { row } of allRows) {
    for (const cells of [row.cloud, row.selfHosted]) {
      if (cells === undefined) continue;
      for (const cell of cells) {
        if (typeof cell === "boolean") continue;
        assert.equal(typeof cell, "string", `"${row.label}" has a cell of type ${typeof cell}`);
        assert.ok(
          cell.length >= 1 && cell.length <= 24,
          `"${row.label}" has a cell of ${cell.length} characters: "${cell}"`,
        );
      }
    }
  }
});

test("labels are unique within a group", () => {
  for (const group of FEATURE_GROUPS) {
    const labels = group.rows.map((row) => row.label);
    assert.equal(new Set(labels).size, labels.length, `group ${group.id} repeats a label`);
  }
});

test("the columns match pricing.mjs", () => {
  assert.deepEqual(
    CLOUD_COLUMNS.map((c) => c.name),
    ["Free", "Starter", "Pro", "Enterprise"],
  );
  assert.deepEqual(
    CLOUD_COLUMNS.map((c) => c.price),
    ["$0 / month", "$30 / month", "$150 / month", "Custom"],
  );
  assert.deepEqual(
    SELF_HOSTED_COLUMNS.map((c) => c.name),
    ["Self-hosted", "Self-hosted licence"],
  );
  assert.deepEqual(
    SELF_HOSTED_COLUMNS.map((c) => c.price),
    ["$0", "Custom"],
  );
});

test("the limit rows match the plans", () => {
  assert.deepEqual(
    limitRow("Spans and scores a month").cloud?.slice(0, 3),
    PLANS.map((p) => formatUnits(p.includedUnits)),
  );
  assert.equal(limitRow("Spans and scores a month").cloud?.[3], "Custom");

  assert.deepEqual(
    limitRow("History").cloud?.slice(0, 3),
    PLANS.map((p) => p.dataAccess),
  );
  assert.equal(limitRow("History").cloud?.[3], "Custom");

  const overage = limitRow("Above the included amount").cloud;
  PLANS.forEach((plan, i) => {
    const expected = plan.overage ? `${formatMoney(OVERAGE_PER_100K)} per 100,000` : "Hard limit";
    assert.equal(overage?.[i], expected, `overage cell for ${plan.name}`);
  });
  assert.equal(overage?.[3], "Custom");

  const seats = limitRow("Seats (viewers are free)").cloud;
  assert.equal(seats?.[0], "1");
  assert.equal(PLANS[0].seats, "1 seat");
  assert.equal(seats?.[1], "Unlimited");
  assert.equal(seats?.[2], "Unlimited");
  assert.equal(PLANS[1].seats, "Unlimited seats");
  assert.equal(PLANS[2].seats, "Unlimited seats");

  assert.equal(limitRow("Projects").cloud?.[0], "1");
  assert.equal(PLANS[0].projects, "1 project");

  const ruleRows = [
    ["Alert rules", "1 alert rule", "20", "20 alert rules"],
    [
      "Active online evaluation rules",
      "1 online evaluation rule",
      "10",
      "10 online evaluation rules",
    ],
    ["Annotation queues", "1 annotation queue", "3", "3 annotation queues"],
    ["Issue sources", "1 issue source", "3", "3 issue sources"],
  ];
  for (const [label, freeText, starterCell, starterText] of ruleRows) {
    const cells = limitRow(label).cloud;
    assert.equal(cells?.[0], "1", `${label} on Free`);
    assert.ok(PLANS[0].rules.includes(freeText), `Free rules should include "${freeText}"`);
    assert.equal(cells?.[1], starterCell, `${label} on Starter`);
    assert.ok(
      PLANS[1].rules.includes(starterText),
      `Starter rules should include "${starterText}"`,
    );
    assert.equal(cells?.[2], "Unlimited", `${label} on Pro`);
  }
  assert.equal(PLANS[2].rules, "Unlimited rules");
});

test("licence rows match the licence features", () => {
  const teamsRows = allRows.filter(
    ({ row }) =>
      row.cloud !== undefined &&
      row.cloud.length === 4 &&
      row.cloud[0] === false &&
      row.cloud[1] === false &&
      row.cloud[2] === TEAMS_CELL &&
      row.cloud[3] === true &&
      row.selfHosted !== undefined,
  );
  for (const { row } of teamsRows) {
    assert.deepEqual(row.selfHosted, [false, true], `"${row.label}" licence cells`);
  }
  const licenceRows = allRows.filter(
    ({ row }) =>
      row.selfHosted !== undefined &&
      row.selfHosted.length === 2 &&
      row.selfHosted[0] === false &&
      row.selfHosted[1] === true,
  );
  // Every licence feature has a row, plus its email support row: a service, not a gated
  // feature (D197). SAML is licence-only, with no Teams cell (D-SA-3).
  assert.equal(licenceRows.length, SELF_HOSTED.licence.features.length + 1);
});

test("retention controls are Pro and up", () => {
  const row = allRows.find(({ row: r }) => r.label === "Retention controls per project")?.row;
  assert.ok(row, "no row labelled Retention controls per project");
  assert.deepEqual(row.cloud, [false, false, true, true]);
  assert.deepEqual(row.selfHosted, [true, true]);
});

test("each table has about 55 to 65 rows", () => {
  const count = (table) => groupsFor(table).reduce((sum, group) => sum + group.rows.length, 0);
  assert.equal(count("cloud"), 62);
  assert.equal(count("selfHosted"), 61);
});

test("groupsFor drops rows and empty groups", () => {
  const cloud = groupsFor("cloud");
  assert.ok(
    !cloud.some((group) => group.id === "deployment"),
    "cloud table has a deployment group",
  );

  const selfHosted = groupsFor("selfHosted");
  const rows = selfHosted.flatMap((group) => group.rows);
  assert.ok(
    !rows.some((row) => row.label === "Dedicated cell"),
    "self-hosted table has Dedicated cell",
  );
  const support = selfHosted.find((group) => group.id === "support");
  assert.equal(support?.rows.length, 2);

  for (const table of ["cloud", "selfHosted"]) {
    const expected = FEATURE_GROUPS.flatMap((group) =>
      group.rows.filter((row) => row[table] !== undefined),
    );
    const actual = groupsFor(table).flatMap((group) => group.rows);
    assert.equal(actual.length, expected.length, `${table} row count`);
    expected.forEach((source, i) => {
      const href = source.doc === null ? null : docHref(source.doc);
      assert.equal(actual[i].href, href, `href of "${source.label}" in ${table}`);
    });
  }
});

test("docHref", () => {
  assert.equal(docHref("tracing/traces"), "/docs/tracing/traces/");
  assert.equal(docHref("integrations"), "/docs/integrations/");
});
