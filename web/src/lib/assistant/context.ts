import { isProjectSettingsPath } from "@/components/settings/settings-nav";
import {
  FILTER_MAX,
  parseEnum,
  parseFlag,
  parseHex,
  parseRfc3339,
  parseString,
  parseViewId,
  SEARCH_MAX,
} from "@/lib/search/common";
import { PRESETS } from "@/lib/time-range";

export type PageKind =
  | "trace"
  | "session"
  | "user"
  | "traces"
  | "spans"
  | "sessions"
  | "users"
  | "sql"
  | "dashboard"
  | "evaluator"
  | "online_rule"
  | "dataset"
  | "experiment"
  | "settings"
  | "other";

export type PageContext = { page: PageKind } & Record<string, string | number | boolean>;

export const CONTEXT_MAX_BYTES = 8192;

const LIST_DEFAULT_RANGE = "24h";
const USER_MAX = 256;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const encoder = new TextEncoder();

type Fields = Record<string, string | number | boolean | undefined>;
type Search = Record<string, unknown>;

/** Builds a context: `page` first, then every defined, non-empty field. */
function build(page: PageKind, fields: Fields): PageContext {
  const context: PageContext = { page };
  for (const [key, value] of Object.entries(fields)) {
    if (value !== undefined && value !== "") {
      context[key] = value;
    }
  }
  return context;
}

/** The part of `pathname` after `/<org>/<project>`, or `undefined` when it is outside the project. */
function pathWithin(pathname: string, org: string, project: string): string | undefined {
  const base = `/${org}/${project}`;
  if (pathname === base) {
    return "";
  }
  if (pathname.startsWith(`${base}/`)) {
    return pathname.slice(base.length).replace(/\/+$/, "");
  }
  return undefined;
}

/** A `range` preset, or an absolute `from`/`to` pair in order. `undefined` when neither is valid. */
function rangeOf(search: Search): Fields | undefined {
  const range = parseEnum(search.range, PRESETS);
  if (range !== undefined) {
    return { range };
  }
  const from = parseRfc3339(search.from);
  const to = parseRfc3339(search.to);
  if (from !== undefined && to !== undefined && Date.parse(from) < Date.parse(to)) {
    return { from, to };
  }
  return undefined;
}

function traceFields(traceId: string, search: Search): PageContext {
  return build("trace", { trace_id: traceId, span_id: parseHex(search.span, 16) });
}

function listFields(path: string, search: Search): PageContext {
  const kind = path.slice(1) as "traces" | "spans" | "sessions" | "users";
  const fields: Fields = {
    view: parseViewId(search.view),
    filter: parseString(search.filter, FILTER_MAX),
    search: parseString(search.search, SEARCH_MAX),
    ...(rangeOf(search) ?? { range: LIST_DEFAULT_RANGE }),
    include_eval: parseFlag(search.eval) ? true : undefined,
    include_experiments: parseFlag(search.exp) ? true : undefined,
  };
  return build(kind, fields);
}

/**
 * The context of the page at `pathname` + `search` in project `org`/`project` (D-AS-29).
 * Pure: the SQL text and parameters come in through `sql`. Invalid search values are dropped.
 */
export function pageContext(input: {
  org: string;
  project: string;
  pathname: string;
  search: Search;
  sql?: { query: string; params: Record<string, string> };
}): PageContext {
  const { search } = input;
  const path = pathWithin(input.pathname, input.org, input.project);
  if (path === undefined) {
    return build("other", { path: input.pathname });
  }

  const trace = /^\/traces\/([0-9a-f]{32})$/.exec(path)?.[1];
  if (trace !== undefined) {
    return traceFields(trace, search);
  }
  if (path === "/traces" || path === "/spans") {
    const traceParam = parseHex(search.trace, 32);
    if (traceParam !== undefined) {
      return build("trace", { trace_id: traceParam, span_id: parseHex(search.span, 16) });
    }
  }

  const session = /^\/sessions\/([^/]+)$/.exec(path)?.[1];
  if (session !== undefined) {
    return build("session", { session_id: session });
  }

  const user = /^\/users\/([^/]+)$/.exec(path)?.[1];
  if (user !== undefined) {
    return build("user", { user_id: user });
  }
  if (path === "/users") {
    const userParam = parseString(search.user, USER_MAX);
    if (userParam !== undefined) {
      return build("user", { user_id: userParam });
    }
  }

  if (path === "/traces" || path === "/spans" || path === "/sessions" || path === "/users") {
    return listFields(path, search);
  }

  if (path === "/sql") {
    const sql = input.sql;
    const params =
      sql !== undefined && Object.keys(sql.params).length > 0
        ? JSON.stringify(sql.params)
        : undefined;
    return build("sql", {
      query: sql?.query.trim() === "" ? undefined : sql?.query,
      params,
      ...rangeOf(search),
    });
  }

  const dashboard = /^\/dashboard(?:\/([^/]+))?$/.exec(path);
  if (dashboard !== null) {
    return build("dashboard", {
      dashboard_id: dashboard[1],
      filter: parseString(search.filter, FILTER_MAX),
      ...rangeOf(search),
    });
  }

  const evaluator = /^\/evaluators\/([^/]+)$/.exec(path)?.[1];
  if (evaluator !== undefined && UUID.test(evaluator)) {
    return build("evaluator", { evaluator_id: evaluator });
  }

  const rule = /^\/online-rules\/([^/]+)\/edit$/.exec(path)?.[1];
  if (rule !== undefined && UUID.test(rule)) {
    return build("online_rule", { rule_id: rule });
  }
  if (path === "/online-rules") {
    const ruleParam = parseString(search.rule, 64);
    if (ruleParam !== undefined && UUID.test(ruleParam)) {
      return build("online_rule", { rule_id: ruleParam });
    }
  }

  const dataset = /^\/datasets\/([^/]+)$/.exec(path)?.[1];
  if (dataset !== undefined && UUID.test(dataset)) {
    return build("dataset", { dataset_id: dataset });
  }

  if (path === "/experiments") {
    const experiment = parseString(search.experiment, 64);
    if (experiment !== undefined && UUID.test(experiment)) {
      return build("experiment", { experiment_id: experiment });
    }
  }

  if (isProjectSettingsPath(input.pathname, input.org, input.project)) {
    return build("settings", {});
  }

  return build("other", { path });
}

/** Removes the chip `key`, never `page`. */
export function withoutField(context: PageContext, key: string): PageContext {
  if (key === "page") {
    return context;
  }
  const copy: PageContext = { ...context };
  delete copy[key];
  return copy;
}

function bytesOf(context: PageContext): number {
  return encoder.encode(JSON.stringify(context)).length;
}

/** The longest non-empty string field other than `page`, with its code points halved. `undefined` when none is left. */
function halveLongest(context: PageContext): PageContext | undefined {
  let longestKey: string | undefined;
  let longest = 0;
  for (const [key, value] of Object.entries(context)) {
    if (key !== "page" && typeof value === "string" && value.length > longest) {
      longestKey = key;
      longest = value.length;
    }
  }
  if (longestKey === undefined) {
    return undefined;
  }
  const points = Array.from(String(context[longestKey]));
  return { ...context, [longestKey]: points.slice(0, Math.floor(points.length / 2)).join("") };
}

/**
 * Fits the context into `CONTEXT_MAX_BYTES` of compact JSON: halves the longest string field until
 * it fits and adds `truncated: true`. As a last resort every field but `page` is dropped.
 */
export function fitContext(context: PageContext): PageContext {
  if (bytesOf(context) <= CONTEXT_MAX_BYTES) {
    return context;
  }
  let fitted: PageContext = { ...context, truncated: true };
  for (;;) {
    if (bytesOf(fitted) <= CONTEXT_MAX_BYTES) {
      return fitted;
    }
    const halved = halveLongest(fitted);
    if (halved === undefined) {
      break;
    }
    fitted = halved;
  }
  return { page: context.page, truncated: true };
}
