import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { test } from "node:test";
import { PICTURES, panelsOf, pictureById, STORIES, shotName } from "./stories.mjs";

const shots = JSON.parse(
  readFileSync(new URL("../../../screenshots.json", import.meta.url), "utf8"),
);
const shotNames = new Set(shots.map((shot) => shot.name));
const pngDir = new URL("../../assets/screenshots/", import.meta.url);

/** The 8 captions, as the fixed content of plan M15 states them (copy edits touch this table too): one
 * per story, or one string for a shared picture (M19 retake). */
const EXPECTED = {
  hero: {
    support: "See what each step of a support answer cost and how long it took.",
    coding: "Find the failed test run, and the fix that followed, in one trace.",
    voice: "Find the one slow tool call behind a voice turn that took 3.1 s.",
    extraction:
      "See the schema check send an invoice back for a second extraction, and what it cost.",
  },
  trace: {
    support: "Each turn of a session, with its score, so the bad answer stands out.",
    coding: "Read the code, patch it and rerun the tests, all in one transcript.",
    voice: "Hear the caller and the agent, and read what was said, in one voice turn.",
    extraction: "A 200-invoice batch, with its three failed invoices at the top.",
  },
  explore: "Ask which prompt got worse, and get the numbers and the cause.",
  issues: "Answers that contradict their sources, grouped into one issue with their examples.",
  evaluate: "Version 4 lifts vat_number from 91% to 96% on 300 invoices.",
  prompts: "Run two prompt versions on the same question, and see which one answers right.",
  alert: "Know when the error rate, a score, the cost or an issue crosses your line.",
};

/** The pictures shared by every story (M19 retake): the same shot, no story picker. */
const SHARED = ["explore", "issues", "evaluate", "prompts", "alert"];
const PANELS = 4 * (7 - SHARED.length) + SHARED.length;

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

test("there are seven pictures, of four stories each or shared by all", () => {
  assert.deepEqual(
    PICTURES.map((picture) => picture.id),
    ["hero", "trace", "explore", "issues", "evaluate", "prompts", "alert"],
  );
  assert.deepEqual(
    PICTURES.filter((picture) => picture.shared).map((picture) => picture.id),
    SHARED,
  );
  for (const picture of PICTURES) {
    const panels = panelsOf(picture.id);
    assert.deepEqual(
      panels.map((panel) => panel.story),
      picture.shared ? ["all"] : STORIES.map((story) => story.id),
    );
    assert.equal(
      picture.shared === undefined,
      picture.stories !== undefined,
      `${picture.id}: shared or stories`,
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
  assert.equal(names.length, PANELS);
  assert.equal(new Set(names).size, PANELS);
  assert.equal(shotName("hero", "voice"), "marketing-trace-view-voice");
  assert.equal(shotName("trace", "support"), "marketing-trace-page");
  assert.equal(shotName("trace", "voice"), "marketing-trace-page-voice");
  assert.equal(shotName("evaluate", "support"), "marketing-evaluate");
  assert.equal(shotName("evaluate", "coding"), "marketing-evaluate");
  assert.equal(shotName("issues", "voice"), "marketing-issue");
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
  assert.equal(new Set(alts).size, PANELS, "two alts are the same");
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
  assert.equal(new Set(captions).size, PANELS, "two captions are the same");

  for (const picture of PICTURES) {
    const expected = EXPECTED[picture.id];
    assert.ok(expected, `EXPECTED has no picture "${picture.id}"`);
    if (picture.shared) {
      assert.equal(picture.shared.caption, expected, `caption of ${picture.id}`);
      continue;
    }
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
