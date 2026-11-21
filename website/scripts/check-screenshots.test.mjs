import assert from "node:assert/strict";
import { test } from "node:test";
import { checkShots } from "./check-screenshots.mjs";

test("a listed shot with its PNG passes", () => {
  assert.deepEqual(checkShots(["a", "b"], ["a.png", "b.png"]), []);
});

test("a shot without a PNG fails", () => {
  assert.equal(checkShots(["a", "b"], ["a.png"]).length, 1);
});

test("a PNG without a shot fails", () => {
  assert.equal(checkShots(["a"], ["a.png", "stray.png"]).length, 1);
});
