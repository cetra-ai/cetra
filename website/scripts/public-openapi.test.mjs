import assert from "node:assert/strict";
import { test } from "node:test";
import { publicSpec } from "./public-openapi.mjs";

test("publicSpec drops internal operations, paths and the tag", () => {
  const spec = {
    openapi: "3.1.0",
    tags: [{ name: "traces" }, { name: "internal" }],
    paths: {
      "/api/v1/traces": {
        get: { tags: ["traces"], operationId: "listTraces" },
        post: { tags: ["internal"], operationId: "internalPost" },
      },
      "/api/internal/orgs": { put: { tags: ["internal"], operationId: "putOrg" } },
    },
  };
  const before = structuredClone(spec);
  const result = publicSpec(spec);
  assert.deepEqual(Object.keys(result.paths), ["/api/v1/traces"]);
  assert.deepEqual(Object.keys(result.paths["/api/v1/traces"]), ["get"]);
  assert.deepEqual(result.tags, [{ name: "traces" }]);
  assert.equal(result.servers[0].url, "http://localhost:8080");
  assert.deepEqual(spec, before);
});

test("publicSpec keeps the servers the document has", () => {
  const servers = [{ url: "https://example.com" }];
  assert.deepEqual(publicSpec({ paths: {}, servers }).servers, servers);
});
