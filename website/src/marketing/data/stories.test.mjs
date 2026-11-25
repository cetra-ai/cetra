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
    support: "Each turn of a session, with its score, so the bad answer stands out.",
    coding: "Read the code, patch it and rerun the tests, all in one transcript.",
    voice: "Hear the caller and the agent, and read what was said, in one voice turn.",
    extraction: "A 200-invoice batch, with its three failed invoices at the top.",
  },
  explore: {
    support: "Ask why answers got a thumbs-down, then open the traces behind them.",
    coding: "Ask why runs cost more, then open the retried runs.",
    voice: "Ask why callers hang up, then open the slow calendar checks.",
    extraction: "Ask which vendors fail, then open the re-extracted invoices.",
  },
  issues: {
    support: "The same question, 244 times in 14 days, grouped into one issue.",
    coding: "Runs that stop when a diff no longer fits, grouped into one issue.",
    voice: "Replies that start before the caller has finished, grouped into one issue.",
    extraction: "Scanned invoices that give OCR nothing to read, grouped into one issue.",
  },
  evaluate: {
    support: "Reviewers score each flagged answer in a queue, one by one.",
    coding: "A code check scores every run, and the rule charts its pass rate by the hour.",
    voice: "Write the judge's question once, then test it on a real call.",
    extraction: "Version 4 lifts vat_number from 91% to 96% on 300 invoices.",
  },
  prompts: {
    support: "Run two prompt versions on the same question, and see which one answers right.",
    coding: "Run the patch prompt on a wide change, and see it split into steps.",
    voice: "Tune the receptionist prompt so it reads the booking back.",
    extraction: "A comma-decimal total, 1.234,56, comes back as 1234.56 in JSON.",
  },
  alert: {
    support: "Know when answer quality drops below your bar.",
    coding: "Know when an hour of agent spend goes over your budget.",
    voice: "Know when one in twenty replies takes longer than 1.5 s.",
    extraction: "Know when scanned invoices come in faster than 10 an hour.",
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
  assert.equal(shotName("trace", "support"), "marketing-trace-page");
  assert.equal(shotName("trace", "voice"), "marketing-trace-page-voice");
  assert.equal(shotName("evaluate", "support"), "marketing-evaluate");
  assert.equal(shotName("evaluate", "coding"), "marketing-evaluate-coding");
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
