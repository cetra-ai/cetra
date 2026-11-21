import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";
import MiniSearch from "minisearch";
import { documentFor } from "./docs-index.mjs";
import { cut, handle, MAX_PAGE_CHARS, pagePath, resetIndexCache } from "./docs-mcp.mjs";
import { INDEX_OPTIONS } from "./docs-search.mjs";

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

/** The search index the build would write for PAGES. */
function indexJson() {
  const index = new MiniSearch(INDEX_OPTIONS);
  index.addAll(Object.entries(PAGES).map(([url, markdown]) => documentFor(url, markdown)));
  return JSON.stringify(index);
}

/** Static files as the ASSETS binding serves them: a 404 page with status 404 for anything else. */
function assetsWith(files) {
  return {
    fetch: async (request) => {
      const { pathname } = new URL(request.url);
      if (files.has(pathname)) {
        return new Response(files.get(pathname), {
          status: 200,
          headers: { "Content-Type": "text/plain; charset=utf-8" },
        });
      }
      return new Response("<!doctype html><title>Not found</title>", {
        status: 404,
        headers: { "Content-Type": "text/html; charset=utf-8" },
      });
    },
  };
}

function allFiles() {
  const files = new Map([["/mcp-index.json", indexJson()]]);
  for (const [url, markdown] of Object.entries(PAGES)) files.set(`${url}index.md`, markdown);
  return files;
}

const ASSETS = assetsWith(allFiles());

/** One MCP request, as a fresh server and transport, the way the Worker makes it. */
function post(body, assets = ASSETS) {
  return handle(
    new Request("https://cetra.sh/mcp", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Accept: "application/json, text/event-stream",
      },
      body: JSON.stringify({ jsonrpc: "2.0", ...body }),
    }),
    assets,
  );
}

async function rpc(method, params, assets) {
  const response = await post({ id: 1, method, params }, assets);
  assert.equal(response.status, 200);
  return response.json();
}

async function callTool(name, args, assets) {
  const reply = await rpc("tools/call", { name, arguments: args }, assets);
  return reply.result;
}

beforeEach(() => resetIndexCache());

test("pagePath gives the page path for a docs URL or path", () => {
  assert.equal(
    pagePath("https://cetra.sh/docs/get-started/quickstart/"),
    "/docs/get-started/quickstart/",
  );
  assert.equal(
    pagePath("https://cetra.sh/docs/get-started/quickstart/index.md"),
    "/docs/get-started/quickstart/",
  );
  assert.equal(
    pagePath("https://cetra.sh/docs/get-started/quickstart#install"),
    "/docs/get-started/quickstart/",
  );
  assert.equal(pagePath("http://www.cetra.sh/docs/x"), "/docs/x/");
  assert.equal(pagePath("/docs/x"), "/docs/x/");
  assert.equal(pagePath("docs/x/"), "/docs/x/");
  assert.equal(pagePath("/docs/"), "/docs/");
  assert.equal(pagePath("https://cetra.sh:443/docs/x/"), "/docs/x/");
});

test("pagePath refuses anything that is not a docs page on cetra.sh", () => {
  assert.equal(pagePath("https://evil.example/docs/x/"), undefined);
  assert.equal(pagePath("/docs/../x/"), undefined);
  assert.equal(pagePath("/docs/../x"), undefined);
  assert.equal(pagePath("/docs/X/"), undefined);
  assert.equal(pagePath("/docs/xindex.md"), undefined);
  assert.equal(pagePath("/blog/"), undefined);
  assert.equal(pagePath(""), undefined);
});

test("cut keeps a page up to the limit and cuts a longer one with a note", () => {
  const exact = "a".repeat(MAX_PAGE_CHARS);
  assert.equal(cut(exact), exact);
  const cutText = cut("a".repeat(MAX_PAGE_CHARS + 1));
  assert.equal(cutText, `${"a".repeat(MAX_PAGE_CHARS)}\n\n[cut at 100000 characters]`);
});

test("GET /mcp answers 405 with Allow: POST", async () => {
  const response = await handle(new Request("https://cetra.sh/mcp", { method: "GET" }), ASSETS);
  assert.equal(response.status, 405);
  assert.equal(response.headers.get("Allow"), "POST");
  assert.equal(await response.text(), "");
});

test("initialize names the server cetra-docs", async () => {
  const reply = await rpc("initialize", {
    protocolVersion: "2025-06-18",
    capabilities: {},
    clientInfo: { name: "test", version: "1" },
  });
  assert.equal(reply.result.serverInfo.name, "cetra-docs");
});

test("tools/list lists exactly search_docs and get_page", async () => {
  const reply = await rpc("tools/list", {});
  assert.deepEqual(reply.result.tools.map((tool) => tool.name).sort(), ["get_page", "search_docs"]);
});

test("search_docs returns hits with a URL and the description as the snippet", async () => {
  const result = await callTool("search_docs", { query: "retention", limit: 1 }, ASSETS);
  assert.deepEqual(JSON.parse(result.content[0].text), [
    {
      title: "Retention",
      url: "https://cetra.sh/docs/platform/retention/",
      snippet: "Set how long data is kept.",
    },
  ]);
});

test("search_docs finds a misspelt query", async () => {
  const result = await callTool("search_docs", { query: "annotaton queue" }, ASSETS);
  const hits = JSON.parse(result.content[0].text);
  assert.equal(hits[0].url, "https://cetra.sh/docs/evaluation/annotation-queues/");
});

test("search_docs answers an empty array text when nothing matches", async () => {
  const result = await callTool("search_docs", { query: "zzqqxx" }, ASSETS);
  assert.equal(result.content[0].text, "[]");
});

test("search_docs rejects an empty query", async () => {
  const result = await callTool("search_docs", { query: "" }, ASSETS);
  assert.equal(result.isError, true);
});

test("get_page returns a known page as Markdown", async () => {
  const result = await callTool(
    "get_page",
    { url: "https://cetra.sh/docs/integrations/langchain/" },
    ASSETS,
  );
  assert.equal(result.isError, undefined);
  assert.equal(result.content[0].text, PAGES["/docs/integrations/langchain/"]);
});

test("get_page answers a tool error for an unknown page", async () => {
  const result = await callTool("get_page", { url: "/docs/no-such-page/" }, ASSETS);
  assert.equal(result.isError, true);
  assert.match(result.content[0].text, /no such page/);
});

test("get_page answers a tool error for a URL off the docs", async () => {
  const result = await callTool("get_page", { url: "https://evil.example/docs/x/" }, ASSETS);
  assert.equal(result.isError, true);
  assert.match(result.content[0].text, /not a cetra docs page/);
});

test("search_docs answers a tool error when the index is missing, and a later call finds hits", async () => {
  const withoutIndex = assetsWith(new Map());
  const failed = await callTool("search_docs", { query: "retention" }, withoutIndex);
  assert.equal(failed.isError, true);
  assert.match(failed.content[0].text, /search index missing/);

  const later = await callTool("search_docs", { query: "retention" }, ASSETS);
  assert.equal(later.isError, undefined);
  assert.equal(JSON.parse(later.content[0].text)[0].title, "Retention");
});
