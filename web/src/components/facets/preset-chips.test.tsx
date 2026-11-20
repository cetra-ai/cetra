import { screen, waitFor } from "@testing-library/react";
import { delay, HttpResponse, http } from "msw";
import { afterEach, expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import { ANNOTATION_DEFINITIONS, serveAnnotationDefinitions } from "@/test/score-definitions";

const TRACES = "/acme/support-bot/traces?range=15m";
const SPANS = "/acme/support-bot/spans?range=15m";
const SESSIONS = "/acme/support-bot/sessions?range=15m";

afterEach(() => {
  server.resetHandlers();
});

/**
 * Answers the facet request of one field with a histogram whose p95 is `p95` (absent when null);
 * every other field answers no values.
 */
function answerP95(field: string, p95: number | null) {
  server.use(
    http.get("/api/v1/projects/:project/facets", ({ request }) => {
      const url = new URL(request.url);
      const view = url.searchParams.get("view");
      if (url.searchParams.get("field") !== field) {
        return HttpResponse.json({
          view,
          field: url.searchParams.get("field"),
          kind: "values",
          total: 0,
          missing: 0,
          values: [],
          more: false,
        });
      }
      return HttpResponse.json({
        view,
        field,
        kind: "histogram",
        total: p95 === null ? 0 : 5,
        missing: 0,
        ...(p95 === null
          ? {}
          : { histogram: { min: 1, max: p95, p50: p95, p95, p99: p95, buckets: [] } }),
      });
    }),
  );
}

it("toggles Errors on and off as the status conjunct", async () => {
  const { router, user } = await renderApp(TRACES);
  // The facet section "Errors" has the same name: the toggle is the pressed one.
  await user.click(await screen.findByRole("button", { name: "Errors", pressed: false }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("status = 'error'");
  });
  expect(screen.getByRole("button", { name: "Errors", pressed: true })).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Errors", pressed: true }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBeUndefined();
  });
});

it("writes LLM on spans as the span kind conjunct", async () => {
  const { router, user } = await renderApp(SPANS);
  await user.click(await screen.findByRole("button", { name: "LLM" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("span_kind = 'llm'");
  });
});

it("writes Errors on sessions as an error count bound", async () => {
  const { router, user } = await renderApp(SESSIONS);
  await user.click(await screen.findByRole("button", { name: "Errors", pressed: false }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("error_count > 0");
  });
});

it("shows a fixed preset as pressed when the filter holds its conjunct", async () => {
  await renderApp(`${TRACES}&filter=${encodeURIComponent("status = 'error'")}`);
  expect(await screen.findByRole("button", { name: "Errors", pressed: true })).toBeInTheDocument();
});

it("writes Slow as a lower bound at the rounded p95 of the field", async () => {
  answerP95("duration_ms", 1234.5);
  const { router, user } = await renderApp(TRACES);
  await user.click(await screen.findByRole("button", { name: "Slow" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("duration_ms >= 1230");
  });
});

it("writes Expensive as a lower bound at the p95 of cost", async () => {
  answerP95("cost_total", 0.00306);
  const { router, user } = await renderApp(TRACES);
  await user.click(await screen.findByRole("button", { name: "Expensive" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("cost_total >= 0.00306");
  });
});

it("keeps Slow disabled while the p95 is loading", async () => {
  server.use(
    http.get("/api/v1/projects/:project/facets", async ({ request }) => {
      const url = new URL(request.url);
      if (url.searchParams.get("field") === "duration_ms") {
        await delay("infinite");
      }
      return HttpResponse.json({
        view: url.searchParams.get("view"),
        field: url.searchParams.get("field"),
        kind: "values",
        total: 0,
        missing: 0,
        values: [],
        more: false,
      });
    }),
  );
  await renderApp(TRACES);
  expect(await screen.findByRole("button", { name: "Slow" })).toBeDisabled();
});

it("keeps Slow disabled when the window has no p95", async () => {
  answerP95("duration_ms", null);
  await renderApp(TRACES);
  await waitFor(() => {
    expect(screen.getByRole("button", { name: "Slow" })).toBeDisabled();
  });
});

it("lists the score definitions under Has score and writes the one picked", async () => {
  serveAnnotationDefinitions();
  const { router, user } = await renderApp(TRACES);
  await user.click(await screen.findByRole("button", { name: "Has score" }));
  await user.click(await screen.findByRole("menuitemcheckbox", { name: "helpfulness" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("has_score('helpfulness')");
  });
});

it("unchecks a score already in the filter, keeping the other conjuncts", async () => {
  serveAnnotationDefinitions();
  const filter = "status = 'ok' AND has_score('helpfulness')";
  const { router, user } = await renderApp(`${TRACES}&filter=${encodeURIComponent(filter)}`);
  await user.click(await screen.findByRole("button", { name: "Has score" }));
  expect(await screen.findByRole("menuitemcheckbox", { name: "helpfulness" })).toBeChecked();
  await user.click(screen.getByRole("menuitemcheckbox", { name: "helpfulness" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("status = 'ok'");
  });
});

it("lists the open issues under Has issue and writes the one picked", async () => {
  const issueId = "01900000-0000-7000-8000-0000000000a1";
  server.use(
    http.get("/api/v1/projects/:project/issues", ({ request }) => {
      const url = new URL(request.url);
      if (url.searchParams.get("state") !== "open" || url.searchParams.get("limit") !== "20") {
        return HttpResponse.json({ data: [], next_cursor: null });
      }
      return HttpResponse.json({
        data: [{ id: issueId, title: "Tool timeout", state: "open" }],
        next_cursor: null,
      });
    }),
  );
  const { router, user } = await renderApp(TRACES);
  await user.click(await screen.findByRole("button", { name: "Has issue" }));
  await user.click(await screen.findByRole("menuitemcheckbox", { name: "Tool timeout" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe(`has_score('issue:${issueId}')`);
  });
  expect(screen.getByRole("button", { name: "Has issue", pressed: true })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Has score", pressed: false })).toBeInTheDocument();
});

it("has no Has score preset on spans", async () => {
  await renderApp(SPANS);
  await screen.findByRole("button", { name: "LLM" });
  expect(screen.queryByRole("button", { name: "Has score" })).not.toBeInTheDocument();
});

it("writes a score name with a quote as a doubled-quote literal", async () => {
  const [helpfulness] = ANNOTATION_DEFINITIONS;
  serveAnnotationDefinitions(helpfulness ? [{ ...helpfulness, name: "o'neil" }] : []);
  const { router, user } = await renderApp(TRACES);
  await user.click(await screen.findByRole("button", { name: "Has score" }));
  await user.click(await screen.findByRole("menuitemcheckbox", { name: "o'neil" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("has_score('o''neil')");
  });
  expect(screen.getByRole("button", { name: "Has score", pressed: true })).toBeInTheDocument();
});

it("keeps the preset chips the height of the filter chips", async () => {
  await renderApp(TRACES);
  const errors = await screen.findByRole("button", { name: "Errors", pressed: false });
  expect(errors).toHaveClass("h-6");
});
