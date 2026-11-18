import { screen, waitFor, within } from "@testing-library/react";
import { expect, it } from "vitest";
import { renderApp } from "@/test/render";

const ERRORS = "01900000-0000-7000-8000-000000000210";
const CONTEXT = "cetra.list-context.acme/support-bot";

function setContext(context: object) {
  window.sessionStorage.setItem(CONTEXT, JSON.stringify(context));
}

it("carries the filters of another list into a list entered with no search", async () => {
  setContext({ source: "traces", range: "1h", filter: "name = 'refund' AND user_id = 'u-1'" });
  const { router } = await renderApp("/acme/support-bot/spans");
  await waitFor(() => {
    expect(router.state.location.search).toMatchObject({
      range: "1h",
      filter: "user_id = 'u-1'",
    });
  });
  const notice = await screen.findByRole("status");
  expect(notice).toHaveTextContent("Filters kept from Traces");
  expect(notice).toHaveTextContent("(1 not kept)");
});

it("carries every filter across the two trace views", async () => {
  setContext({ source: "traces", filter: "name = 'refund' AND user_id = 'u-1'" });
  const { router } = await renderApp("/acme/support-bot/users");
  await waitFor(() => {
    expect(router.state.location.search).toMatchObject({
      filter: "name = 'refund' AND user_id = 'u-1'",
    });
  });
  const notice = await screen.findByRole("status");
  expect(notice).toHaveTextContent("Filters kept from Traces");
  expect(notice).not.toHaveTextContent("not kept");
});

it("applies the default view instead of the carried filters", async () => {
  setContext({ source: "spans", filter: "user_id = 'u-1'" });
  const { router } = await renderApp("/acme/support-bot/traces");
  await waitFor(() => {
    expect(router.state.location.search).toMatchObject({ view: ERRORS });
  });
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
});

it("does not carry filters into a list entered with a search", async () => {
  setContext({ source: "traces", filter: "user_id = 'u-1'" });
  const { router } = await renderApp("/acme/support-bot/spans?range=1h");
  await screen.findByRole("grid", { name: "Spans" });
  expect(router.state.location.search).toEqual({ range: "1h" });
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
});

it("clears the carried filters and keeps the range", async () => {
  setContext({ source: "traces", range: "1h", filter: "user_id = 'u-1'" });
  const { router, user } = await renderApp("/acme/support-bot/spans");
  const notice = await screen.findByRole("status");
  await user.click(within(notice).getByRole("button", { name: "Clear" }));
  await waitFor(() => {
    expect(router.state.location.search).toEqual({ range: "1h" });
  });
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
});

it("does not carry filters on a return from a detail page", async () => {
  const { router } = await renderApp(
    `/acme/support-bot/traces?filter=${encodeURIComponent("user_id = 'u-1'")}`,
  );
  await screen.findByRole("grid", { name: "Traces" });
  await router.navigate({
    to: "/$org/$project/spans",
    params: { org: "acme", project: "support-bot" },
    state: { listRestore: {} },
  });
  await screen.findByRole("grid", { name: "Spans" });
  expect(router.state.location.search).toEqual({});
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
});

it("removes the notice on the next search change", async () => {
  setContext({ source: "traces", range: "1h", filter: "user_id = 'u-1'" });
  const { router, user } = await renderApp("/acme/support-bot/spans");
  await screen.findByRole("status");
  await user.click(screen.getByRole("switch", { name: "Eval" }));
  await waitFor(() => {
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
  });
  expect(router.state.location.search).toMatchObject({
    range: "1h",
    filter: "user_id = 'u-1'",
    eval: 1,
  });
});

it("says what was not kept when nothing was carried, and loads the list", async () => {
  setContext({ source: "traces", filter: "name = 'refund'" });
  const { router } = await renderApp("/acme/support-bot/spans");
  const notice = await screen.findByRole("status");
  expect(notice).toHaveTextContent("1 filter from Traces not kept");
  expect(notice).not.toHaveTextContent("Filters kept from");
  await screen.findByRole("grid", { name: "Spans" });
  expect(router.state.location.search).toEqual({});
});

it("loads the list when the stored context carries nothing", async () => {
  setContext({
    source: "traces",
    range: "1y",
    from: "2026-02-01T00:00:00Z",
    to: "2026-01-01T00:00:00Z",
  });
  const { router } = await renderApp("/acme/support-bot/spans");
  await screen.findByRole("grid", { name: "Spans" });
  expect(router.state.location.search).toEqual({});
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
});

it("does not read a context written for another project", async () => {
  window.sessionStorage.setItem(
    "cetra.list-context.acme/other",
    JSON.stringify({ source: "traces", filter: "user_id = 'u-1'" }),
  );
  const { router } = await renderApp("/acme/support-bot/spans");
  await screen.findByRole("grid", { name: "Spans" });
  expect(router.state.location.search).toEqual({});
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
});
