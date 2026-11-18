import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo } from "react";
import { $api, api } from "@/api/client";
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
  /** Whether the dashboards are the browser's. It keeps their query keys apart from the API's. */
  readonly local: boolean;
  list(): Promise<DashboardSummary[]>;
  get(id: string): Promise<Dashboard>;
  create(body: CreateDashboard): Promise<Dashboard>;
  /** `version` is the version the caller read. The local store has one writer, so it never refuses one. */
  update(id: string, body: UpdateDashboard): Promise<Dashboard>;
  remove(id: string): Promise<void>;
};

/**
 * The query key of the dashboard list. The browser's list adds a trailing "local", so the two
 * never share a cache entry. Invalidating the key without it refreshes both (a prefix match).
 */
export function dashboardsKey(org: string, project: string, local: boolean) {
  const key = $api.queryOptions("get", "/api/v1/projects/{project}/dashboards", {
    params: { path: { project: projectRef(org, project) }, query: { limit: 100 } },
  }).queryKey;
  return local ? [...key, "local"] : key;
}

/** The query key of one dashboard, with the same trailing "local" as `dashboardsKey`. */
export function dashboardKey(org: string, project: string, id: string, local: boolean) {
  const key = $api.queryOptions("get", "/api/v1/projects/{project}/dashboards/{id}", {
    params: { path: { project: projectRef(org, project), id } },
  }).queryKey;
  return local ? [...key, "local"] : key;
}

/** The dashboards API, read page by page until `next_cursor` is null. */
export function apiStore(org: string, project: string): DashboardStore {
  const path = { project: projectRef(org, project) };
  return {
    local: false,
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

/** One element of the stored list: a dashboard that passes the rules, or any other element, kept as it is. */
type Slot = { kind: "record"; dashboard: LocalDashboard } | { kind: "raw"; raw: unknown };

/** The stored list, when it can be read: its elements in their stored order. */
type Readable = { readable: true; slots: Slot[] };

/** Not a JSON list, or storage is unavailable: the list reads as empty and writes are refused. */
type Stored = Readable | { readable: false };

const UNREADABLE = "Saved dashboards in this browser cannot be read; nothing was changed.";
const FULL = "Could not save in this browser: its storage is full or unavailable.";

/** The last parsed text. Unchanged text is not validated again. */
let parsed: { text: string; stored: Stored } | undefined;

/** The browser's storage key for a project's dashboards. */
export function localDashboardsKey(org: string, project: string): string {
  return `${STORAGE_PREFIX}dashboards.${org}/${project}`;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** The element as a dashboard, or undefined when it does not pass the record shape and the rules. */
function asDashboard(raw: unknown): LocalDashboard | undefined {
  if (
    !isObject(raw) ||
    typeof raw.id !== "string" ||
    typeof raw.name !== "string" ||
    typeof raw.version !== "number" ||
    typeof raw.created_at !== "string" ||
    typeof raw.updated_at !== "string" ||
    !isObject(raw.definition)
  ) {
    return undefined;
  }
  try {
    const issues = validateDashboard({
      name: raw.name,
      description: typeof raw.description === "string" ? raw.description : undefined,
      definition: raw.definition as Definition,
    });
    return issues.length === 0 ? (raw as LocalDashboard) : undefined;
  } catch {
    // A value whose rules throw while checking it is skipped like any other invalid one.
    return undefined;
  }
}

/** A list of stored elements as slots. An element that repeats an earlier id is skipped too. */
function slotsOf(items: unknown[]): Slot[] {
  const seen = new Set<string>();
  return items.map((raw): Slot => {
    const dashboard = asDashboard(raw);
    if (dashboard === undefined || seen.has(dashboard.id)) {
      return { kind: "raw", raw };
    }
    seen.add(dashboard.id);
    return { kind: "record", dashboard };
  });
}

function parseStored(text: string): Stored {
  if (parsed?.text === text) {
    return parsed.stored;
  }
  let stored: Stored;
  try {
    const value: unknown = JSON.parse(text);
    stored = Array.isArray(value) ? { readable: true, slots: slotsOf(value) } : { readable: false };
  } catch {
    stored = { readable: false };
  }
  parsed = { text, stored };
  return stored;
}

function readStored(key: string): Stored {
  let text: string | null;
  try {
    text = localStorage.getItem(key);
  } catch {
    // Storage is blocked (private mode, cleared site data): the store is unreadable.
    return { readable: false };
  }
  return text === null ? { readable: true, slots: [] } : parseStored(text);
}

/** The stored dashboards that pass the rules. An unreadable store has none. */
function records(stored: Stored): LocalDashboard[] {
  if (!stored.readable) {
    return [];
  }
  return stored.slots.flatMap((slot) => (slot.kind === "record" ? [slot.dashboard] : []));
}

/** The store as a write sees it. An unreadable one is refused, and never overwritten. */
function readForWrite(key: string): Readable {
  const stored = readStored(key);
  if (!stored.readable) {
    throw problem(409, "conflict", "Conflict", UNREADABLE);
  }
  return stored;
}

/**
 * The stored list after a write: the skipped elements stay where they were, the dashboards that
 * are still there take their place in the list, and the new ones are added at the end.
 */
function rebuild(slots: Slot[], dashboards: LocalDashboard[]): unknown[] {
  const pending = new Map(dashboards.map((item) => [item.id, item]));
  const out: unknown[] = [];
  for (const slot of slots) {
    if (slot.kind === "raw") {
      out.push(slot.raw);
      continue;
    }
    const next = pending.get(slot.dashboard.id);
    if (next !== undefined) {
      out.push(next);
      pending.delete(slot.dashboard.id);
    }
  }
  out.push(...pending.values());
  return out;
}

function writeDashboards(key: string, stored: Readable, dashboards: LocalDashboard[]): void {
  const text = JSON.stringify(rebuild(stored.slots, dashboards));
  try {
    localStorage.setItem(key, text);
  } catch {
    throw problem(507, "internal", "Insufficient Storage", FULL);
  }
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
  const key = localDashboardsKey(org, project);
  return {
    local: true,
    async list() {
      return records(readStored(key)).map(({ definition: _definition, ...summary }) => summary);
    },
    async get(id) {
      const found = records(readStored(key)).find((item) => item.id === id);
      if (found === undefined) {
        throw problem(404, "not_found", "Not Found");
      }
      return found;
    },
    async create(body) {
      const stored = readForWrite(key);
      const name = body.name.trim();
      const description =
        body.description === undefined ? undefined : trimmedOrUndefined(body.description);
      const issues = validateDashboard({ name, description, definition: body.definition });
      if (issues.length > 0) {
        throw problem(400, "bad_request", "Bad Request", undefined, issues);
      }
      const dashboards = records(stored);
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
      writeDashboards(key, stored, [...dashboards, dashboard]);
      return dashboard;
    },
    async update(id, body) {
      const stored = readForWrite(key);
      const dashboards = records(stored);
      const current = dashboards.find((item) => item.id === id);
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
        stored,
        dashboards.map((item) => (item.id === id ? next : item)),
      );
      return next;
    },
    async remove(id) {
      const stored = readForWrite(key);
      const dashboards = records(stored);
      if (!dashboards.some((item) => item.id === id)) {
        throw problem(404, "not_found", "Not Found");
      }
      writeDashboards(
        key,
        stored,
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
 * For that viewer, a change to the browser's dashboards from another tab refreshes the lists.
 */
export function useDashboardStore(
  org: string,
  project: string,
): { store: DashboardStore; local: boolean } {
  const { data: me } = useQuery(meQuery);
  const queryClient = useQueryClient();
  const membership = me?.orgs.find((entry) => entry.slug === org);
  const local = membership?.demo === true && membership.role === "viewer";
  useEffect(() => {
    if (!local) {
      return;
    }
    const key = localDashboardsKey(org, project);
    // A null key is a clear() of the storage, which also changes these dashboards.
    const onStorage = (event: StorageEvent) => {
      if (event.key === key || event.key === null) {
        void queryClient.invalidateQueries({ queryKey: dashboardsKey(org, project, false) });
      }
    };
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, [local, org, project, queryClient]);
  return useMemo(
    () => ({ store: local ? localStore(org, project) : apiStore(org, project), local }),
    [local, org, project],
  );
}
