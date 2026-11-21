// The docs MCP server (D-AO-13): stateless, read-only, no auth, nothing logged. The Worker
// (worker/index.mjs) calls `handle` for POST /mcp. This file imports only docs-search.mjs, the
// SDK, minisearch and zod, so that the Worker bundle loads without nodejs_compat.
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { WebStandardStreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/webStandardStreamableHttp.js";
import { z } from "zod";
import { loadIndex, search } from "./docs-search.mjs";

export const MAX_PAGE_CHARS = 100_000;

/** The page path "/docs/<p>/" for a docs URL or path; undefined for anything else. */
export function pagePath(input) {
  let path = input.trim();
  if (/^https?:\/\//.test(path)) {
    let url;
    try {
      url = new URL(path);
    } catch {
      return undefined;
    }
    if (url.hostname !== "cetra.sh" && url.hostname !== "www.cetra.sh") return undefined;
    path = url.pathname;
  } else if (path.startsWith("docs/")) {
    path = `/${path}`;
  }
  path = path.split(/[?#]/)[0];
  path = path.replace(/\/index\.md$/, "/");
  if (!path.endsWith("/")) path += "/";
  if (!/^\/docs\/[a-z0-9/-]*$/.test(path) || path.includes("//")) return undefined;
  return path;
}

/** A page's Markdown cut to MAX_PAGE_CHARS, with a final note line when cut. */
export function cut(markdown) {
  if (markdown.length <= MAX_PAGE_CHARS) return markdown;
  return `${markdown.slice(0, MAX_PAGE_CHARS)}\n\n[cut at ${MAX_PAGE_CHARS} characters]`;
}

// The search index, loaded from /mcp-index.json once per isolate. A failed load is not kept.
let cached;

/** For tests: forget the cached index. */
export function resetIndexCache() {
  cached = undefined;
}

async function index(request, assets) {
  if (cached === undefined) {
    const response = await assets.fetch(new Request(new URL("/mcp-index.json", request.url)));
    if (response.status !== 200) throw new Error("search index missing");
    cached = loadIndex(await response.text());
  }
  return cached;
}

const READ_ONLY = { readOnlyHint: true, openWorldHint: false };

/** Handles one HTTP request. `assets.fetch(request)` reads the static files. A new server and
 *  transport are made per request: the SDK refuses to reuse a stateless transport. */
export async function handle(request, assets) {
  if (request.method !== "POST")
    return new Response(null, { status: 405, headers: { Allow: "POST" } });

  const server = new McpServer(
    { name: "cetra-docs", version: "1" },
    { instructions: "Search and read the cetra documentation (https://cetra.sh/docs/)." },
  );
  server.registerTool(
    "search_docs",
    {
      description:
        "Search the cetra documentation. Returns pages with a URL and a one-line summary; read one with get_page.",
      inputSchema: {
        query: z.string().min(1).max(200),
        limit: z.number().int().min(1).max(20).optional(),
      },
      annotations: READ_ONLY,
    },
    async ({ query, limit }) => ({
      content: [
        {
          type: "text",
          text: JSON.stringify(search(await index(request, assets), query, limit ?? 8)),
        },
      ],
    }),
  );
  server.registerTool(
    "get_page",
    {
      description:
        "Read one cetra documentation page as Markdown. `url` is a https://cetra.sh/docs/… URL or a /docs/… path.",
      inputSchema: { url: z.string().min(1).max(300) },
      annotations: READ_ONLY,
    },
    async ({ url }) => {
      const path = pagePath(url);
      if (path === undefined) {
        return {
          isError: true,
          content: [
            { type: "text", text: "not a cetra docs page: give a https://cetra.sh/docs/… URL" },
          ],
        };
      }
      const response = await assets.fetch(new Request(new URL(`${path}index.md`, request.url)));
      // Only 200 counts: a missing page comes back as the 404 page's body.
      if (response.status !== 200) {
        return { isError: true, content: [{ type: "text", text: `no such page: ${path}` }] };
      }
      return { content: [{ type: "text", text: cut(await response.text()) }] };
    },
  );

  const transport = new WebStandardStreamableHTTPServerTransport({
    sessionIdGenerator: undefined,
    enableJsonResponse: true,
  });
  await server.connect(transport);
  return transport.handleRequest(request);
}
