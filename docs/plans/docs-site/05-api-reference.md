# W5 — The API reference, generated at build time

**Commit subject:** `Generate the API reference from openapi.json`

**Goal:** `/docs/reference/api/` documents the public REST API: a hand-written overview (base URL, authentication, errors, pagination, rate limits, stability) and one generated page per operation, rendered by `starlight-openapi` from `crates/server/openapi.json` at every build, without the control plane's `internal` routes. `/docs/reference/otlp/` documents the ingest endpoint. The Reference topic appears.

## Read first

- README § Decisions D-W-7; § Page list › Reference topic (rows `reference/`, `reference/api/`, `reference/otlp`).
- `crates/server/openapi.json` (its `info`, `tags`, a few `paths`, `components.securitySchemes`), `crates/server/src/api/openapi.rs` (where `info.description` is set), `justfile` § `openapi`.
- `docs/spec/09-api.md` (conventions, authentication, errors and problem codes, pagination, rate limits, the OTLP section); `docs/spec/02-ingestion.md` (limits, accepted encodings, 413 and 402 answers).
- After install: `website/node_modules/starlight-openapi/README.md` (0.27.0's options).

**Check first:** `python3 -c "import json;print(sorted({t for p in json.load(open('crates/server/openapi.json'))['paths'].values() for o in p.values() if isinstance(o, dict) for t in o.get('tags', [])}))"` prints a list that contains `internal`. If not, stop and report (the filter below would be wrong).

## Files

| File | Change |
|---|---|
| `website/package.json`, `pnpm-lock.yaml` | `starlight-openapi` 0.27.0; the `gen:openapi` script |
| `website/scripts/public-openapi.mjs`, `public-openapi.test.mjs` (new) | write `src/generated/openapi.json` without `internal` |
| `website/astro.config.mjs` | the plugin, the Reference topic |
| `website/src/content/docs/docs/reference/index.mdx` (new) | Reference index |
| `website/src/content/docs/docs/reference/api/index.mdx` (new) | API overview |
| `website/src/content/docs/docs/reference/otlp.mdx` (new) | OTLP endpoint |
| `crates/server/src/api/openapi.rs`, `crates/server/openapi.json`, `web/src/api/schema.gen.ts` | a user-facing `info.description` (regenerated with `just openapi`) |
| `website/src/content/docs/docs/index.mdx`, `get-started/opentelemetry.mdx` | links |

## Steps

### 1. The public OpenAPI file

`website/scripts/public-openapi.mjs` exports `publicSpec(spec)` and a `main`:

- `publicSpec` returns a deep copy where every operation whose `tags` include `"internal"` is removed, a path left with no operation is removed, and the `internal` entry is removed from the top-level `tags`. It also sets `servers: [{ url: "http://localhost:8080", description: "A local cetra; use your instance's URL" }]` when the document has none. It removes nothing else.
- `main` reads `../crates/server/openapi.json` (resolved from `import.meta.url`), writes `src/generated/openapi.json` (creating the directory), and prints the number of operations kept and dropped.

`public-openapi.test.mjs`: a tiny spec with one `internal` and one `traces` operation on the same path and one `internal`-only path; the result keeps the `traces` operation, drops the other two and the tag, adds `servers`, and leaves the input object unchanged.

`website/package.json` scripts:

```json
"gen:openapi": "node scripts/public-openapi.mjs",
"dev": "pnpm gen:openapi && astro dev",
"build": "pnpm gen:openapi && astro build",
"check": "biome check . && pnpm test:scripts && pnpm gen:openapi && astro check && pnpm check:content && astro build"
```

(pnpm does not run `pre*` scripts by default, hence the explicit calls.)

### 2. The plugin

Add `"starlight-openapi": "0.27.0"` to `dependencies`, `pnpm install` (no peer warning; if `@astrojs/markdown-satteri` is reported, add 0.4.7 as README § Tooling says).

In `astro.config.mjs`:

```js
import starlightOpenAPI, { openAPISidebarGroups } from "starlight-openapi";
```

Add to `plugins`, before the sidebar-topics plugin:

```js
starlightOpenAPI([
  {
    base: "docs/reference/api",
    schema: "./src/generated/openapi.json",
    sidebar: { label: "Endpoints", collapsed: true },
  },
]),
```

and the topic, after Docs:

```js
{
  label: "Reference",
  link: "/docs/reference/",
  icon: "document",
  items: [
    { label: "Overview", link: "/docs/reference/" },
    {
      label: "REST API",
      items: [{ label: "Overview", link: "/docs/reference/api/" }, ...openAPISidebarGroups],
    },
    { label: "OTLP endpoint", link: "/docs/reference/otlp/" },
  ],
},
```

Check the option names (`base`, `schema`, `sidebar.label`, `sidebar.collapsed`) and how `openAPISidebarGroups` is meant to be placed against the 0.27.0 README; use its names if they differ, and say so in the report. If the plugin generates its own page at `docs/reference/api/` that collides with `reference/api/index.mdx`, move the overview to `reference/api/overview.mdx` and change the links.

### 3. `info.description`

In `crates/server/src/api/openapi.rs`, the description becomes: "The cetra REST API: traces, spans, sessions, users, scores, evaluation, datasets, experiments, alerts and administration. Authenticate with an API key (`sk-cetra-…`) or a personal access token (`pat-cetra-…`) as a bearer token." Run `just openapi` (it rewrites `crates/server/openapi.json` and `web/src/api/schema.gen.ts`). The description is the generated overview's first paragraph, so it must not cite a spec.

### 4. The pages

**`reference/index.mdx`** (title "Reference"): one sentence, then a `<CardGrid>` with cards for REST API and OTLP endpoint (later phases add CLI, SDKs, attributes, filter language, SQL schema).

**`reference/api/index.mdx`** (title "REST API"), from spec 09:
- Base URL (`<public_url>/api/v1`; the UI's own `/api/auth/*` routes are listed but meant for the UI).
- Authentication: the key kinds and which routes take which (bearer `sk-cetra-` for a project's data, `pat-cetra-` for the CLI, MCP and user-level calls, cookies for the UI); a `curl` example (`bash` fence) listing traces.
- Errors: the problem-details body (`type`, `title`, `status`, `code`, `detail`, field errors) and a table of every problem `code` from spec 09 with its status and meaning.
- Pagination: the cursor (`next_cursor`), page sizes.
- Rate limits and `429`, the `402` of a plan's cap.
- Time, IDs and money conventions as the API shows them (nanoseconds since the epoch, UUIDv7, lowercase hex trace and span IDs, USD).
- Stability: "Before 1.0 the API may change between minor releases; every change is in the changelog with an upgrade note."
- A link to the raw document: say it is `crates/server/openapi.json` in the repository (GitHub URL).

**`reference/otlp.mdx`** (title "OTLP endpoint"), from spec 02 and 09: endpoints and ports; authentication; protobuf and JSON; gzip; the limits (`ingest.max_request_bytes`, `max_spans_per_request`, `max_grpc_message_bytes`, `max_attribute_bytes`, linking each to the configuration reference), and the status each limit answers; `402` when an org's plan cap is reached and what the SDKs do then; partial success; `/v1/logs` and `/v1/metrics` accepted and dropped; spans whose IDs or timestamps are invalid (what is rejected, what is fixed).

`get-started/opentelemetry.mdx`: link the endpoints table to `/docs/reference/otlp/`. `index.mdx`: a "Reference" card.

## Tests

- `public-openapi.test.mjs` passes.
- `just check-docs` passes; `website/dist/docs/reference/api/` holds one page per public operation and none for an `internal` one (`rg -l "api/internal" website/dist` finds nothing).
- `just check-web` passes (the regenerated `schema.gen.ts`).
- `cargo test -p cetra-server --test openapi` passes.

## Done when

- `just check` passes.
- Search (Pagefind) finds an endpoint by its path, for example `/api/v1/traces`.
