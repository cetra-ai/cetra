import { describe, expect, it } from "vitest";
import {
  CONTEXT_MAX_BYTES,
  fitContext,
  type PageContext,
  pageContext,
  withoutField,
} from "@/lib/assistant/context";

const ORG = "acme";
const PROJECT = "support-bot";
const TRACE = "0af7651916cd43dd8448eb211c80319c";
const SPAN = "b7ad6b7169203331";
const UUID = "01900000-0000-7000-8000-000000001a01";

function at(
  pathname: string,
  search: Record<string, unknown> = {},
  sql?: { query: string; params: Record<string, string> },
) {
  return pageContext({
    org: ORG,
    project: PROJECT,
    pathname: `/${ORG}/${PROJECT}${pathname}`,
    search,
    sql,
  });
}

describe("pageContext", () => {
  it("reads a trace from its path and the span from ?span=", () => {
    expect(at(`/traces/${TRACE}`, { span: SPAN })).toEqual({
      page: "trace",
      trace_id: TRACE,
      span_id: SPAN,
    });
    expect(at(`/traces/${TRACE}`)).toEqual({ page: "trace", trace_id: TRACE });
  });

  it("reads a trace from ?trace= on the traces and spans lists", () => {
    expect(at("/traces", { trace: TRACE, span: SPAN })).toEqual({
      page: "trace",
      trace_id: TRACE,
      span_id: SPAN,
    });
    expect(at("/spans", { trace: TRACE })).toEqual({ page: "trace", trace_id: TRACE });
  });

  it("reads a session from its path", () => {
    expect(at("/sessions/checkout-42")).toEqual({ page: "session", session_id: "checkout-42" });
  });

  it("reads a user from its path and from ?user= on the users list", () => {
    expect(at("/users/u-1")).toEqual({ page: "user", user_id: "u-1" });
    expect(at("/users", { user: "u-2" })).toEqual({ page: "user", user_id: "u-2" });
  });

  it("reads the list filters, flags and saved view of a traces list", () => {
    expect(
      at("/traces", {
        filter: "status = 'error'",
        search: "checkout",
        range: "1h",
        eval: 1,
        exp: "1",
        view: UUID,
      }),
    ).toEqual({
      page: "traces",
      view: UUID,
      filter: "status = 'error'",
      search: "checkout",
      range: "1h",
      include_eval: true,
      include_experiments: true,
    });
  });

  it("defaults a list range to 24h", () => {
    expect(at("/traces")).toEqual({ page: "traces", range: "24h" });
  });

  it("uses from and to on a list when range is missing", () => {
    expect(at("/sessions", { from: "2026-01-01T00:00:00Z", to: "2026-01-02T00:00:00Z" })).toEqual({
      page: "sessions",
      from: "2026-01-01T00:00:00Z",
      to: "2026-01-02T00:00:00Z",
    });
  });

  it("drops invalid list values and keeps the default range", () => {
    expect(at("/traces", { range: "3y", filter: "   ", search: "x".repeat(2000) })).toEqual({
      page: "traces",
      range: "24h",
    });
  });

  it("reads the spans and users list kinds", () => {
    expect(at("/spans", { filter: "name = 'x'" })).toEqual({
      page: "spans",
      filter: "name = 'x'",
      range: "24h",
    });
    expect(at("/users", { filter: "user_id = 'a'" })).toEqual({
      page: "users",
      filter: "user_id = 'a'",
      range: "24h",
    });
  });

  it("reads the SQL text, the params as JSON and the range", () => {
    expect(
      at("/sql", { range: "7d" }, { query: "SELECT 1", params: { name: "checkout" } }),
    ).toEqual({ page: "sql", query: "SELECT 1", params: '{"name":"checkout"}', range: "7d" });
  });

  it("leaves out an empty SQL text and params", () => {
    expect(at("/sql", {}, { query: "  ", params: {} })).toEqual({ page: "sql" });
  });

  it("reads the dashboard id, filter and range", () => {
    expect(at("/dashboard/dash-1", { filter: "env = 'prod'", range: "6h" })).toEqual({
      page: "dashboard",
      dashboard_id: "dash-1",
      filter: "env = 'prod'",
      range: "6h",
    });
  });

  it("has no dashboard id on the overview", () => {
    expect(at("/dashboard")).toEqual({ page: "dashboard" });
  });

  it("reads an evaluator by its UUID", () => {
    expect(at(`/evaluators/${UUID}`)).toEqual({ page: "evaluator", evaluator_id: UUID });
    expect(at("/evaluators/not-a-uuid")).toEqual({ page: "other", path: "/evaluators/not-a-uuid" });
  });

  it("reads an online rule from its edit path and from ?rule=", () => {
    expect(at(`/online-rules/${UUID}/edit`)).toEqual({ page: "online_rule", rule_id: UUID });
    expect(at("/online-rules", { rule: UUID })).toEqual({ page: "online_rule", rule_id: UUID });
  });

  it("reads a dataset by its UUID", () => {
    expect(at(`/datasets/${UUID}`)).toEqual({ page: "dataset", dataset_id: UUID });
  });

  it("reads an experiment from ?experiment=", () => {
    expect(at("/experiments", { experiment: UUID })).toEqual({
      page: "experiment",
      experiment_id: UUID,
    });
  });

  it("names a project settings page without fields", () => {
    expect(at("/settings/general")).toEqual({ page: "settings" });
  });

  it("gives every other page its path", () => {
    expect(at("/alerts/rule-1")).toEqual({ page: "other", path: "/alerts/rule-1" });
  });

  it("gives a path outside the project the whole pathname", () => {
    expect(
      pageContext({ org: ORG, project: PROJECT, pathname: "/other/place", search: {} }),
    ).toEqual({ page: "other", path: "/other/place" });
  });
});

describe("withoutField", () => {
  it("removes a field and keeps page", () => {
    const context: PageContext = { page: "traces", filter: "a", range: "24h" };
    expect(withoutField(context, "filter")).toEqual({ page: "traces", range: "24h" });
    expect(withoutField(context, "page")).toEqual(context);
  });

  it("does not change the context it is given", () => {
    const context: PageContext = { page: "traces", filter: "a" };
    withoutField(context, "filter");
    expect(context).toEqual({ page: "traces", filter: "a" });
  });
});

describe("fitContext", () => {
  it("returns a small context unchanged", () => {
    const context: PageContext = { page: "sql", query: "SELECT 1" };
    expect(fitContext(context)).toBe(context);
  });

  it("cuts a 20 KB SQL query to fit and marks it truncated", () => {
    const context: PageContext = { page: "sql", query: "SELECT 1 ".repeat(2500) };
    const fitted = fitContext(context);
    expect(new TextEncoder().encode(JSON.stringify(fitted)).length).toBeLessThanOrEqual(
      CONTEXT_MAX_BYTES,
    );
    expect(fitted.truncated).toBe(true);
    expect(fitted.page).toBe("sql");
    expect(String(fitted.query).length).toBeGreaterThan(0);
  });

  it("cuts the longest field first and keeps the others", () => {
    const context: PageContext = { page: "sql", query: "x".repeat(9000), range: "24h" };
    const fitted = fitContext(context);
    expect(fitted.range).toBe("24h");
    expect(fitted.truncated).toBe(true);
  });

  it("drops every field but page when even halving cannot fit", () => {
    const context: PageContext = {
      page: "other",
      path: "/x",
      ...Object.fromEntries(Array.from({ length: 1500 }, (_, index) => [`n${index}`, index])),
    };
    const fitted = fitContext(context);
    expect(fitted).toEqual({ page: "other", truncated: true });
  });
});
