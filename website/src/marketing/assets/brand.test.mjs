import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

test("the logo copy equals the brand file", () => {
  const copy = readFileSync(new URL("./cetra-logo-on-dark.svg", import.meta.url));
  const brand = readFileSync(
    new URL("../../../../assets/brand/cetra-logo-on-dark.svg", import.meta.url),
  );
  assert.deepStrictEqual(copy, brand);
});
