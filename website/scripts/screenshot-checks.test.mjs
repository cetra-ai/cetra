import assert from "node:assert/strict";
import { test } from "node:test";
import { checkClipRect, checkStorage } from "./screenshot-checks.mjs";

test("a shot without a storage map passes", () => {
  assert.doesNotThrow(() => checkStorage([{ name: "a" }]));
});

test("a storage map of string values passes", () => {
  assert.doesNotThrow(() =>
    checkStorage([{ name: "a", storage: { "cetra.theme": "light", "cetra.assistant.open": "1" } }]),
  );
});

test("an empty storage map passes", () => {
  assert.doesNotThrow(() => checkStorage([{ name: "a", storage: {} }]));
});

test("a storage value that is not an object is rejected, naming the shot", () => {
  for (const storage of ["x", 1, null, ["a"]]) {
    assert.throws(
      () => checkStorage([{ name: "bad-shot", storage }]),
      /shot "bad-shot": storage must be an object/,
    );
  }
});

test("an empty key is rejected, naming the shot", () => {
  assert.throws(
    () => checkStorage([{ name: "bad-shot", storage: { "": "x" } }]),
    /shot "bad-shot": storage "" must be a non-empty key with a string value/,
  );
});

test("a value that is not a string is rejected, naming the shot and the key", () => {
  for (const value of [1, true, null, { a: 1 }]) {
    assert.throws(
      () => checkStorage([{ name: "bad-shot", storage: { a: value } }]),
      /shot "bad-shot": storage "a" must be a non-empty key with a string value/,
    );
  }
});

test("the first bad shot in a list is the one named", () => {
  assert.throws(
    () => checkStorage([{ name: "good" }, { name: "bad", storage: { a: 1 } }]),
    /shot "bad"/,
  );
});

test("a clipRect of integers passes, with x and y at 0", () => {
  assert.doesNotThrow(() =>
    checkClipRect([{ name: "a", clipRect: { x: 0, y: 0, width: 900, height: 900 } }]),
  );
  assert.doesNotThrow(() => checkClipRect([{ name: "a" }]));
});

test("a clipRect that is not an object is rejected, naming the shot", () => {
  for (const clipRect of ["x", 1, null, [1, 2, 3, 4]]) {
    assert.throws(
      () => checkClipRect([{ name: "bad-shot", clipRect }]),
      /shot "bad-shot": clipRect must be an object/,
    );
  }
});

test("a clipRect side that is missing, not an integer, or out of range is rejected", () => {
  const good = { x: 0, y: 0, width: 900, height: 900 };
  const bad = [
    { ...good, width: undefined },
    { ...good, height: 0 },
    { ...good, width: -5 },
    { ...good, x: -1 },
    { ...good, y: 1.5 },
    { ...good, height: "900" },
  ];
  for (const clipRect of bad) {
    assert.throws(
      () => checkClipRect([{ name: "bad-shot", clipRect }]),
      /shot "bad-shot": clipRect\.\w+ must be an integer/,
    );
  }
});

test("a shot with both clip and clipRect is rejected, naming the shot", () => {
  assert.throws(
    () =>
      checkClipRect([
        { name: "bad-shot", clip: "main", clipRect: { x: 0, y: 0, width: 1, height: 1 } },
      ]),
    /shot "bad-shot": a shot has clip or clipRect, not both/,
  );
});
