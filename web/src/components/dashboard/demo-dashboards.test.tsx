import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";
import { STORAGE_PREFIX } from "@/product";
import { renderApp } from "@/test/render";
import { recordRequests } from "@/test/requests";
import dashboardDocument from "../../../../fixtures/api/static/dashboard.json";
import dashboardsDocument from "../../../../fixtures/api/static/dashboards.json";
import projectDocument from "../../../../fixtures/api/static/project.json";

type Role = Schema<"OrgRole">;

const USER = {
  id: "01900000-0000-7000-8000-000000000100",
  email: "ada@example.com",
  name: "Ada Example",
  is_instance_admin: false,
};
const NOTICE = "These dashboards are stored in this browser only.";
const LOCAL_KEY = `${STORAGE_PREFIX}dashboards.acme/support-bot`;
const SERVER_NAME = dashboardsDocument.data[0]?.name ?? "";
const CUSTOM = `/acme/support-bot/dashboard/${dashboardDocument.id}`;

/** Signs in as a member of `acme` with the given role, and whether acme is the demo org. */
function signedInAs(role: Role, demo: boolean) {
  server.use(
    http.get("/api/auth/me", () =>
      HttpResponse.json({
        user: USER,
        orgs: [
          { id: "01900000-0000-7000-8000-000000000010", slug: "acme", name: "Acme", role, demo },
        ],
      }),
    ),
    http.get("/api/v1/projects/:project", () => HttpResponse.json({ ...projectDocument, role })),
  );
}

/** Calls to the dashboards API: the list, one dashboard, and the writes (not the widget queries). */
function dashboardsApiCalls(calls: { method: string; path: string }[]) {
  return calls.filter(
    (call) => call.path.includes("/dashboards") && !call.path.endsWith("/dashboards/query"),
  );
}

it("keeps a demo viewer's dashboards in the browser, through a reload", async () => {
  signedInAs("viewer", true);
  const requests = recordRequests();
  try {
    const rendered = await renderApp("/acme/support-bot/dashboard");
    const { user } = rendered;
    await user.click(await screen.findByRole("button", { name: "Switch dashboard" }));
    await user.click(screen.getByRole("menuitem", { name: "New dashboard" }));
    await user.type(screen.getByLabelText("Name"), "Spend");
    await user.click(screen.getByRole("button", { name: "Create dashboard" }));

    await user.click(await screen.findByRole("button", { name: "Edit" }));
    await user.click(screen.getByRole("button", { name: "Add heading" }));
    await user.type(screen.getByLabelText("Heading"), "Cost by model");
    await user.click(screen.getByRole("button", { name: "Apply" }));
    await user.click(screen.getByRole("button", { name: "Save" }));
    await screen.findByText("Cost by model");

    const stored = JSON.parse(localStorage.getItem(LOCAL_KEY) ?? "[]") as { id: string }[];
    expect(stored).toHaveLength(1);
    rendered.unmount();

    const again = await renderApp(`/acme/support-bot/dashboard/${stored[0]?.id}`);
    expect(await screen.findByText("Cost by model")).toBeInTheDocument();
    again.unmount();

    expect(dashboardsApiCalls(requests.calls)).toEqual([]);
  } finally {
    requests.stop();
  }
});

it("lists only the Overview and the browser's dashboards to a demo viewer", async () => {
  signedInAs("viewer", true);
  const { user } = await renderApp("/acme/support-bot/dashboard");
  await user.click(await screen.findByRole("button", { name: "Switch dashboard" }));
  const menu = await screen.findByRole("menu");
  expect(within(menu).getByRole("menuitem", { name: "Overview" })).toBeInTheDocument();
  expect(within(menu).queryByRole("menuitem", { name: SERVER_NAME })).toBeNull();
});

it("duplicating the Overview as a demo viewer stores it locally", async () => {
  signedInAs("viewer", true);
  const requests = recordRequests();
  try {
    const { user } = await renderApp("/acme/support-bot/dashboard");
    await user.click(await screen.findByRole("button", { name: "Dashboard actions" }));
    await user.click(screen.getByRole("menuitem", { name: "Duplicate" }));
    await user.click(screen.getByRole("button", { name: "Duplicate" }));

    await waitFor(() => expect(localStorage.getItem(LOCAL_KEY)).not.toBeNull());
    const stored = JSON.parse(localStorage.getItem(LOCAL_KEY) ?? "[]") as { name: string }[];
    expect(stored.map((item) => item.name)).toEqual(["Copy of Overview"]);
    expect(dashboardsApiCalls(requests.calls)).toEqual([]);
  } finally {
    requests.stop();
  }
});

it("a storage event from another tab refreshes the list", async () => {
  signedInAs("viewer", true);
  const { user } = await renderApp("/acme/support-bot/dashboard");
  await user.click(await screen.findByRole("button", { name: "Switch dashboard" }));
  expect(
    within(await screen.findByRole("menu")).queryByRole("menuitem", { name: "Elsewhere" }),
  ).toBeNull();
  await user.keyboard("{Escape}");

  localStorage.setItem(
    LOCAL_KEY,
    JSON.stringify([
      {
        id: "01900000-0000-7000-8000-000000000a01",
        name: "Elsewhere",
        version: 1,
        created_at: "2026-11-18T10:00:00.000Z",
        updated_at: "2026-11-18T10:00:00.000Z",
        definition: { widgets: [] },
      },
    ]),
  );
  window.dispatchEvent(new StorageEvent("storage", { key: LOCAL_KEY }));

  await user.click(await screen.findByRole("button", { name: "Switch dashboard" }));
  expect(
    within(await screen.findByRole("menu")).getByRole("menuitem", { name: "Elsewhere" }),
  ).toBeInTheDocument();
});

it("shows the browser-only banner in edit mode for a demo viewer", async () => {
  signedInAs("viewer", true);
  const { user } = await renderApp("/acme/support-bot/dashboard");
  await user.click(await screen.findByRole("button", { name: "Switch dashboard" }));
  await user.click(screen.getByRole("menuitem", { name: "New dashboard" }));
  await user.type(screen.getByLabelText("Name"), "Spend");
  await user.click(screen.getByRole("button", { name: "Create dashboard" }));
  await user.click(await screen.findByRole("button", { name: "Edit" }));
  expect(screen.getByText(NOTICE)).toBeInTheDocument();
});

it("a demo viewer's Overview is the preset, with no Edit and no request for a stored one", async () => {
  signedInAs("viewer", true);
  const { user } = await renderApp("/acme/support-bot/dashboard");
  await screen.findByRole("region", { name: "Traces & errors" });
  await user.click(await screen.findByRole("button", { name: "Switch dashboard" }));
  await user.keyboard("{Escape}");
  const requests = recordRequests();
  try {
    await user.click(await screen.findByRole("button", { name: "Dashboard actions" }));
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("button", { name: "Edit" })).toBeNull();
    expect(requests.calls.some((call) => call.path.endsWith("/dashboards/overview"))).toBe(false);
  } finally {
    requests.stop();
  }
});

it("uses the API for a demo member, not the browser", async () => {
  signedInAs("member", true);
  const requests = recordRequests();
  try {
    const { user } = await renderApp(CUSTOM);
    await user.click(await screen.findByRole("button", { name: "Edit" }));
    expect(screen.queryByText(NOTICE)).toBeNull();
    await waitFor(() => expect(dashboardsApiCalls(requests.calls).length).toBeGreaterThan(0));
    expect(localStorage.getItem(LOCAL_KEY)).toBeNull();
  } finally {
    requests.stop();
  }
});

it("uses the API for a viewer who is not in the demo org, with no edit controls", async () => {
  signedInAs("viewer", false);
  const requests = recordRequests();
  try {
    const { user } = await renderApp(CUSTOM);
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Switch dashboard" })).toHaveTextContent(
        dashboardDocument.name,
      ),
    );
    expect(screen.queryByRole("button", { name: "Edit" })).toBeNull();
    await user.click(screen.getByRole("button", { name: "Switch dashboard" }));
    expect(screen.queryByRole("menuitem", { name: "New dashboard" })).toBeNull();
    expect(dashboardsApiCalls(requests.calls).length).toBeGreaterThan(0);
  } finally {
    requests.stop();
  }
});
