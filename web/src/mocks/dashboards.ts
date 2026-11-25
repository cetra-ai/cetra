import { type HttpHandler, HttpResponse, http } from "msw";
import type { Dashboard, Definition } from "@/lib/dashboards/types";
import { validateDashboard, validateDefinition } from "@/lib/dashboards/validate";
import { uuidv7 } from "@/lib/uuid";
import dashboardDocument from "../../../fixtures/api/static/dashboard.json";
import staleDocument from "../../../fixtures/api/static/dashboard-stale.json";
import dashboardsDocument from "../../../fixtures/api/static/dashboards.json";
import meDocument from "../../../fixtures/api/static/me.json";
import { projectOfRef } from "./projects";
import { STORY_DASHBOARD_STORIES } from "./story-dashboards";

const LIST = "/api/v1/projects/:project/dashboards";
const ONE = "/api/v1/projects/:project/dashboards/:id";
const OVERVIEW = "/api/v1/projects/:project/dashboards/overview";
/** The server keeps at most this many dashboards per project (spec 04). */
const MAX_DASHBOARDS = 100;
/** Mirrors the server's `DashboardSummary` page size default. */
const DEFAULT_PAGE = 50;

let dashboards: Dashboard[] = [];
/** The project's stored Overview; undefined while it shows the built-in preset. */
let storedOverview: Dashboard | undefined;

/** The fixture dashboards: the list entries, with the definition of the one that has it. */
export function seedDashboards(): Dashboard[] {
  return dashboardsDocument.data.map((summary) => ({
    ...summary,
    definition:
      summary.id === dashboardDocument.id
        ? structuredClone(dashboardDocument.definition as Definition)
        : { widgets: [] },
  }));
}

/**
 * Handlers for one in-memory store. Calling this replaces the store with `initial`, and the
 * stored Overview with `overview` (absent: the project shows the preset).
 */
export function dashboardHandlers(
  initial: readonly Dashboard[],
  overview?: Dashboard,
): HttpHandler[] {
  dashboards = initial.map((dashboard) => structuredClone(dashboard));
  storedOverview = overview === undefined ? undefined : structuredClone(overview);
  return handlers;
}

const handlers: HttpHandler[] = [
  http.get(LIST, ({ request, params }) => {
    const url = new URL(request.url);
    const limit = Number(url.searchParams.get("limit") ?? DEFAULT_PAGE);
    const offset = Number(url.searchParams.get("cursor") ?? 0);
    const sorted = [...dashboards].filter((item) => visibleIn(item, params.project)).sort(byName);
    const page = sorted.slice(offset, offset + limit).map(summaryOf);
    const next = offset + limit < sorted.length ? String(offset + limit) : null;
    return HttpResponse.json({ data: page, next_cursor: next });
  }),
  http.post(LIST, async ({ request }) => {
    const body = await readJson(request);
    if (!body) {
      return problem(400, "bad_request", "Bad Request");
    }
    const name = typeof body.name === "string" ? body.name : "";
    const description = typeof body.description === "string" ? body.description : undefined;
    const definition = asDefinition(body.definition);
    if (!definition) {
      return problem(400, "bad_request", "Bad Request");
    }
    const errors = validateDashboard({ name, description, definition });
    if (errors.length > 0) {
      return problem(400, "bad_request", "Bad Request", errors);
    }
    if (dashboards.length >= MAX_DASHBOARDS) {
      return problem(409, "conflict", "Conflict", [], "too many dashboards");
    }
    const now = new Date().toISOString();
    const trimmed = description?.trim() ?? "";
    const dashboard: Dashboard = {
      id: uuidv7(),
      name: name.trim(),
      ...(trimmed === "" ? {} : { description: trimmed }),
      definition,
      version: 1,
      created_by: meDocument.user.id,
      updated_by: meDocument.user.id,
      created_at: now,
      updated_at: now,
    };
    dashboards.push(dashboard);
    return HttpResponse.json(dashboard, { status: 201 });
  }),
  // The Overview's routes come before `ONE`, so `overview` is never read as an id.
  http.get(OVERVIEW, () => (storedOverview ? HttpResponse.json(storedOverview) : notFound())),
  http.put(OVERVIEW, async ({ request }) => {
    const body = await readJson(request);
    if (!body || (body.version !== undefined && typeof body.version !== "number")) {
      return problem(400, "bad_request", "Bad Request");
    }
    const definition = asDefinition(body.definition);
    if (!definition) {
      return problem(400, "bad_request", "Bad Request");
    }
    const errors = validateDefinition(definition);
    if (errors.length > 0) {
      return problem(400, "bad_request", "Bad Request", errors);
    }
    // Without a stored Overview, the first save creates it at version 1 and names no version; a
    // stored one is saved at the version the caller read. Anything else is a 409 with the stored one.
    const stored = storedOverview?.version ?? 0;
    if (storedOverview === undefined && body.version === undefined) {
      const now = new Date().toISOString();
      storedOverview = {
        id: uuidv7(),
        name: "Overview",
        definition,
        version: 1,
        created_by: meDocument.user.id,
        updated_by: meDocument.user.id,
        created_at: now,
        updated_at: now,
      };
      return HttpResponse.json(storedOverview, { status: 201 });
    }
    if (storedOverview === undefined || body.version !== stored) {
      return staleOverview(stored);
    }
    storedOverview = {
      ...storedOverview,
      definition,
      version: stored + 1,
      updated_by: meDocument.user.id,
      updated_at: new Date().toISOString(),
    };
    return HttpResponse.json(storedOverview);
  }),
  http.delete(OVERVIEW, () => {
    storedOverview = undefined;
    return new HttpResponse(null, { status: 204 });
  }),
  http.get(ONE, ({ params }) => {
    const dashboard = dashboards.find(
      (item) => item.id === params.id && visibleIn(item, params.project),
    );
    return dashboard ? HttpResponse.json(dashboard) : notFound();
  }),
  http.patch(ONE, async ({ params, request }) => {
    const dashboard = dashboards.find(
      (item) => item.id === params.id && visibleIn(item, params.project),
    );
    const body = await readJson(request);
    if (!body || typeof body.version !== "number") {
      return problem(400, "bad_request", "Bad Request");
    }
    const definition = body.definition === undefined ? undefined : asDefinition(body.definition);
    if (body.definition !== undefined && !definition) {
      return problem(400, "bad_request", "Bad Request");
    }
    // An omitted name is unchanged, so its rules do not apply; the placeholder is never stored.
    const errors = validateDashboard({
      name: typeof body.name === "string" ? body.name : "unchanged",
      description: typeof body.description === "string" ? body.description : undefined,
      definition,
    }).filter((issue) => issue.field !== "name" || typeof body.name === "string");
    if (errors.length > 0) {
      return problem(400, "bad_request", "Bad Request", errors);
    }
    if (!dashboard) {
      return notFound();
    }
    if (body.version !== dashboard.version) {
      return HttpResponse.json(
        {
          ...staleDocument,
          detail: `the dashboard changed; current version is ${dashboard.version}`,
        },
        { status: 409, headers: { "Content-Type": "application/problem+json" } },
      );
    }
    if (typeof body.name === "string") {
      dashboard.name = body.name.trim();
    }
    if (typeof body.description === "string") {
      const trimmed = body.description.trim();
      if (trimmed === "") {
        delete dashboard.description;
      } else {
        dashboard.description = trimmed;
      }
    }
    if (definition) {
      dashboard.definition = definition;
    }
    dashboard.version += 1;
    dashboard.updated_by = meDocument.user.id;
    dashboard.updated_at = new Date().toISOString();
    return HttpResponse.json(dashboard);
  }),
  http.delete(ONE, ({ params }) => {
    const index = dashboards.findIndex(
      (item) => item.id === params.id && visibleIn(item, params.project),
    );
    if (index < 0) {
      return notFound();
    }
    dashboards.splice(index, 1);
    return new HttpResponse(null, { status: 204 });
  }),
];

/**
 * Whether a project's dashboards list and routes show `dashboard`: a story dashboard only in its
 * story's project (P-4), every other dashboard in every project. `project` is the decoded ref.
 */
function visibleIn(dashboard: Dashboard, project: unknown): boolean {
  const story = STORY_DASHBOARD_STORIES[dashboard.id];
  return (
    story === undefined ||
    projectOfRef(typeof project === "string" ? project : undefined)?.story === story
  );
}

function byName(left: Dashboard, right: Dashboard): number {
  if (left.name !== right.name) {
    return left.name < right.name ? -1 : 1;
  }
  return left.id < right.id ? -1 : left.id > right.id ? 1 : 0;
}

function summaryOf(dashboard: Dashboard): Omit<Dashboard, "definition"> {
  const { definition: _definition, ...summary } = dashboard;
  return summary;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** A definition with its widget list, or undefined when the value is not one. */
function asDefinition(value: unknown): Definition | undefined {
  if (!isObject(value) || !Array.isArray(value.widgets)) {
    return undefined;
  }
  return structuredClone(value) as Definition;
}

async function readJson(request: Request): Promise<Record<string, unknown> | undefined> {
  try {
    const body: unknown = await request.json();
    return isObject(body) ? body : undefined;
  } catch {
    return undefined;
  }
}

/** The 409 of a save at a version other than the stored one: 0 when nothing is stored. */
function staleOverview(current: number): Response {
  return HttpResponse.json(
    {
      ...staleDocument,
      detail: `the dashboard changed; current version is ${current}`,
    },
    { status: 409, headers: { "Content-Type": "application/problem+json" } },
  );
}

function notFound(): Response {
  return problem(404, "not_found", "Not Found");
}

function problem(
  status: number,
  code: string,
  title: string,
  errors: { field: string; message: string }[] = [],
  detail?: string,
): Response {
  return HttpResponse.json(
    {
      type: "about:blank",
      title,
      status,
      code,
      ...(detail !== undefined ? { detail } : {}),
      ...(errors.length > 0 ? { errors } : {}),
    },
    { status, headers: { "Content-Type": "application/problem+json" } },
  );
}
