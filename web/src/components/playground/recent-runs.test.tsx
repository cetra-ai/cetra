import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { beforeEach, expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";

const PAGE = "/acme/support-bot/playground";
const USER_ID = "01900000-0000-7000-8000-000000000100";
const TRACE_ID = "0af7651916cd43dd8448eb211c80319c";
const FILTER = `environment = 'cetra-playground' AND metadata['cetra.playground.run_by'] = '${USER_ID}'`;

const ROW = {
  trace_id: TRACE_ID,
  start_time: "2026-01-01T10:00:00.000000000Z",
  end_time: "2026-01-01T10:00:01.000000000Z",
  models: ["gpt-5-mini"],
  cost_total: 0.0021,
  error_count: 0,
};

beforeEach(() => {
  localStorage.clear();
});

it("lists the user's playground runs, asking for the exact filter", async () => {
  const filters: (string | null)[] = [];
  server.use(
    http.get("/api/v1/projects/:project/traces", ({ request }) => {
      filters.push(new URL(request.url).searchParams.get("filter"));
      return HttpResponse.json({ data: [ROW], next_cursor: null });
    }),
  );
  const { user } = await renderApp(PAGE);
  await user.click(await screen.findByRole("button", { name: "Recent runs" }));

  const sheet = await screen.findByRole("dialog", { name: "Recent runs" });
  expect(await within(sheet).findByText("gpt-5-mini")).toBeInTheDocument();
  expect(within(sheet).getByText("$0.0021")).toBeInTheDocument();
  expect(filters).toContain(FILTER);
});

it("says so when the user has no runs", async () => {
  server.use(
    http.get("/api/v1/projects/:project/traces", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );
  const { user } = await renderApp(PAGE);
  await user.click(await screen.findByRole("button", { name: "Recent runs" }));

  const sheet = await screen.findByRole("dialog", { name: "Recent runs" });
  expect(await within(sheet).findByText("No playground runs yet.")).toBeInTheDocument();
});

it("Open sends the run to the page as ?run= and the page loads it", async () => {
  const detailPaths: string[] = [];
  server.use(
    http.get("/api/v1/projects/:project/traces", () =>
      HttpResponse.json({ data: [ROW], next_cursor: null }),
    ),
    http.get("/api/v1/projects/:project/traces/:trace_id", ({ request }) => {
      detailPaths.push(new URL(request.url).pathname);
      // A trace with no spans: the page reports that the run cannot be opened.
      return HttpResponse.json({ spans: [], trace: ROW, truncated: false });
    }),
  );
  const { user } = await renderApp(PAGE);
  await user.click(await screen.findByRole("button", { name: "Recent runs" }));
  const sheet = await screen.findByRole("dialog", { name: "Recent runs" });
  await user.click(await within(sheet).findByRole("button", { name: "Open" }));

  await waitFor(() => expect(detailPaths.length).toBe(1));
  expect(detailPaths[0]?.endsWith(`/traces/${TRACE_ID}`)).toBe(true);
  expect(await screen.findByText("Could not open the run.")).toBeInTheDocument();
});
