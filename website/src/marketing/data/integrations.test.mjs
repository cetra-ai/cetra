import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { test } from "node:test";
import {
  groups,
  LANGUAGE_CHIPS,
  loadIntegrations,
  MIGRATING_GROUP,
  monogram,
  parseIntegrations,
} from "./integrations.mjs";

const card = (href, title = "X") => `<LinkCard title="${title}" description="d" href="${href}" />`;
const row = (href, tested = "captured", how = "how") =>
  `| [X](${href}) | ✓ | — | ${how} | ${tested} |`;

test("parses the real page", () => {
  const items = loadIntegrations();
  assert.ok(items.length >= 1);
  assert.deepEqual(groups(items), [
    "Providers",
    "Gateways",
    "Frameworks",
    "Apps and tools",
    "Coding agents",
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
  assert.deepEqual(openai.languages, ["python", "typescript"]);
  assert.deepEqual(openai.tests, [{ kind: "recorded", lang: null }]);

  const bedrock = items.find((item) => item.title === "Amazon Bedrock");
  assert.deepEqual(bedrock.languages, ["python"]);
  assert.deepEqual(bedrock.tests, [{ kind: "sdk", lang: null }]);
});

test("a per-language test cell is parsed per language", () => {
  const langchain = loadIntegrations().find((item) => item.title === "LangChain");
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
  assert.equal(items[0].title, "Amazon Bedrock");
  assert.equal(items.at(-1).title, "From Langfuse");
});

test("a hand-written test cell is parsed", () => {
  const copilot = loadIntegrations().find((item) => item.title === "GitHub Copilot");
  assert.deepEqual(copilot.languages, []);
  assert.deepEqual(copilot.tests, [{ kind: "handwritten", lang: null }]);
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
      languages: ["python"],
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

test("a setup-only row without a card is left out", () => {
  const mdx = `${card("/docs/integrations/x/")}\n${row("/docs/integrations/x/")}\n${row("/docs/integrations/y/", "setup only")}\n`;
  const items = parseIntegrations(mdx);
  assert.deepEqual(
    items.map((item) => item.href),
    ["/docs/integrations/x/"],
  );
});

test("a card on a setup-only row throws", () => {
  const mdx = `${card("/docs/integrations/y/")}\n${row("/docs/integrations/y/", "setup only")}\n`;
  assert.throws(() => parseIntegrations(mdx), /card for setup-only row \/docs\/integrations\/y\//);
});

test("a tested row without a card throws", () => {
  const mdx = `${row("/docs/integrations/z/", "SDK test")}\n`;
  assert.throws(() => parseIntegrations(mdx), /no card for table row \/docs\/integrations\/z\//);
});

test("the real page's languages", () => {
  const items = loadIntegrations();
  const languages = (title) => items.find((item) => item.title === title).languages;
  assert.deepEqual(languages("OpenAI"), ["python", "typescript"]);
  assert.deepEqual(languages("Amazon Bedrock"), ["python"]);
  assert.deepEqual(languages("Vercel AI SDK"), ["typescript"]);
  assert.deepEqual(languages("Spring AI"), ["java"]);
  assert.deepEqual(languages("Koog"), ["kotlin"]);
  assert.deepEqual(languages("Microsoft.Extensions.AI"), ["dotnet"]);
  assert.deepEqual(languages("Claude Code"), []);
  assert.deepEqual(languages("Bifrost"), []);
});

test("every real integration's languages are known and unique", () => {
  for (const item of loadIntegrations()) {
    for (const language of item.languages) {
      assert.ok(language in LANGUAGE_CHIPS, `unknown language ${language} on ${item.title}`);
    }
    assert.equal(
      new Set(item.languages).size,
      item.languages.length,
      `duplicate language on ${item.title}`,
    );
  }
});

test("a How cell's leading language is read", () => {
  const mdx = `## Providers\n\n${card("/docs/integrations/x/")}\n| [X](/docs/integrations/x/) | — | — | Java, a starter | captured |\n`;
  assert.deepEqual(parseIntegrations(mdx)[0].languages, ["java"]);
});

test("a language named later in the How cell is not read", () => {
  const mdx = `## Providers\n\n${card("/docs/integrations/x/")}\n| [X](/docs/integrations/x/) | — | — | uses Java, sometimes | captured |\n`;
  assert.deepEqual(parseIntegrations(mdx)[0].languages, []);
});

test("the columns come before the How language", () => {
  const mdx = `## Providers\n${card("/docs/integrations/x/")}\n${row("/docs/integrations/x/", "captured", ".NET, x")}\n`;
  assert.deepEqual(parseIntegrations(mdx)[0].languages, ["python", "dotnet"]);
});

test("no item has the old booleans", () => {
  for (const item of loadIntegrations()) {
    assert.equal("python" in item, false, `${item.title} still has python`);
    assert.equal("typescript" in item, false, `${item.title} still has typescript`);
  }
});

test("monogram", () => {
  assert.equal(monogram("OpenAI"), "O");
  assert.equal(monogram(".NET thing"), "N");
  assert.equal(monogram("9router"), "9");
  assert.equal(monogram("…"), "?");
  assert.equal(monogram("élan"), "L");
});
