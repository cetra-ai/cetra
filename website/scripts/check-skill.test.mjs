import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { checkSkill, cliCommands, nodeCliCommands, realContext } from "./check-skill.mjs";

const valid = [
  "---",
  "name: cetra",
  "description: Add tracing.",
  "---",
  "",
  "See [x](references/ok.md) and https://cetra.sh/docs/integrations/claude-code/.",
  "Run `cetra query`.",
  "",
].join("\n");

const ctx = {
  referenceExists: (name) => name === "ok",
  pageExists: (path) => path === "integrations/claude-code/",
  commands: new Set(["query"]),
  nodeCommands: new Set(["init", "query"]),
};

const check = (text, context = ctx) => checkSkill(text, context);

test("accepts a valid minimal skill", () => {
  assert.deepEqual(check(valid), []);
});

test("reports a missing frontmatter as one error", () => {
  const errors = check("# cetra\n");
  assert.equal(errors.length, 1);
  assert.equal(errors[0].line, 1);
});

test("reports a name other than cetra", () => {
  assert.equal(check(valid.replace("name: cetra", "name: other")).length, 1);
});

test("reports an empty description", () => {
  assert.equal(check(valid.replace("description: Add tracing.", 'description: ""')).length, 1);
});

test("reports a description of 1025 characters", () => {
  const long = valid.replace("description: Add tracing.", `description: ${"a".repeat(1025)}`);
  assert.equal(check(long).length, 1);
});

test("reports an unknown frontmatter key", () => {
  assert.equal(check(valid.replace("name: cetra\n", "name: cetra\nversion: 1\n")).length, 1);
});

test("reports an allowed-tools that is not a string", () => {
  const listed = valid.replace("name: cetra\n", "name: cetra\nallowed-tools:\n  - a\n");
  assert.equal(check(listed).length, 1);
});

test("accepts a compatibility key", () => {
  assert.deepEqual(check(valid.replace("name: cetra\n", "name: cetra\ncompatibility: node\n")), []);
});

test("reports a frontmatter that is a list, at line 1, without throwing", () => {
  const errors = check("---\n- a list\n---\n\n# cetra\n");
  assert.equal(errors.length, 1);
  assert.equal(errors[0].line, 1);
});

test("reports 501 lines as too long and accepts 500", () => {
  const long = valid + "filler\n".repeat(494);
  const errors = check(long);
  assert.equal(errors.length, 1);
  assert.match(errors[0].message, /501 lines/);
  assert.deepEqual(check(valid + "filler\n".repeat(493)), []);
});

test("reports a reference that does not exist", () => {
  assert.deepEqual(check(valid.replace("references/ok.md", "references/nope.md")).length, 1);
});

test("reports a relative link that is not a reference", () => {
  assert.equal(check(valid.replace("references/ok.md", "other.md")).length, 1);
});

test("reports a docs page that does not exist", () => {
  const text = `${valid}See https://cetra.sh/docs/nope/ here.\n`;
  assert.equal(check(text).length, 1);
});

test("reports a docs link without a trailing slash", () => {
  const text = `${valid}See https://cetra.sh/docs/x here.\n`;
  assert.equal(check(text, { ...ctx, pageExists: () => true }).length, 1);
});

test("does not check cetra.sh URLs outside the docs", () => {
  const text = `${valid}Call https://cetra.sh/llms.txt and https://cetra.sh/mcp.\n`;
  assert.deepEqual(check(text), []);
});

test("reports a cetra subcommand the CLI does not have", () => {
  assert.equal(check(`${valid}Run \`cetra frobnicate\`.\n`).length, 1);
});

test("ignores npx package names and prose", () => {
  const text = `${valid}Use \`npx @cetra-ai/cli query\`. cetra shows traces.\n`;
  assert.deepEqual(check(text), []);
});

test("checks cetra subcommands inside a code fence", () => {
  const text = `${valid}\`\`\`bash\ncetra frobnicate\n\`\`\`\n`;
  assert.equal(check(text).length, 1);
});

test("finds the subcommands of the CLI reference headings", () => {
  const headings = "### `cetra serve`\n#### `cetra agents setup`\n### `cetra query`\n";
  assert.deepEqual([...cliCommands(headings)], ["serve", "query"]);
});

test("finds the subcommands of the real CLI reference", () => {
  const cli = readFileSync("src/content/docs/docs/reference/cli.md", "utf8");
  assert.deepEqual(
    [...cliCommands(cli)],
    [
      "serve",
      "admin",
      "version",
      "health",
      "debug-bundle",
      "login",
      "logout",
      "query",
      "traces",
      "projects",
      "agents",
    ],
  );
});

test("accepts a docs link followed by a full stop", () => {
  const text = `${valid}See https://cetra.sh/docs/integrations/claude-code/.\n`;
  assert.deepEqual(check(text), []);
});

test("ignores link-like text inside inline code spans and fenced code", () => {
  const text = `${valid}Write \`x[0](y)\` here.\n\`\`\`md\n[a](other.md)\n\`\`\`\n`;
  assert.deepEqual(check(text), []);
});

test("reads a reference link whose target is followed by a title", () => {
  const text = valid.replace("references/ok.md", 'references/nope.md "Title"');
  const errors = check(text);
  assert.equal(errors.length, 1);
  assert.match(errors[0].message, /missing reference references\/nope\.md/);
});

test("accepts a reference link with a #fragment and checks its file name", () => {
  assert.deepEqual(check(valid.replace("references/ok.md", "references/ok.md#top")), []);
  const missing = check(valid.replace("references/ok.md", "references/nope.md#top"));
  assert.equal(missing.length, 1);
  assert.match(missing[0].message, /missing reference references\/nope\.md/);
});

test("checks cetra subcommands after a double or single quote in a fence", () => {
  const text = `${valid}\`\`\`bash\necho "cetra frob"\necho 'cetra frob'\n\`\`\`\n`;
  assert.equal(check(text).length, 2);
});

test("accepts npx @cetra-ai/cli with a version tag and cetra-cli in code", () => {
  const text = `${valid}\`npx @cetra-ai/cli@latest init\`\n\`\`\`bash\ncetra-cli query "SELECT 1"\n\`\`\`\n`;
  assert.deepEqual(check(text), []);
});

test("reports an npx @cetra-ai/cli subcommand the node CLI does not have", () => {
  const errors = check(`${valid}Run \`npx @cetra-ai/cli frob\`.\n`);
  assert.equal(errors.length, 1);
  assert.match(errors[0].message, /unknown command `cetra-cli frob`/);
});

test("reports a cetra-cli subcommand the node CLI does not have, in a fence", () => {
  assert.equal(check(`${valid}\`\`\`bash\ncetra-cli frob\n\`\`\`\n`).length, 1);
});

test("finds the subcommands of the cetra-cli headings", () => {
  const headings = "### `cetra-cli init`\n#### `cetra-cli x`\n### `cetra-cli query`\n";
  assert.deepEqual([...nodeCliCommands(headings)], ["init", "query"]);
});

test("finds the subcommands of the real cetra-cli reference", () => {
  const page = readFileSync("src/content/docs/docs/reference/cetra-cli.md", "utf8");
  assert.deepEqual([...nodeCliCommands(page)], ["init", "query", "login"]);
});

test("checks the real SKILL.md against the real context", () => {
  const text = readFileSync("../skills/cetra/SKILL.md", "utf8");
  assert.deepEqual(checkSkill(text, realContext()), []);
});
