import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { ProjectTable } from "@/components/home/project-table";
import type { ActivityState } from "@/components/home/use-project-activity";
import projectsFixture from "../../../../fixtures/api/static/projects.json";
import activityFixture from "../../../../fixtures/api/static/projects-activity.json";

const activity = activityFixture as Schema<"ProjectsActivity">;
const projects = projectsFixture.data as Schema<"Project">[];

function fixtureActivity(id: string): Schema<"ProjectActivity"> {
  const found = activity.data.find((row) => row.project_id === id);
  if (!found) {
    throw new Error(`no activity fixture for ${id}`);
  }
  return found;
}

/** Renders the table with a root route, so its name links resolve. */
async function renderTable(activityOf: (id: string) => ActivityState) {
  const rootRoute = createRootRoute({
    component: () => <ProjectTable org="acme" rows={projects} activity={activityOf} />,
  });
  const router = createRouter({
    routeTree: rootRoute,
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  render(<RouterProvider router={router} />);
  await router.load();
}

function readyOf(id: string): ActivityState {
  const value = fixtureActivity(id);
  return { state: "ready", from: activity.from, activity: value };
}

describe("ProjectTable", () => {
  it("shows role, retention and slug", async () => {
    await renderTable(() => ({ state: "none" }));

    const table = await screen.findByRole("table", { name: "Projects" });
    const support = within(table).getByRole("row", { name: /Support bot/ });
    expect(within(support).getByText("Owner")).toBeInTheDocument();
    expect(within(support).getByText("30 days")).toBeInTheDocument();
    expect(within(support).getByText("support-bot")).toBeInTheDocument();
    const playground = within(table).getByRole("row", { name: /Playground/ });
    expect(within(playground).getByText("Forever")).toBeInTheDocument();
  });

  it("shows the activity columns", async () => {
    await renderTable((id) => readyOf(id));

    const table = await screen.findByRole("table", { name: "Projects" });
    const support = within(table).getByRole("row", { name: /Support bot/ });
    expect(within(support).getByText("42")).toBeInTheDocument();
    expect(within(support).getByText("7.1%")).toBeInTheDocument();
    expect(within(support).getByText("$1.23")).toBeInTheDocument();
    const playground = within(table).getByRole("row", { name: /Playground/ });
    expect(within(playground).getByText("0")).toBeInTheDocument();
    expect(within(playground).getByText("$0")).toBeInTheDocument();
    expect(within(playground).getAllByText("—")).toHaveLength(2);
  });

  it("shows dashes for unavailable activity", async () => {
    await renderTable(() => ({ state: "none" }));

    const table = await screen.findByRole("table", { name: "Projects" });
    const support = within(table).getByRole("row", { name: /Support bot/ });
    expect(within(support).getAllByText("—")).toHaveLength(4);
  });
});
