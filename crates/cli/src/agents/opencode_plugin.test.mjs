// Tests for the pure helpers of opencode_plugin.ts. Importing the plugin reads no file: it
// reads its config only when OpenCode calls setup or server.
// Run: node --test crates/cli/src/agents/opencode_plugin.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";
import { configPath, finishReason, parseConfig, usageAttributes } from "./opencode_plugin.ts";

test("configPath takes the config from XDG_CONFIG_HOME", () => {
  assert.equal(configPath({ XDG_CONFIG_HOME: "/x" }), "/x/opencode/cetra-opencode.json");
});

test("configPath falls back to HOME/.config when XDG_CONFIG_HOME is empty", () => {
  assert.equal(
    configPath({ XDG_CONFIG_HOME: "", HOME: "/h" }),
    "/h/.config/opencode/cetra-opencode.json",
  );
});

test("configPath is null when neither variable is set", () => {
  assert.equal(configPath({}), null);
  assert.equal(configPath({ XDG_CONFIG_HOME: "", HOME: "" }), null);
});

test("parseConfig reads the object setup writes", () => {
  const written = JSON.stringify({
    managed_by: "cetra agents setup opencode",
    endpoint: "https://cetra.example.com",
    headers: { Authorization: "Bearer k" },
    capture_content: true,
  });
  assert.deepEqual(parseConfig(written), {
    endpoint: "https://cetra.example.com",
    headers: { Authorization: "Bearer k" },
    captureContent: true,
  });
});

test("parseConfig is null without an endpoint", () => {
  assert.equal(parseConfig(JSON.stringify({ headers: { Authorization: "Bearer k" } })), null);
});

test("parseConfig is null when the endpoint is blank", () => {
  assert.equal(parseConfig(JSON.stringify({ endpoint: "  " })), null);
});

test("parseConfig is null for text that is not JSON", () => {
  assert.equal(parseConfig("not json"), null);
});

test("parseConfig is null for a JSON array", () => {
  assert.equal(parseConfig("[]"), null);
});

test("parseConfig drops a header that is not a string", () => {
  const parsed = parseConfig(
    JSON.stringify({ endpoint: "https://x", headers: { Authorization: "Bearer k", retries: 3 } }),
  );
  assert.deepEqual(parsed, { endpoint: "https://x", headers: { Authorization: "Bearer k" }, captureContent: false });
});

test("parseConfig reads a missing headers object as empty", () => {
  assert.deepEqual(parseConfig(JSON.stringify({ endpoint: "https://x" })).headers, {});
});

test("parseConfig is false for a string capture_content", () => {
  const parsed = parseConfig(JSON.stringify({ endpoint: "https://x", capture_content: "true" }));
  assert.equal(parsed.captureContent, false);
});

test("parseConfig drops the trailing slash of the endpoint", () => {
  assert.equal(parseConfig(JSON.stringify({ endpoint: "https://x/" })).endpoint, "https://x");
});

test("usageAttributes adds the cache to the input and reports it apart", () => {
  const attrs = usageAttributes({ input: 100, output: 20, reasoning: 5, cache: { read: 30, write: 10 } });
  assert.deepEqual(attrs, [
    { key: "gen_ai.usage.input_tokens", value: { intValue: "140" } },
    { key: "gen_ai.usage.output_tokens", value: { intValue: "20" } },
    { key: "gen_ai.usage.reasoning.output_tokens", value: { intValue: "5" } },
    { key: "gen_ai.usage.cache_read.input_tokens", value: { intValue: "30" } },
    { key: "gen_ai.usage.cache_creation.input_tokens", value: { intValue: "10" } },
  ]);
});

test("usageAttributes reads a missing count as zero", () => {
  const attrs = usageAttributes({ input: 100, output: 20 });
  assert.deepEqual(
    attrs.map((attr) => attr.value.intValue),
    ["100", "20", "0", "0", "0"],
  );
});

test("usageAttributes is empty without tokens", () => {
  assert.deepEqual(usageAttributes(undefined), []);
});

test("finishReason maps the two dashed values", () => {
  assert.equal(finishReason("tool-calls"), "tool_calls");
  assert.equal(finishReason("content-filter"), "content_filter");
});

test("finishReason keeps any other value and drops undefined", () => {
  assert.equal(finishReason("stop"), "stop");
  assert.equal(finishReason("other"), "other");
  assert.equal(finishReason(undefined), undefined);
});
