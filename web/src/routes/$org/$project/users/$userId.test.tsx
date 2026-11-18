import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { presetLabel } from "@/lib/time-range";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import deletionFixture from "../../../../../../fixtures/api/static/deletion.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

const TRACE = "a1000000000000000000000000000001";
const DESCRIBE_TRACE = "a1000000000000000000000000000004";

it("lists user-42 sessions and opens one", async () => {
  const { router, user } = await renderApp("/acme/support-bot/users/user-42");
  expect(await screen.findByText("3 traces")).toBeInTheDocument();
  expect(screen.getByText("1 session")).toBeInTheDocument();
  expect(screen.getByText("5.2k")).toBeInTheDocument();
  await user.click(screen.getByRole("tab", { name: /Sessions/ }));
  const grid = screen.getByRole("grid", { name: "Sessions" });
  expect(within(grid).getByText("sess-001")).toBeInTheDocument();
  await user.click(within(grid).getByRole("row", { name: /sess-001/ }));
  expect(router.state.location.pathname).toBe("/acme/support-bot/sessions/sess-001");
  // Let the session page finish loading, so no transition outlives the test.
  expect(await screen.findByRole("region", { name: "Conversation" })).toBeInTheDocument();
});

it("follows the user link from a trace header", async () => {
  const { router, user } = await renderApp(`/acme/support-bot/traces?trace=${TRACE}`);
  const header = await screen.findByRole("region", { name: "Trace" });
  await user.click(within(header).getByRole("link", { name: "user-42" }));
  expect(router.state.location.pathname).toBe("/acme/support-bot/users/user-42");
  expect(await screen.findByText("3 traces")).toBeInTheDocument();
});

it("shows the scores, metadata and usage on the overview tab", async () => {
  await renderApp("/acme/support-bot/users/user-42");
  expect(await screen.findByRole("region", { name: "User scores" })).toBeInTheDocument();
  expect(screen.getByRole("region", { name: "User metadata" })).toBeInTheDocument();
  await waitFor(() => {
    expect(screen.getByText("p95 latency")).toBeInTheDocument();
  });
  expect(screen.getByRole("tab", { name: "Overview" })).toHaveAttribute("aria-selected", "true");
});

it("lists the user's traces and opens one", async () => {
  const { router, user } = await renderApp("/acme/support-bot/users/user-42");
  await user.click(await screen.findByRole("tab", { name: "Traces" }));
  const grid = await screen.findByRole("grid", { name: "Traces" });
  await user.click(within(grid).getByRole("row", { name: /describe-image/ }));
  expect(router.state.location.pathname).toBe(`/acme/support-bot/traces/${DESCRIBE_TRACE}`);
  // Let the trace page finish loading, so no transition outlives the test.
  expect(await screen.findByRole("grid", { name: "Trace" })).toBeInTheDocument();
});

it("opens the traces tab from the URL", async () => {
  await renderApp("/acme/support-bot/users/user-42?tab=traces");
  expect(await screen.findByRole("grid", { name: "Traces" })).toBeInTheDocument();
  expect(screen.getByRole("tab", { name: "Traces" })).toHaveAttribute("aria-selected", "true");
});

it("links to the dashboard filtered to the user, in the page's range", async () => {
  await renderApp("/acme/support-bot/users/user-42");
  // The sidebar has a Dashboard link too; this one is the page's.
  const links = await screen.findAllByRole("link", { name: "Dashboard" });
  const href = links
    .map((link) => link.getAttribute("href") ?? "")
    .find((value) => value.includes("filter="));
  expect(href).toContain("filter=user_id");
  expect(href).toContain("range=30d");
});

it("shows the header and an empty state when the user has no trace in the range", async () => {
  server.use(
    http.get("/api/v1/projects/:project/users/:user_id", () =>
      HttpResponse.json({ status: 404, title: "Not found", code: "not_found" }, { status: 404 }),
    ),
  );
  await renderApp("/acme/support-bot/users/user-42");
  expect(await screen.findByText("No traces from this user in this range")).toBeInTheDocument();
  expect(screen.getByText("user-42")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: presetLabel("30d") })).toBeInTheDocument();
});

it("shows a loading state, not 'No traces', while the first page of traces is pending", async () => {
  let release: () => void = () => undefined;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  server.use(
    http.get("/api/v1/projects/:project/traces", async ({ request }) => {
      if (!new URL(request.url).searchParams.get("filter")?.includes("user_id")) {
        return undefined;
      }
      // Hold the user's first page, then let the fixture answer it.
      await gate;
      return undefined;
    }),
  );
  await renderApp("/acme/support-bot/users/user-42?tab=traces");
  expect(await screen.findByText("3 traces")).toBeInTheDocument();
  await waitFor(() => {
    expect(document.querySelector('[aria-busy="true"]')).not.toBeNull();
  });
  expect(screen.queryByText("No traces")).toBeNull();

  release();
  const grid = await screen.findByRole("grid", { name: "Traces" });
  expect(within(grid).getByRole("row", { name: /describe-image/ })).toBeInTheDocument();
});

it("writes the tab to the URL and removes it for Overview", async () => {
  const { router, user } = await renderApp("/acme/support-bot/users/user-42");
  await user.click(await screen.findByRole("tab", { name: /Sessions/ }));
  await waitFor(() => {
    expect(router.state.location.search.tab).toBe("sessions");
  });
  await user.click(screen.getByRole("tab", { name: "Overview" }));
  await waitFor(() => {
    expect(router.state.location.search.tab).toBeUndefined();
  });
});

it("carries the range and the eval flag into the back link to users", async () => {
  const { unmount } = await renderApp("/acme/support-bot/users/user-42");
  const back = await screen.findByRole("link", { name: "Back to users" });
  expect(back.getAttribute("href")).toContain("range=30d");
  expect(back.getAttribute("href")).not.toContain("eval=1");
  unmount();

  await renderApp("/acme/support-bot/users/user-42?eval=1");
  const withEval = await screen.findByRole("link", { name: "Back to users" });
  expect(withEval.getAttribute("href")).toContain("range=30d");
  expect(withEval.getAttribute("href")).toContain("eval=1");
});

it("keeps the page's range, eval flag and filter on the dashboard link", async () => {
  await renderApp("/acme/support-bot/users/user-42?range=7d&eval=1");
  const links = await screen.findAllByRole("link", { name: "Dashboard" });
  const href = links
    .map((link) => link.getAttribute("href") ?? "")
    .find((value) => value.includes("filter="));
  expect(href).toContain("range=7d");
  expect(href).toContain("eval=1");
  expect(href).toContain("filter=");
});

it("shows an error in the usage charts when the widget queries fail, and the page still renders", async () => {
  server.use(
    http.post("/api/v1/projects/:project/dashboards/query", () =>
      HttpResponse.json({ status: 500, title: "Server error", code: "internal" }, { status: 500 }),
    ),
  );
  await renderApp("/acme/support-bot/users/user-42");
  expect((await screen.findAllByRole("alert")).length).toBeGreaterThan(0);
  expect(screen.getByRole("region", { name: "User scores" })).toBeInTheDocument();
  expect(screen.getByRole("tab", { name: "Overview" })).toHaveAttribute("aria-selected", "true");
});

it("deletes the user's data after type-to-confirm, then returns to the data tab", async () => {
  let body: unknown;
  server.use(
    http.post("/api/v1/projects/:project/deletions", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json(deletionFixture, { status: 202 });
    }),
  );
  const { router, user } = await renderApp("/acme/support-bot/users/user-42");
  await user.click(await screen.findByRole("button", { name: "Delete user data…" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Delete this user's data?" });
  const confirm = within(dialog).getByRole("button", { name: "Delete user data" });
  expect(confirm).toBeDisabled();
  await user.type(within(dialog).getByRole("textbox"), "user-42");
  await user.click(confirm);

  await waitFor(() =>
    expect(router.state.location.pathname).toBe("/acme/support-bot/settings/data"),
  );
  // Let the settings page finish loading, so no transition outlives the test.
  await waitFor(() => expect(router.state.status).toBe("idle"));
  expect(body).toEqual({ kind: "user", user_id: "user-42" });
});

it("disables delete user data for members, with the reason", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "member" }),
    ),
  );
  await renderApp("/acme/support-bot/users/user-42");

  const button = await screen.findByRole("button", { name: "Delete user data…" });
  expect(button).toBeDisabled();
  expect(button.parentElement).toHaveAccessibleDescription("Only project admins can delete data");
});

it("goes back to the user page from a session opened there", async () => {
  const { router, user } = await renderApp("/acme/support-bot/users/user-42");
  expect(await screen.findByText("3 traces")).toBeInTheDocument();
  await user.click(screen.getByRole("tab", { name: /Sessions/ }));
  const grid = screen.getByRole("grid", { name: "Sessions" });
  await user.click(within(grid).getByRole("row", { name: /sess-001/ }));
  await user.click(await screen.findByRole("link", { name: "Back to Ada Example" }));
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/users/user-42");
  });
  await waitFor(() => {
    expect(router.state.status).toBe("idle");
  });
});
