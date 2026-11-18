import { useQuery } from "@tanstack/react-query";
import { useMemo } from "react";
import { api } from "@/api/client";
import { projectRef } from "@/api/project-ref";
import { meQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { STORAGE_PREFIX } from "@/product";
import type {
  CreateDashboard,
  Dashboard,
  DashboardSummary,
  Definition,
  UpdateDashboard,
} from "./types";
import { type FieldIssue, validateDashboard } from "./validate";

/** Page size of the API list. The server allows up to 100. */
const DASHBOARD_PAGE = 100;
/** A list that repeats a cursor is an error, not a loop. */
const DASHBOARD_MAX_PAGES = 10;
/** The server keeps at most this many dashboards per project (spec 04). */
const MAX_DASHBOARDS = 100;

/**
 * Where a project's dashboards are kept: the dashboards API, or the browser's localStorage for a
 * demo-org viewer (D-C-17). Both answer the same shapes and throw the same problems.
 */
export type DashboardStore = {
  list(): Promise<DashboardSummary[]>;
  get(id: string): Promise<Dashboard>;
  create(body: CreateDashboard): Promise<Dashboard>;
  /** `version` is the version the caller read. The local store has one writer, so it never refuses one. */
  update(id: string, body: UpdateDashboard): Promise<Dashboard>;
  remove(id: string): Promise<void>;
};

/** The dashboards API, read page by page until `next_cursor` is null. */
export function apiStore(org: string, project: string): DashboardStore {
  const path = { project: projectRef(org, project) };
  return {
    async list() {
      const dashboards: DashboardSummary[] = [];
      const seen = new Set<string>();
      let cursor: string | undefined;
      for (let page = 1; page <= DASHBOARD_MAX_PAGES; page += 1) {
        const { data, error } = await api.GET("/api/v1/projects/{project}/dashboards", {
          params: { path, query: { limit: DASHBOARD_PAGE, cursor } },
        });
        if (error) {
          throw error;
        }
        if (!data) {
          throw new Error("Empty dashboard list response");
        }
        dashboards.push(...data.data);
        cursor = data.next_cursor ?? undefined;
        if (cursor === undefined) {
          return dashboards;
        }
        if (seen.has(cursor)) {
          throw new Error("Dashboard list repeated a page cursor");
        }
        seen.add(cursor);
      }
      throw new Error("Dashboard list has too many pages");
    },
    async get(id) {
      const { data, error } = await api.GET("/api/v1/projects/{project}/dashboards/{id}", {
        params: { path: { project: path.project, id } },
      });
      if (error) {
        throw error;
      }
      if (!data) {
        throw new Error("Empty dashboard response");
      }
      return data;
    },
    async create(body) {
      const { data, error } = await api.POST("/api/v1/projects/{project}/dashboards", {
        params: { path },
        body,
      });
      if (error) {
        throw error;
      }
      if (!data) {
        throw new Error("Empty dashboard response");
      }
      return data;
    },
    async update(id, body) {
      const { data, error } = await api.PATCH("/api/v1/projects/{project}/dashboards/{id}", {
        params: { path: { project: path.project, id } },
        body,
      });
      if (error) {
        throw error;
      }
      if (!data) {
        throw new Error("Empty dashboard response");
      }
      return data;
    },
    async remove(id) {
      const { error } = await api.DELETE("/api/v1/projects/{project}/dashboards/{id}", {
        params: { path: { project: path.project, id } },
      });
      if (error) {
        throw error;
      }
    },
  };
}

/** A dashboard as the browser keeps it. */
type LocalDashboard = Dashboard;

/** Writes that failed because localStorage refused them, for this page only. */
const unsaved = new Map<string, string>();

function readText(key: string): string | null {
  const pending = unsaved.get(key);
  if (pending !== undefined) {
    return pending;
  }
  try {
    return localStorage.getItem(key);
  } catch {
    // Storage is blocked (private mode, cleared site data): the list reads as empty.
    return null;
  }
}

function writeText(key: string, text: string): void {
  try {
    localStorage.setItem(key, text);
    unsaved.delete(key);
  } catch {
    // Storage refused the write: this page keeps the list in memory until a write succeeds.
    unsaved.set(key, text);
  }
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * The stored list, or an empty one when the stored text does not parse or a record does not
 * pass the dashboard rules. A record that breaks the rules would break the page it renders on.
 */
function readDashboards(key: string): LocalDashboard[] {
  const text = readText(key);
  if (text === null) {
    return [];
  }
  try {
    const value: unknown = JSON.parse(text);
    if (!Array.isArray(value)) {
      return [];
    }
    const valid = value.every(
      (item): item is LocalDashboard =>
        isObject(item) &&
        typeof item.id === "string" &&
        typeof item.name === "string" &&
        typeof item.version === "number" &&
        typeof item.created_at === "string" &&
        typeof item.updated_at === "string" &&
        isObject(item.definition) &&
        validateDashboard({
          name: item.name,
          description: typeof item.description === "string" ? item.description : undefined,
          definition: item.definition as Definition,
        }).length === 0,
    );
    return valid ? value : [];
  } catch {
    // A value that does not parse (or whose rules throw while checking it) reads as empty.
    return [];
  }
}

function writeDashboards(key: string, dashboards: LocalDashboard[]): void {
  writeText(key, JSON.stringify(dashboards));
}

/** A problem shaped like the server's, so the edit dialogs show it the same way. */
function problem(
  status: number,
  code: Schema<"Problem">["code"],
  title: string,
  detail?: string,
  errors: FieldIssue[] = [],
): Schema<"Problem"> {
  return {
    type: "about:blank",
    title,
    status,
    code,
    ...(detail !== undefined ? { detail } : {}),
    ...(errors.length > 0 ? { errors } : {}),
  };
}

/** Trimmed text, or undefined when it is empty: the server stores an empty description as none. */
function trimmedOrUndefined(text: string): string | undefined {
  const trimmed = text.trim();
  return trimmed === "" ? undefined : trimmed;
}

/**
 * The dashboards of a demo-org viewer, kept in this browser (D-C-17, spec 11 § Dashboard). The
 * rules and the errors are the server's: 04 § Dashboards, 400 with paths, 404, and 409 for the
 * 100-dashboard cap.
 */
export function localStore(org: string, project: string): DashboardStore {
  const key = `${STORAGE_PREFIX}dashboards.${org}/${project}`;
  return {
    async list() {
      return readDashboards(key).map(({ definition: _definition, ...summary }) => summary);
    },
    async get(id) {
      const found = readDashboards(key).find((item) => item.id === id);
      if (found === undefined) {
        throw problem(404, "not_found", "Not Found");
      }
      return found;
    },
    async create(body) {
      const name = body.name.trim();
      const description =
        body.description === undefined ? undefined : trimmedOrUndefined(body.description);
      const issues = validateDashboard({ name, description, definition: body.definition });
      if (issues.length > 0) {
        throw problem(400, "bad_request", "Bad Request", undefined, issues);
      }
      const dashboards = readDashboards(key);
      if (dashboards.length >= MAX_DASHBOARDS) {
        throw problem(409, "conflict", "Conflict", "too many dashboards");
      }
      const now = new Date().toISOString();
      const dashboard: LocalDashboard = {
        id: uuidv7(),
        name,
        ...(description === undefined ? {} : { description }),
        definition: body.definition,
        version: 1,
        created_at: now,
        updated_at: now,
      };
      writeDashboards(key, [...dashboards, dashboard]);
      return dashboard;
    },
    async update(id, body) {
      const dashboards = readDashboards(key);
      const index = dashboards.findIndex((item) => item.id === id);
      const current = dashboards[index];
      if (current === undefined) {
        throw problem(404, "not_found", "Not Found");
      }
      const name = body.name === undefined ? current.name : body.name.trim();
      const description =
        body.description === undefined ? current.description : trimmedOrUndefined(body.description);
      const definition = body.definition ?? current.definition;
      const issues = validateDashboard({ name, description, definition });
      if (issues.length > 0) {
        throw problem(400, "bad_request", "Bad Request", undefined, issues);
      }
      const next: LocalDashboard = {
        id: current.id,
        name,
        ...(description === undefined ? {} : { description }),
        definition,
        version: current.version + 1,
        created_at: current.created_at,
        updated_at: new Date().toISOString(),
      };
      writeDashboards(
        key,
        dashboards.map((item, at) => (at === index ? next : item)),
      );
      return next;
    },
    async remove(id) {
      const dashboards = readDashboards(key);
      if (!dashboards.some((item) => item.id === id)) {
        throw problem(404, "not_found", "Not Found");
      }
      writeDashboards(
        key,
        dashboards.filter((item) => item.id !== id),
      );
    },
  };
}

/**
 * A UUIDv7: a 48-bit Unix time in milliseconds, then random bits (RFC 9562), lowercase hex. Ids made
 * at later times sort later. `now` is for tests.
 */
export function uuidv7(now: number = Date.now()): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  let millis = now;
  for (let at = 5; at >= 0; at -= 1) {
    bytes[at] = millis % 256;
    millis = Math.floor(millis / 256);
  }
  bytes[6] = ((bytes[6] ?? 0) & 0x0f) | 0x70;
  bytes[8] = ((bytes[8] ?? 0) & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/**
 * The store of the open project: the local one for a demo-org viewer, else the API. `local` tells
 * the components the store is the browser's, so the editing controls are shown to that viewer.
 */
export function useDashboardStore(
  org: string,
  project: string,
): { store: DashboardStore; local: boolean } {
  const { data: me } = useQuery(meQuery);
  const membership = me?.orgs.find((entry) => entry.slug === org);
  const local = membership?.demo === true && membership.role === "viewer";
  return useMemo(
    () => ({ store: local ? localStore(org, project) : apiStore(org, project), local }),
    [local, org, project],
  );
}
