import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  Outlet,
  RouterProvider,
} from "@tanstack/react-router";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { ProjectCard } from "@/components/home/project-card";
import type { ActivityState } from "@/components/home/use-project-activity";
import { TooltipProvider } from "@/components/ui/tooltip";
import projectsFixture from "../../../../fixtures/api/static/projects.json";
import activityFixture from "../../../../fixtures/api/static/projects-activity.json";

const SUPPORT_ID = "01900000-0000-7000-8000-000000000001";
const PLAYGROUND_ID = "01900000-0000-7000-8000-000000000002";

const activity = activityFixture as Schema<"ProjectsActivity">;
const projects = projectsFixture.data as Schema<"Project">[];
const supportBot = projects[0] as Schema<"Project">;
const playground = projects[1] as Schema<"Project">;

function fixtureActivity(id: string): Schema<"ProjectActivity"> {
  const found = activity.data.find((row) => row.project_id === id);
  if (!found) {
    throw new Error(`no activity fixture for ${id}`);
  }
  return found;
}

const supportActivity = fixtureActivity(SUPPORT_ID);
const quietActivity = fixtureActivity(PLAYGROUND_ID);

function readyState(value: Schema<"ProjectActivity">): ActivityState {
  return { state: "ready", from: activity.from, activity: value };
}

/** Renders the card with a root route, so its links and navigation resolve. */
async function renderCard(card: ActivityState, project: Schema<"Project"> = supportBot) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const root = createRootRoute({
    component: () => (
      <TooltipProvider>
        <Outlet />
      </TooltipProvider>
    ),
  });
  const index = createRoute({
    getParentRoute: () => root,
    path: "/",
    component: () => <ProjectCard org="acme" project={project} activity={card} />,
  });
  const traces = createRoute({
    getParentRoute: () => root,
    path: "$org/$project/traces",
    component: () => <p>Traces page</p>,
  });
  const router = createRouter({
    routeTree: root.addChildren([index, traces]),
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  render(
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  await router.load();
  return router;
}

describe("ProjectCard", () => {
  it("links the name to the project's traces", async () => {
    await renderCard({ state: "loading" });

    expect(await screen.findByRole("link", { name: "Support bot" })).toHaveAttribute(
      "href",
      "/acme/support-bot/traces",
    );
  });

  it("exposes exactly one link, named after the project", async () => {
    await renderCard({ state: "loading" });

    const article = await screen.findByRole("article", { name: "Support bot" });
    const links = within(article).getAllByRole("link");
    expect(links).toHaveLength(1);
    expect(links[0]).toHaveAccessibleName("Support bot");
  });

  it("stretches the name's link over the card", async () => {
    await renderCard({ state: "loading" });

    const article = await screen.findByRole("article", { name: "Support bot" });
    expect(article).toHaveClass("relative");
    const link = within(article).getByRole("link", { name: "Support bot" });
    expect(link).toHaveClass("after:absolute", "after:inset-0");
  });

  it("navigates when the card body is clicked", async () => {
    const router = await renderCard({ state: "loading" });

    const article = await screen.findByRole("article", { name: "Support bot" });
    await userEvent.click(within(article).getByText("Traces"));

    expect(await screen.findByText("Traces page")).toBeInTheDocument();
    expect(router.state.location.pathname).toBe("/acme/support-bot/traces");
  });

  it("shows slug and role", async () => {
    await renderCard({ state: "loading" });

    const article = await screen.findByRole("article", { name: "Support bot" });
    expect(within(article).getByText("support-bot")).toBeInTheDocument();
    expect(within(article).getByText("Owner")).toBeInTheDocument();
  });

  it("shows the activity numbers and the sparkline", async () => {
    await renderCard(readyState(supportActivity));

    const article = await screen.findByRole("article", { name: "Support bot" });
    expect(within(article).getByText("42")).toBeInTheDocument();
    expect(within(article).getByText("7.1%")).toBeInTheDocument();
    expect(within(article).getByText("$1.23")).toBeInTheDocument();
    expect(
      within(article).getByRole("img", { name: "Traces in the last 24 hours" }),
    ).toBeInTheDocument();
    const time = article.querySelector("time");
    expect(time).toHaveAttribute("datetime", supportActivity.last_ingest_at);
  });

  it("says when there are no traces", async () => {
    await renderCard(readyState(quietActivity), playground);

    const article = await screen.findByRole("article", { name: "Playground" });
    expect(within(article).getByText("No traces in the last 24 hours")).toBeInTheDocument();
    expect(within(article).getByText("0")).toBeInTheDocument();
    expect(within(article).getAllByText("—")).toHaveLength(2);
    expect(within(article).getByText("$0")).toBeInTheDocument();
  });

  it("shows dashes when activity is unavailable", async () => {
    await renderCard({ state: "none" });

    const article = await screen.findByRole("article", { name: "Support bot" });
    expect(within(article).getByText("Activity unavailable")).toBeInTheDocument();
    expect(within(article).getAllByText("—")).toHaveLength(4);
  });

  it("shows skeletons while loading", async () => {
    await renderCard({ state: "loading" });

    const article = await screen.findByRole("article", { name: "Support bot" });
    expect(article.querySelectorAll('[data-slot="skeleton"]').length).toBeGreaterThanOrEqual(5);
    expect(within(article).queryByText("42")).not.toBeInTheDocument();
  });

  it("shows a dash for an unknown last trace even with traces", async () => {
    const withoutTime: Schema<"ProjectActivity"> = { ...supportActivity };
    delete withoutTime.last_ingest_at;
    await renderCard(readyState(withoutTime));

    const article = await screen.findByRole("article", { name: "Support bot" });
    expect(within(article).getByText("42")).toBeInTheDocument();
    expect(within(article).getByText("—")).toBeInTheDocument();
    expect(article.querySelector("time")).toBeNull();
  });
});
