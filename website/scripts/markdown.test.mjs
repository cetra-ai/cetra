import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { prepass, SITE, toMarkdown } from "./markdown.mjs";

const TABS = `<Tabs syncKey="lang">
<TabItem label="Python">

\`\`\`python
print("hi")
\`\`\`

</TabItem>
<TabItem label="TypeScript">

\`\`\`ts
console.log("hi");
\`\`\`

</TabItem>
</Tabs>`;

test("prepass turns tabs into bold labels and keeps the fences", () => {
  const out = prepass(TABS);
  assert.ok(out.includes("**Python**"));
  assert.ok(out.includes("**TypeScript**"));
  assert.ok(out.includes('```python\nprint("hi")\n```'));
  assert.ok(out.includes("```ts\nconsole.log"));
  assert.doesNotMatch(out, /<\/?Tab/);
});

test("prepass drops sample markers and MDX comments", () => {
  const out = prepass(
    "Text.\n{/* untested: reason */}\n{/* test */}\n{/* example:quick-start */}\nMore.",
  );
  assert.equal(out, "Text.\nMore.");
});

test("prepass drops a one-line Backing element", () => {
  const out = prepass('Before.\n<Backing fixtures={["a/b"]} tests={["x"]} />\nAfter.');
  assert.equal(out, "Before.\nAfter.");
});

test("prepass drops a Backing element that spans three lines", () => {
  const out = prepass('Before.\n<Backing\n  fixtures={["a/b"]}\n  tests={["x"]}\n/>\nAfter.');
  assert.equal(out, "Before.\nAfter.");
});

test("prepass drops the Image element", () => {
  const out = prepass('Before.\n<Image src={traceView} alt="The trace view" />\nAfter.');
  assert.equal(out, "Before.\nAfter.");
});

test("prepass turns a card grid into a Markdown list", () => {
  const out = prepass(
    [
      "<CardGrid>",
      '<LinkCard title="OpenAI" description="Chat and embeddings." href="/docs/integrations/openai/" />',
      '<LinkCard title="DSPy" href="/docs/integrations/dspy/" />',
      "</CardGrid>",
    ].join("\n"),
  );
  assert.equal(
    out,
    "- [OpenAI](/docs/integrations/openai/): Chat and embeddings.\n- [DSPy](/docs/integrations/dspy/)",
  );
});

test("prepass throws on a tilde fence outside code", () => {
  assert.throws(() => prepass("Text.\n~~~bash\necho hi\n~~~"), /line 2: use backtick fences/);
});

test("prepass does not close a backtick fence on a line with a language after the backticks", () => {
  const source = "```bash\necho hi\n```bash\n~~~\n```\nAfter.";
  const out = prepass(source);
  assert.equal(out, source);
});

test("prepass leaves a fence with component-like lines untouched", () => {
  const source =
    '```mdx\n<TabItem label="X">\n{/* test */}\n<Backing fixtures={["a"]} tests={["b"]} />\n```';
  assert.equal(prepass(source), source);
});

test("prepass leaves the front matter untouched", () => {
  const source = '---\ntitle: "<Tabs>"\ndescription: "{/* test */}"\n---\nBody.';
  assert.equal(prepass(source), source);
});

test("prepass throws on a Backing element that is never closed", () => {
  assert.throws(
    () => prepass('Text.\n<Backing fixtures={["a/b"]}\n  tests={["x"]}\nMore text.'),
    /unclosed <Backing> or <Image> at line 2/,
  );
});

test("toMarkdown makes a title heading, then the description, then the body", () => {
  const out = toMarkdown(
    "---\ntitle: OpenAI\ndescription: Chat and embeddings.\n---\n\nThe body.\n",
  );
  assert.equal(out, "# OpenAI\n\nChat and embeddings.\n\nThe body.\n");
});

test("toMarkdown makes links to docs pages absolute and leaves others alone", () => {
  const out = toMarkdown(
    "---\ntitle: T\n---\n\n[Traces](/docs/tracing/traces/) [x](#anchor) [y](https://example.com/)\n",
  );
  assert.ok(out.includes(`[Traces](${SITE}/docs/tracing/traces/)`));
  assert.ok(out.includes("[x](#anchor)"));
  assert.ok(out.includes("[y](https://example.com/)"));
});

test("toMarkdown removes the Starlight component imports", () => {
  const out = toMarkdown(
    `---\ntitle: T\n---\n\nimport { Tabs, TabItem } from "@astrojs/starlight/components";\n\nText.\n`,
  );
  assert.doesNotMatch(out, /import /);
  assert.ok(out.includes("Text."));
});

test("toMarkdown keeps placeholders in code and in prose", () => {
  const out = toMarkdown(
    "---\ntitle: T\n---\n\nSet `<YOUR_PASSWORD>` here.\n\n```bash\nexport KEY=<YOUR_KEY>\n```\n",
  );
  assert.ok(out.includes("`<YOUR_PASSWORD>`"));
  assert.ok(out.includes("export KEY=<YOUR_KEY>"));
});

test("toMarkdown converts an Aside to a bold note", () => {
  const out = toMarkdown("---\ntitle: T\n---\n\n<Aside>\nMind this.\n</Aside>\n");
  assert.ok(out.includes("**Note:** Mind this."));
  assert.doesNotMatch(out, /<Aside/);
});

test("toMarkdown throws on an unconverted component and names its line", () => {
  assert.throws(
    () => toMarkdown('---\ntitle: T\n---\n\nFirst.\n\n<Widget foo="1" />\n'),
    /line 5 of the converted text: unconverted component <Widget/,
  );
});

test("toMarkdown keeps the indentation of fences nested in a list item", () => {
  const out = toMarkdown("---\ntitle: T\n---\n\n1. Step\n\n   ```bash\n   ls\n   ```\n2. Next\n");
  assert.ok(out.includes("1. Step\n\n   ```bash\n   ls\n   ```\n2. Next"));
});

test("toMarkdown puts the description before a bash fence that holds a # comment", () => {
  const out = toMarkdown("---\ndescription: D\n---\n\n```bash\n# comment\n```\n");
  assert.equal(out, "D\n\n```bash\n# comment\n```\n");
});

test("toMarkdown adds no heading when the source has no front matter", () => {
  const out = toMarkdown("Just text.\n");
  assert.equal(out, "Just text.\n");
  assert.doesNotMatch(out, /^# /m);
});

test("toMarkdown ends with exactly one newline and has no trailing spaces", () => {
  const out = toMarkdown("---\ntitle: T\n---\n\nText.   \n\n\n\nMore.  \n\n\n");
  assert.ok(out.endsWith("\n"));
  assert.ok(!out.endsWith("\n\n"));
  assert.doesNotMatch(out, / +$/m);
  assert.doesNotMatch(out, /\n\n\n/);
});

test("toMarkdown converts the OpenAI integration page", () => {
  const source = readFileSync("src/content/docs/docs/integrations/openai.mdx", "utf8");
  const out = toMarkdown(source);
  assert.ok(out.includes("**Python**"));
  assert.ok(out.includes("**TypeScript**"));
  assert.doesNotMatch(out, /\{\/\*/);
  assert.doesNotMatch(out, /<Backing/);
  assert.doesNotMatch(out, /<Tab/);
});
