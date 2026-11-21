// The cetra-site Worker (D-W-3, D-AO-13): /mcp is the docs MCP server; every other path is a
// static asset and never reaches this script (assets.run_worker_first).
import { handle } from "../scripts/docs-mcp.mjs";

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname !== "/mcp") return env.ASSETS.fetch(request);
    return handle(request, env.ASSETS);
  },
};
