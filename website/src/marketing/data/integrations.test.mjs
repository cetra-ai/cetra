import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { test } from "node:test";
import { groups, loadIntegrations, MIGRATING_GROUP, parseIntegrations } from "./integrations.mjs";

const card = (href, title = "X") => `<LinkCard title="${title}" description="d" href="${href}" />`;
const row = (href, tested = "captured") => `| [X](${href}) | ✓ | — | how | ${tested} |`;

test("parses the real page", () => {
  const items = loadIntegrations();
  assert.ok(items.length >= 1);
  assert.deepEqual(groups(items), [
    "Providers",
    "Frameworks",
    "Instrumentation libraries",
    MIGRATING_GROUP,
  ]);
  for (const group of groups(items)) {
    assert.ok(
      items.some((item) => item.group === group),
      group,
    );
  }

  const openai = items.find((item) => item.title === "OpenAI");
  assert.equal(openai.python, true);
  assert.equal(openai.typescript, true);
  assert.deepEqual(openai.tests, [{ kind: "recorded", lang: null }]);

  const bedrock = items.find((item) => item.title === "Amazon Bedrock");
  assert.equal(bedrock.typescript, false);
  assert.deepEqual(bedrock.tests, [{ kind: "sdk", lang: null }]);
});

test("a per-language test cell is parsed per language", () => {
  const langchain = loadIntegrations().find((item) => item.title === "LangChain and LangGraph");
  assert.deepEqual(langchain.tests, [
    { kind: "recorded", lang: "Python" },
    { kind: "sdk", lang: "TypeScript" },
  ]);
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

test("CRLF line endings parse", () => {
  const mdx = `## Providers\r\n\r\n${card("/docs/integrations/x/")}\r\n${row("/docs/integrations/x/")}\r\n`;
  assert.deepEqual(parseIntegrations(mdx), [
    {
      group: "Providers",
      title: "X",
      description: "d",
      href: "/docs/integrations/x/",
      python: true,
      typescript: false,
      tests: [{ kind: "recorded", lang: null }],
    },
  ]);
});

test("trailing spaces on rows and cards parse", () => {
  const mdx = `## Providers\n${card("/docs/integrations/x/")}   \n${row("/docs/integrations/x/")}  \n`;
  const items = parseIntegrations(mdx);
  assert.equal(items.length, 1);
  assert.equal(items[0].href, "/docs/integrations/x/");
  assert.deepEqual(items[0].tests, [{ kind: "recorded", lang: null }]);
});

test("a duplicate table row throws", () => {
  const mdx = `${card("/docs/integrations/x/")}\n${row("/docs/integrations/x/")}\n${row("/docs/integrations/x/")}\n`;
  assert.throws(
    () => parseIntegrations(mdx),
    /duplicate table row \/docs\/integrations\/x\/ at line 3/,
  );
});

test("a duplicate card throws", () => {
  const mdx = `${card("/docs/integrations/x/")}\n${card("/docs/integrations/x/")}\n${row("/docs/integrations/x/")}\n`;
  assert.throws(() => parseIntegrations(mdx), /duplicate card \/docs\/integrations\/x\/ at line 2/);
});

test("an unknown tested wording throws", () => {
  const mdx = `${card("/docs/integrations/x/")}\n${row("/docs/integrations/x/", "verified")}\n`;
  assert.throws(() => parseIntegrations(mdx), /unknown test wording "verified" at line 2/);
  const bad = `${card("/docs/integrations/x/")}\n${row("/docs/integrations/x/", "captured (Go)")}\n`;
  assert.throws(() => parseIntegrations(bad), /unknown test wording "captured \(Go\)"/);
});
