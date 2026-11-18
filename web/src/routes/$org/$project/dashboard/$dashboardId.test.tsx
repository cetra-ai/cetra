import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { OVERVIEW } from "@/lib/dashboards/overview";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import { recordRequests } from "@/test/requests";
import dashboardDocument from "../../../../../../fixtures/api/static/dashboard.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

const DASHBOARD_ID = dashboardDocument.id;
const CUSTOM = `/acme/support-bot/dashboard/${DASHBOARD_ID}`;
const OVERVIEW_URL = "/acme/support-bot/dashboard";
const UUID_PATH = /^\/acme\/support-bot\/dashboard\/[0-9a-f-]{36}$/;

/** The project as a member of the given role sees it. */
function asRole(role: string) {
  server.use(
    http.get("/api/v1/projects/:project", () => HttpResponse.json({ ...projectFixture, role })),
  );
}

it("loads the dashboard named in the URL", async () => {
  const { router } = await renderApp(CUSTOM);
  expect(await screen.findByRole("heading", { level: 2, name: "Traffic" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Switch dashboard" })).toHaveTextContent(
    "Support overview",
  );
  expect(router.state.location.pathname).toBe(CUSTOM);
});

it("shows a not-found state with a link to the Overview for an unknown id", async () => {
  await renderApp("/acme/support-bot/dashboard/01900000-0000-7000-8000-00000000dead");
  expect(await screen.findByRole("heading", { name: "Dashboard not found" })).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Go to the Overview" })).toHaveAttribute(
    "href",
    OVERVIEW_URL,
  );
});

it("keeps the range, filter, group and flags when switching dashboards", async () => {
  const { router, user } = await renderApp(
    `${CUSTOM}?range=1h&group=model&eval=1&filter=status%20%3D%20'error'`,
  );
  await user.click(await screen.findByRole("button", { name: "Switch dashboard" }));
  await user.click(screen.getByRole("menuitem", { name: "Overview" }));
  await waitFor(() => expect(router.state.location.pathname).toBe(OVERVIEW_URL));
  expect(router.state.location.search).toMatchObject({
    range: "1h",
    group: "model",
    eval: 1,
    filter: "status = 'error'",
  });
});

it("lists the Overview and the project's dashboards, and opens the chosen one", async () => {
  const { router, user } = await renderApp(OVERVIEW_URL);
  await user.click(await screen.findByRole("button", { name: "Switch dashboard" }));
  expect(screen.getByRole("menuitem", { name: "Overview" })).toBeInTheDocument();
  await user.click(screen.getByRole("menuitem", { name: "Support overview" }));
  await waitFor(() => expect(router.state.location.pathname).toBe(CUSTOM));
});

it("creates a new dashboard from the switcher and opens it", async () => {
  const { router, user } = await renderApp(OVERVIEW_URL);
  await user.click(await screen.findByRole("button", { name: "Switch dashboard" }));
  await user.click(screen.getByRole("menuitem", { name: "New dashboard" }));
  await user.type(screen.getByLabelText("Name"), "Ops board");
  await user.click(screen.getByRole("button", { name: "Create dashboard" }));
  await waitFor(() => expect(router.state.location.pathname).toMatch(UUID_PATH));
  expect(await screen.findByRole("button", { name: "Switch dashboard" })).toHaveTextContent(
    "Ops board",
  );
});

it("gives a member on a custom dashboard Edit and the menu", async () => {
  const { user } = await renderApp(CUSTOM);
  await screen.findByRole("heading", { level: 2, name: "Traffic" });
  expect(screen.getByRole("button", { name: "Edit" })).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Dashboard actions" }));
  expect(screen.getByRole("menuitem", { name: "Rename" })).toBeInTheDocument();
  expect(screen.getByRole("menuitem", { name: "Duplicate" })).toBeInTheDocument();
  expect(screen.getByRole("menuitem", { name: "Delete" })).toBeInTheDocument();
});

it("gives a member on the Overview Edit and Duplicate, but no rename, delete or reset", async () => {
  const { user } = await renderApp(OVERVIEW_URL);
  await screen.findByRole("heading", { level: 2, name: "Volume" });
  expect(screen.getByRole("button", { name: "Edit" })).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Dashboard actions" }));
  expect(screen.getByRole("menuitem", { name: "Duplicate" })).toBeInTheDocument();
  expect(screen.queryByRole("menuitem", { name: "Rename" })).not.toBeInTheDocument();
  expect(screen.queryByRole("menuitem", { name: "Delete" })).not.toBeInTheDocument();
  expect(screen.queryByRole("menuitem", { name: "Reset to default" })).not.toBeInTheDocument();
});

it("gives a viewer the switcher only", async () => {
  asRole("viewer");
  const { user } = await renderApp(CUSTOM);
  await screen.findByRole("heading", { level: 2, name: "Traffic" });
  expect(screen.queryByRole("button", { name: "Edit" })).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Dashboard actions" })).not.toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Switch dashboard" }));
  expect(screen.getByRole("menuitem", { name: "Support overview" })).toBeInTheDocument();
  expect(screen.queryByRole("menuitem", { name: "New dashboard" })).not.toBeInTheDocument();
});

it("duplicates the Overview into a dashboard with its widgets", async () => {
  const requests = recordRequests();
  try {
    const { router, user } = await renderApp(OVERVIEW_URL);
    await screen.findByRole("heading", { level: 2, name: "Volume" });
    await user.click(screen.getByRole("button", { name: "Dashboard actions" }));
    await user.click(screen.getByRole("menuitem", { name: "Duplicate" }));
    await user.click(screen.getByRole("button", { name: "Duplicate" }));
    await waitFor(() => expect(router.state.location.pathname).toMatch(UUID_PATH));
    const post = requests.calls.find(
      (call) => call.method === "POST" && call.path.endsWith("/dashboards"),
    );
    await waitFor(() => expect(post?.body).toBeDefined());
    expect(post?.body).toMatchObject({
      name: "Copy of Overview",
      definition: { widgets: OVERVIEW.widgets },
    });
    expect(await screen.findByRole("heading", { level: 2, name: "Volume" })).toBeInTheDocument();
  } finally {
    requests.stop();
  }
});

it("deletes a custom dashboard and opens the Overview", async () => {
  const { router, user } = await renderApp(CUSTOM);
  await screen.findByRole("heading", { level: 2, name: "Traffic" });
  await user.click(screen.getByRole("button", { name: "Dashboard actions" }));
  await user.click(screen.getByRole("menuitem", { name: "Delete" }));
  const dialog = await screen.findByRole("dialog");
  await user.click(within(dialog).getByRole("button", { name: "Delete dashboard" }));
  await waitFor(() => expect(router.state.location.pathname).toBe(OVERVIEW_URL));
});
