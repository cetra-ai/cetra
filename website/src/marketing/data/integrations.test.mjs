import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { test } from "node:test";
import { groups, loadIntegrations, parseIntegrations } from "./integrations.mjs";

const card = (href, title = "X") => `<LinkCard title="${title}" description="d" href="${href}" />`;

test("parses the real page", () => {
  const items = loadIntegrations();
  assert.equal(items.length, 17);
  assert.deepEqual(groups(items), [
    "Providers",
    "Frameworks",
    "Instrumentation libraries",
    "Migrating",
  ]);

  const openai = items.find((item) => item.title === "OpenAI");
  assert.equal(openai.python, true);
  assert.equal(openai.typescript, true);
  assert.equal(openai.tested, "captured");

  const bedrock = items.find((item) => item.title === "Amazon Bedrock");
  assert.equal(bedrock.typescript, false);
  assert.equal(bedrock.tested, "SDK test");
});

test("every href has a docs page", () => {
  for (const item of loadIntegrations()) {
    const slug = item.href.split("/").filter(Boolean).at(-1);
    const base = `src/content/docs/docs/integrations/${slug}`;
    assert.ok(
      existsSync(`${base}.mdx`) || existsSync(`${base}.md`),
      `no docs page for ${item.href}`,
    );
  }
});

test("order follows the page", () => {
  const items = loadIntegrations();
  assert.equal(items[0].title, "OpenAI");
  assert.equal(items.at(-1).title, "From Laminar");
});

test("a card without a row throws", () => {
  const mdx = `## Providers\n\n${card("/docs/integrations/x/")}\n`;
  assert.throws(() => parseIntegrations(mdx), /no table row for card \/docs\/integrations\/x\//);
});

test("a row without a card throws", () => {
  const mdx = "| [X](/docs/integrations/x/) | ✓ | — | how | captured |\n";
  assert.throws(() => parseIntegrations(mdx), /no card for table row \/docs\/integrations\/x\//);
});

test("a malformed table row throws", () => {
  const mdx = "| [X](/docs/integrations/x/) | yes | ✓ | how | captured |\n";
  assert.throws(() => parseIntegrations(mdx), /malformed table row at line 1/);
});
