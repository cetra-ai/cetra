import assert from "node:assert/strict";
import { test } from "node:test";
import MiniSearch from "minisearch";
import { documentFor } from "./docs-index.mjs";
import { INDEX_OPTIONS, loadIndex, search } from "./docs-search.mjs";

const PAGES = {
  "/docs/evaluation/annotation-queues/": [
    "# Annotation queues",
    "",
    "Review traces by hand and score them.",
    "",
    "## Add an item",
    "",
    "Pick a trace and add it to the queue.",
    "",
  ].join("\n"),
  "/docs/integrations/langchain/": [
    "# LangChain",
    "",
    "Send traces from LangChain to cetra.",
    "",
    "## Install",
    "",
    "```bash",
    "## comment",
    "pip install cetra",
    "```",
    "",
  ].join("\n"),
  "/docs/platform/retention/": [
    "# Retention",
    "",
    "Set how long data is kept.",
    "",
    "## Storage",
    "",
    "Data is kept on the local disk.",
    "",
  ].join("\n"),
};

function buildIndex() {
  const index = new MiniSearch(INDEX_OPTIONS);
  index.addAll(Object.entries(PAGES).map(([url, markdown]) => documentFor(url, markdown)));
  return index;
}

test("documentFor splits a page into its title, description, headings and body", () => {
  const doc = documentFor(
    "/docs/a/",
    "# Title here\n\nFirst line.\nSecond line.\n\n## One\n\nText.\n",
  );
  assert.equal(doc.url, "/docs/a/");
  assert.equal(doc.title, "Title here");
  assert.equal(doc.description, "First line. Second line.");
  assert.equal(doc.headings, "One");
  assert.equal(doc.body, "## One\n\nText.");
});

test("documentFor ignores a heading-like line inside a bash fence", () => {
  const doc = documentFor("/docs/langchain/", PAGES["/docs/integrations/langchain/"]);
  assert.equal(doc.headings, "Install");
  assert.equal(doc.description, "Send traces from LangChain to cetra.");
  assert.match(doc.body, /## comment/);
});

test("documentFor fails on a page without a title line", () => {
  assert.throws(() => documentFor("/docs/x/", "No title here.\n"), /no "# " title line/);
});

test("an index finds a misspelt query on the right page", () => {
  const hits = search(buildIndex(), "annotaton queue", 8);
  assert.equal(hits[0].url, "https://cetra.sh/docs/evaluation/annotation-queues/");
});

test("stopwords are dropped from a query", () => {
  const hits = search(buildIndex(), "how do I keep data", 8);
  assert.equal(hits[0].url, "https://cetra.sh/docs/platform/retention/");
});

test("a query made only of stopwords still searches those words", () => {
  // "is" is a stopword, and it is the only word in the query: it must still be searched.
  const hits = search(buildIndex(), "is", 8);
  assert.equal(hits[0].url, "https://cetra.sh/docs/platform/retention/");
});

test("a long question whose words are not all on one page still finds the page", () => {
  const hits = search(buildIndex(), "how do I send traces from langchain to cetra quickly", 8);
  assert.equal(hits[0].url, "https://cetra.sh/docs/integrations/langchain/");
});

test("search returns the page URL and the description as the snippet", () => {
  const [hit] = search(buildIndex(), "retention", 8);
  assert.deepEqual(hit, {
    title: "Retention",
    url: "https://cetra.sh/docs/platform/retention/",
    snippet: "Set how long data is kept.",
  });
});

test("a serialized index loads and finds the same pages", () => {
  const json = JSON.stringify(buildIndex());
  const loaded = loadIndex(json);
  assert.deepEqual(
    search(loaded, "annotaton queue", 8),
    search(buildIndex(), "annotaton queue", 8),
  );
});
