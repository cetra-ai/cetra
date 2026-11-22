import assert from "node:assert/strict";
import { test } from "node:test";
import { checkSite } from "./check-env-vars.mjs";

const code = "CETRA_DATA_DIR CETRA_AUTH__OIDC__ other";
const keys = new Set(["storage.flush.max_age"]);
const check = (text) => checkSite({ "p.md": text }, code, keys);

test("accepts a name the code reads", () => {
  assert.deepEqual(check("Set `CETRA_DATA_DIR`."), []);
});

test("finds a name the code does not read", () => {
  assert.deepEqual(check("a\nSet `CETRA_NOPE` now."), [
    "p.md:2: CETRA_NOPE is not read by the code",
  ]);
});

test("accepts the split form of a documented key", () => {
  assert.deepEqual(check("`CETRA_STORAGE__FLUSH__MAX_AGE`"), []);
});

test("finds the split form of an undocumented key", () => {
  assert.deepEqual(check("`CETRA_STORAGE__NOPE`"), [
    "p.md:1: CETRA_STORAGE__NOPE is not a documented configuration key",
  ]);
});

test("accepts a pattern whose prefix the code has", () => {
  assert.deepEqual(check("CETRA_AUTH__OIDC__<NAME>__CLIENT_SECRET"), []);
});

test("finds a pattern whose prefix the code lacks", () => {
  assert.deepEqual(check("CETRA_NOPE__<NAME>__X"), [
    "p.md:1: CETRA_NOPE__<…> is not read by the code",
  ]);
});

test("ignores the bare prefix", () => {
  assert.deepEqual(check("`CETRA_` followed by the path"), []);
});

test("checks names inside code fences", () => {
  assert.equal(check("```bash\nCETRA_NOPE=1\n```").length, 1);
});

test("accepts a name that only the node CLI reads", () => {
  const cli = "const project = env.CETRA_PROJECT;";
  assert.deepEqual(checkSite({ "p.md": "Set `CETRA_PROJECT`." }, cli, keys), []);
});

test("checks the agent skill's pages too", () => {
  assert.deepEqual(checkSite({ "skills/cetra/SKILL.md": "Set `CETRA_NOPE`." }, code, keys), [
    "skills/cetra/SKILL.md:1: CETRA_NOPE is not read by the code",
  ]);
});
