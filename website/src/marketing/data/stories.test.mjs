import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { test } from "node:test";
import { PICTURES, panelsOf, pictureById, STORIES, shotName } from "./stories.mjs";

const shots = JSON.parse(
  readFileSync(new URL("../../../screenshots.json", import.meta.url), "utf8"),
);
const shotNames = new Set(shots.map((shot) => shot.name));
const pngDir = new URL("../../assets/screenshots/", import.meta.url);

/** The 28 captions, as the fixed content of plan M15 states them (copy edits touch this table too). */
const EXPECTED = {
  hero: {
    support: "See what each step of a support answer cost and how long it took.",
    coding: "Find the failed test run, and the fix that followed, in one trace.",
    voice: "Find the one slow tool call behind a voice turn that took 3.1 s.",
    extraction:
      "See the schema check send an invoice back for a second extraction, and what it cost.",
  },
  trace: {
    support: "Every support conversation, with its cost and latency.",
    coding: "Only the failed runs, so nothing broken hides in the traffic.",
    voice: "The slowest calls first, so you fix what callers feel.",
    extraction: "Spot the costly billing traces, with what each one cost.",
  },
  explore: {
    support: "Ask which answers scored low, then get a quality dashboard built for you.",
    coding: "Ask what failed, then get a dashboard for the coding agent built for you.",
    voice: "Ask which calls were slow, then get a latency dashboard built for you.",
    extraction: "Ask which invoices needed a retry, then get a dashboard built for you.",
  },
  issues: {
    support: "What customers ask most, grouped for you, with real examples.",
    coding: "The same test failure across runs becomes one issue to fix.",
    voice: "Slow replies grouped into one issue, with the trace that shows why.",
    extraction: "Wrong totals grouped into one issue, before they reach your books.",
  },
  evaluate: {
    support: "Reviewers score real answers in a queue.",
    coding: "Review the agent's patches the way you review a pull request.",
    voice: "Check whether each caller got what they asked for.",
    extraction: "Check the extracted fields against the invoice, one by one.",
  },
  prompts: {
    support: "Try a prompt change on a real question before you ship it.",
    coding: "Run the patch prompt on a real issue and see the diff.",
    voice: "Tune the receptionist prompt for short spoken answers.",
    extraction: "Compare a cheap and a strong model on the same invoice, with the cost of each.",
  },
  alert: {
    support: "Know when answer quality drops below your bar.",
    coding: "Know when the coding agent's error rate climbs, in Slack.",
    voice: "Know when callers start waiting, before they hang up.",
    extraction: "Know when the schema check pass rate slips.",
  },
};

test("the four stories are in the fixed order", () => {
  assert.deepEqual(
    STORIES.map((story) => story.id),
    ["support", "coding", "voice", "extraction"],
  );
  assert.deepEqual(
    STORIES.map((story) => story.label),
    ["Support agent", "Coding agent", "Voice agent", "Document extraction"],
  );
  assert.equal(STORIES[0].id, "support");
});

test("there are seven pictures of four stories each", () => {
  assert.deepEqual(
    PICTURES.map((picture) => picture.id),
    ["hero", "trace", "explore", "issues", "evaluate", "prompts", "alert"],
  );
  for (const picture of PICTURES) {
    const panels = panelsOf(picture.id);
    assert.deepEqual(
      panels.map((panel) => panel.story),
      STORIES.map((story) => story.id),
    );
  }
});

test("every shot is in screenshots.json and has a PNG", () => {
  const names = [];
  for (const picture of PICTURES) {
    for (const panel of panelsOf(picture.id)) {
      assert.ok(shotNames.has(panel.shot), `${panel.shot} is not in screenshots.json`);
      assert.ok(existsSync(new URL(`${panel.shot}.png`, pngDir)), `${panel.shot}.png is missing`);
      names.push(panel.shot);
    }
  }
  assert.equal(names.length, 28);
  assert.equal(new Set(names).size, 28);
  assert.equal(shotName("hero", "voice"), "marketing-trace-view-voice");
  assert.equal(shotName("trace", "support"), "marketing-trace-list");
});

test("alts are one sentence that describes the picture", () => {
  const alts = [];
  for (const picture of PICTURES) {
    for (const panel of panelsOf(picture.id)) {
      assert.ok(panel.alt.length > 0, `${panel.shot} has no alt`);
      assert.ok(panel.alt.length <= 260, `${panel.shot} alt is longer than 260 characters`);
      assert.ok(panel.alt.endsWith("."), `${panel.shot} alt does not end with a full stop`);
      assert.ok(!panel.alt.includes("\n"), `${panel.shot} alt has a line break`);
      alts.push(panel.alt);
    }
  }
  assert.equal(new Set(alts).size, 28, "two alts are the same");
});

test("captions are one line", () => {
  const captions = [];
  for (const picture of PICTURES) {
    for (const panel of panelsOf(picture.id)) {
      assert.ok(panel.caption.length > 0, `${panel.shot} has no caption`);
      assert.ok(panel.caption.length <= 100, `${panel.shot} caption is longer than 100 characters`);
      assert.ok(panel.caption.endsWith("."), `${panel.shot} caption does not end with a full stop`);
      assert.ok(!panel.caption.includes("\n"), `${panel.shot} caption has a line break`);
      captions.push(panel.caption);
    }
  }
  assert.equal(new Set(captions).size, 28, "two captions are the same");

  for (const picture of PICTURES) {
    const expected = EXPECTED[picture.id];
    assert.ok(expected, `EXPECTED has no picture "${picture.id}"`);
    for (const { id } of STORIES) {
      assert.equal(
        pictureById(picture.id).stories[id].caption,
        expected[id],
        `caption of ${picture.id} / ${id}`,
      );
    }
  }
});

test("no copy uses a banned word", () => {
  const banned = /\b(seamless(ly)?|blazing(ly)?|effortless(ly)?|powerful|simply|easy|just)\b/i;
  for (const picture of PICTURES) {
    const labels =
      typeof picture.label === "string" ? [picture.label] : Object.values(picture.label);
    const texts = [...labels];
    for (const panel of panelsOf(picture.id)) texts.push(panel.alt, panel.caption);
    for (const text of texts) {
      assert.doesNotMatch(text, banned, text);
      assert.doesNotMatch(text, /!/, text);
      assert.doesNotMatch(text, /Cetra|CETRA/, text);
      assert.doesNotMatch(text, /\p{Extended_Pictographic}/u, text);
    }
  }
});

test("unknown pictures and stories throw", () => {
  assert.throws(() => pictureById("nope"), /stories:/);
  assert.throws(() => shotName("hero", "nope"), /stories:/);
});
