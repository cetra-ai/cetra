import assert from "node:assert/strict";
import { createServer } from "node:net";
import { test } from "node:test";
import { portIsFree } from "./port-free.mjs";

test("a port held by a listener is not free, and is free again once it closes", async () => {
  const holder = createServer();
  await new Promise((resolve) => holder.listen(0, "127.0.0.1", resolve));
  const { port } = holder.address();
  assert.equal(await portIsFree(port), false);
  await new Promise((resolve) => holder.close(resolve));
  assert.equal(await portIsFree(port), true);
});
