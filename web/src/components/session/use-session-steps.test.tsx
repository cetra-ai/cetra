import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { type ReactNode, useState } from "react";
import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { useSessionSteps } from "@/components/session/use-session-steps";
import { stepsFilter } from "@/lib/session-turns";
import { server } from "@/mocks/node";

const FIRST = "a1000000000000000000000000000001";
const SECOND = "a1000000000000000000000000000002";

const START = "2026-01-01T10:00:00.123456789Z";
const END = "2026-01-01T10:05:30.987654321Z";

function traceOf(traceId: string): Schema<"TraceSummary"> {
  return {
    trace_id: traceId,
    name: "support-agent",
    start_time: START,
    end_time: END,
    duration_ns: 2_000_000_000,
    error_count: 0,
    has_root: true,
    llm_count: 1,
    span_count: 3,
    metadata: {},
    models: [],
    scores: [],
    status: "ok",
    tags: [],
  };
}

function turnOf(traceId: string): Schema<"SessionTurn"> {
  return {
    trace: traceOf(traceId),
    input: { format: "text", value: "Where is my order?" },
    output: { format: "text", value: "It ships tomorrow." },
  };
}

function spanOf(overrides: Partial<Schema<"SpanSummary">>): Schema<"SpanSummary"> {
  return {
    trace_id: FIRST,
    span_id: "b2000000000000000000000000000002",
    parent_span_id: "b2000000000000000000000000000001",
    name: "get_order_status",
    span_kind: "tool",
    start_time: "2026-01-01T10:00:00.500000000Z",
    end_time: "2026-01-01T10:00:01.000000000Z",
    duration_ns: 500_000_000,
    status: "ok",
    metadata: {},
    tags: [],
    ...overrides,
  };
}

/** One query client per mounted test, so the answers stay cached while it runs. */
function Wrapper({ children }: { children: ReactNode }) {
  const [client] = useState(
    () => new QueryClient({ defaultOptions: { queries: { retry: false } } }),
  );
  return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}

function renderSteps(turns: readonly Schema<"SessionTurn">[]) {
  return renderHook(
    () =>
      useSessionSteps({
        org: "acme",
        project: "support-bot",
        session: { start_time: START, end_time: END },
        turns,
        includeEval: false,
        includeExperiments: false,
      }),
    { wrapper: Wrapper },
  );
}

function serveSpans(
  answer: () => { data: Schema<"SpanSummary">[]; next_cursor: string | null },
): URLSearchParams[] {
  const queries: URLSearchParams[] = [];
  server.use(
    http.get("/api/v1/projects/:project/spans", ({ request }) => {
      queries.push(new URL(request.url).searchParams);
      return HttpResponse.json(answer());
    }),
  );
  return queries;
}

describe("useSessionSteps", () => {
  it("asks the turns' steps in start order with a trace id filter", async () => {
    const queries = serveSpans(() => ({ data: [], next_cursor: null }));
    const { result } = renderSteps([turnOf(FIRST), turnOf(SECOND)]);

    await waitFor(() => expect(result.current.status).toBe("ready"));
    expect(queries).toHaveLength(1);
    const query = queries[0];
    if (!query) {
      throw new Error("no spans request");
    }
    expect(query.get("filter")).toBe(stepsFilter([FIRST, SECOND]));
    expect(query.get("sort")).toBe("start_time");
    expect(query.get("order")).toBe("asc");
    expect(query.get("limit")).toBe("1000");
    expect(query.get("from")).toBe("2026-01-01T10:00:00.123Z");
    expect(query.get("to")).toBe("2026-01-01T10:05:30.988Z");
    expect(query.has("include_eval")).toBe(false);
    expect(query.has("include_experiments")).toBe(false);
  });

  it("asks 100 turns per request", async () => {
    const queries = serveSpans(() => ({ data: [], next_cursor: null }));
    const ids = Array.from({ length: 101 }, (_, index) => `a1${String(index).padStart(30, "0")}`);
    const { result } = renderSteps(ids.map((id) => turnOf(id)));

    await waitFor(() => expect(result.current.status).toBe("ready"));
    expect(queries).toHaveLength(2);
    const filters = queries
      .map((query) => query.get("filter") ?? "")
      .sort((left, right) => right.length - left.length);
    expect(filters).toEqual([stepsFilter(ids.slice(0, 100)), stepsFilter(ids.slice(100))]);
  });

  it("groups the steps by trace and leaves roots out", async () => {
    serveSpans(() => ({
      data: [
        spanOf({
          span_id: "b2000000000000000000000000000001",
          parent_span_id: undefined,
          span_kind: "llm",
          name: "chat",
        }),
        spanOf({ trace_id: FIRST, span_kind: "tool", name: "get_order_status" }),
        spanOf({ trace_id: SECOND, span_kind: "llm", name: "chat" }),
      ],
      next_cursor: null,
    }));
    const { result } = renderSteps([turnOf(FIRST), turnOf(SECOND)]);

    await waitFor(() => expect(result.current.status).toBe("ready"));
    expect(result.current.stepsOf(FIRST)?.map((span) => span.name)).toEqual([
      "get_order_status",
    ]);
    expect(result.current.stepsOf(SECOND)?.map((span) => span.name)).toEqual(["chat"]);
    expect(result.current.stepsOf("a1000000000000000000000000000003")).toEqual([]);
  });

  it("is pending, then ready; a turn without steps has an empty list", async () => {
    serveSpans(() => ({ data: [spanOf({ trace_id: FIRST })], next_cursor: null }));
    const { result } = renderSteps([turnOf(FIRST)]);

    expect(result.current.status).toBe("pending");
    expect(result.current.stepsOf(FIRST)).toBeUndefined();
    await waitFor(() => expect(result.current.status).toBe("ready"));
    expect(result.current.stepsOf(FIRST)?.map((span) => span.span_id)).toEqual([
      "b2000000000000000000000000000002",
    ]);
    expect(result.current.stepsOf(SECOND)).toEqual([]);
    expect(result.current.truncated).toBe(false);
  });

  it("reports an error and no steps", async () => {
    server.use(
      http.get(
        "/api/v1/projects/:project/spans",
        () =>
          HttpResponse.json(
            { type: "about:blank", title: "Internal error", status: 500, code: "internal" },
            { status: 500 },
          ),
      ),
    );
    const { result } = renderSteps([turnOf(FIRST)]);

    await waitFor(() => expect(result.current.status).toBe("error"));
    expect(result.current.stepsOf(FIRST)).toBeUndefined();
    expect(result.current.truncated).toBe(false);
  });

  it("follows next pages and stops after five, marking the steps truncated", async () => {
    let asked = 0;
    server.use(
      http.get("/api/v1/projects/:project/spans", () => {
        asked += 1;
        return HttpResponse.json({ data: [], next_cursor: "more" });
      }),
    );
    const { result } = renderSteps([turnOf(FIRST)]);

    await waitFor(() => expect(result.current.status).toBe("ready"));
    expect(asked).toBe(5);
    expect(result.current.truncated).toBe(true);
    expect(result.current.stepsOf(FIRST)).toEqual([]);
  });
});
