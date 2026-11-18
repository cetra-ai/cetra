import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import type { ReactNode } from "react";
import { expect, it, vi } from "vitest";
import type { Dashboard, Definition } from "@/lib/dashboards/types";
import { dashboardHandlers, seedDashboards } from "@/mocks/dashboards";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import dashboardDocument from "../../../../fixtures/api/static/dashboard.json";
import {
  DeleteDashboardDialog,
  DuplicateDashboardDialog,
  NewDashboardDialog,
  RenameDashboardDialog,
} from "./dashboard-dialogs";

const ORG = "acme";
const PROJECT = "support-bot";
const LIST = "/api/v1/projects/:project/dashboards";
const ONE = "/api/v1/projects/:project/dashboards/:id";
const STALE = "Someone else changed this dashboard. Reload it and try again.";

function renderWith(node: ReactNode) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: Infinity, staleTime: Infinity } },
  });
  render(<QueryClientProvider client={queryClient}>{node}</QueryClientProvider>);
  return userEvent.setup();
}

const definition = dashboardDocument.definition as Definition;

const source = {
  id: dashboardDocument.id,
  name: dashboardDocument.name,
  description: dashboardDocument.description,
  version: dashboardDocument.version,
};

it("creates an empty dashboard and reports it", async () => {
  server.use(...dashboardHandlers(seedDashboards()));
  const onCreated = vi.fn<(dashboard: Dashboard) => void>();
  const user = renderWith(
    <NewDashboardDialog
      org={ORG}
      project={PROJECT}
      open
      onOpenChange={() => {}}
      onCreated={onCreated}
    />,
  );
  await user.type(screen.getByLabelText("Name"), "Costs");
  await user.click(screen.getByRole("button", { name: "Create dashboard" }));
  await waitFor(() => expect(onCreated).toHaveBeenCalledTimes(1));
  expect(onCreated.mock.calls[0]?.[0]).toMatchObject({ name: "Costs", version: 1 });
});

it("checks the name on the client before any request", async () => {
  let requests = 0;
  server.use(
    http.post(LIST, () => {
      requests += 1;
      return HttpResponse.json({}, { status: 201 });
    }),
  );
  const user = renderWith(
    <NewDashboardDialog
      org={ORG}
      project={PROJECT}
      open
      onOpenChange={() => {}}
      onCreated={() => {}}
    />,
  );
  await user.click(screen.getByRole("button", { name: "Create dashboard" }));
  expect(await screen.findByText("must be 1 to 100 characters")).toBeInTheDocument();
  expect(requests).toBe(0);
});

it("shows a server 400 under the field it names", async () => {
  server.use(
    http.post(LIST, () =>
      problemResponse(400, "bad_request", {
        errors: [{ field: "name", message: "must be 1 to 100 characters" }],
      }),
    ),
  );
  const onCreated = vi.fn();
  const user = renderWith(
    <NewDashboardDialog
      org={ORG}
      project={PROJECT}
      open
      onOpenChange={() => {}}
      onCreated={onCreated}
    />,
  );
  await user.type(screen.getByLabelText("Name"), "Costs");
  await user.click(screen.getByRole("button", { name: "Create dashboard" }));
  expect(await screen.findByText("must be 1 to 100 characters")).toBeInTheDocument();
  expect(onCreated).not.toHaveBeenCalled();
});

it("shows the cap as a form message", async () => {
  server.use(
    http.post(LIST, () => problemResponse(409, "conflict", { detail: "too many dashboards" })),
  );
  const user = renderWith(
    <NewDashboardDialog
      org={ORG}
      project={PROJECT}
      open
      onOpenChange={() => {}}
      onCreated={() => {}}
    />,
  );
  await user.type(screen.getByLabelText("Name"), "Costs");
  await user.click(screen.getByRole("button", { name: "Create dashboard" }));
  expect(await screen.findByText("too many dashboards")).toBeInTheDocument();
});

it("renames with the version it read and clears a blank description", async () => {
  server.use(...dashboardHandlers(seedDashboards()));
  let body: unknown;
  server.use(
    http.patch(ONE, async ({ request }) => {
      body = await request.json();
      return HttpResponse.json({ ...dashboardDocument, name: "Renamed", version: 4 });
    }),
  );
  const onSaved = vi.fn();
  const user = renderWith(
    <RenameDashboardDialog
      org={ORG}
      project={PROJECT}
      dashboard={source}
      open
      onOpenChange={() => {}}
      onSaved={onSaved}
    />,
  );
  const name = screen.getByLabelText("Name");
  await user.clear(name);
  await user.type(name, "Renamed");
  await user.clear(screen.getByLabelText("Description (optional)"));
  await user.click(screen.getByRole("button", { name: "Save" }));
  await waitFor(() => expect(onSaved).toHaveBeenCalledTimes(1));
  expect(body).toEqual({ version: 3, name: "Renamed", description: "" });
});

it("tells the user when the dashboard changed elsewhere", async () => {
  server.use(
    http.patch(ONE, () =>
      problemResponse(409, "conflict", { detail: "the dashboard changed; current version is 4" }),
    ),
  );
  const user = renderWith(
    <RenameDashboardDialog
      org={ORG}
      project={PROJECT}
      dashboard={source}
      open
      onOpenChange={() => {}}
      onSaved={() => {}}
    />,
  );
  await user.click(screen.getByRole("button", { name: "Save" }));
  expect(await screen.findByText(STALE)).toBeInTheDocument();
});

it("duplicates the definition under a new name", async () => {
  server.use(...dashboardHandlers(seedDashboards()));
  let body: unknown;
  server.use(
    http.post(LIST, async ({ request }) => {
      body = await request.json();
      return HttpResponse.json({ id: "x", name: "Copy", version: 1 }, { status: 201 });
    }),
  );
  const onCreated = vi.fn();
  const user = renderWith(
    <DuplicateDashboardDialog
      org={ORG}
      project={PROJECT}
      defaultName="Support overview copy"
      definition={definition}
      open
      onOpenChange={() => {}}
      onCreated={onCreated}
    />,
  );
  await user.clear(screen.getByLabelText("Name"));
  await user.type(screen.getByLabelText("Name"), "Copy");
  await user.click(screen.getByRole("button", { name: "Duplicate" }));
  await waitFor(() => expect(onCreated).toHaveBeenCalledTimes(1));
  expect(body).toEqual({ name: "Copy", definition });
});

it("deletes after confirmation", async () => {
  server.use(...dashboardHandlers(seedDashboards()));
  const onDeleted = vi.fn();
  const user = renderWith(
    <DeleteDashboardDialog
      org={ORG}
      project={PROJECT}
      dashboard={source}
      open
      onOpenChange={() => {}}
      onDeleted={onDeleted}
    />,
  );
  expect(screen.getByText(/This cannot be undone/)).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Delete dashboard" }));
  await waitFor(() => expect(onDeleted).toHaveBeenCalledTimes(1));
});

it("shows the server's refusal when the delete fails", async () => {
  server.use(http.delete(ONE, () => problemResponse(404, "not_found")));
  const user = renderWith(
    <DeleteDashboardDialog
      org={ORG}
      project={PROJECT}
      dashboard={source}
      open
      onOpenChange={() => {}}
      onDeleted={() => {}}
    />,
  );
  await user.click(screen.getByRole("button", { name: "Delete dashboard" }));
  expect(await screen.findByText("Not Found")).toBeInTheDocument();
});
