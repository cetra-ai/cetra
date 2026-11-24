import assert from "node:assert/strict";
import { test } from "node:test";
import { checkStorage } from "./screenshot-storage.mjs";

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
