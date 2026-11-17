import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it, vi } from "vitest";
import { projectRef } from "@/api/project-ref";
import { ProjectProvider } from "@/components/shell/project-context";
import { ThemeProvider } from "@/components/theme-provider";
import { TooltipProvider } from "@/components/ui/tooltip";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import ruleFixture from "../../../../fixtures/api/static/online-rule.json";
import projectFixture from "../../../../fixtures/api/static/project.json";
import { RulePanel } from "./rule-panel";

const ORG = "acme";
const PROJECT = "support-bot";
const RULE_ID = "01900000-0000-7000-8000-000000000700";
const RULE = "/api/v1/projects/:project/online-rules/:id";

/** Renders the panel inside the project context, with a root route for its links. */
async function renderPanel(options: { role?: "admin" | "viewer"; onClose?: () => void } = {}) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: Infinity, staleTime: Infinity } },
  });
  const onClose = options.onClose ?? vi.fn();
  const rootRoute = createRootRoute({
    component: () => (
      <TooltipProvider>
        <ProjectProvider
          value={{
            org: ORG,
            project: PROJECT,
            ref: projectRef(ORG, PROJECT),
            data: { ...projectFixture, role: options.role ?? "admin" },
          }}
        >
          <RulePanel ruleId={RULE_ID} layout={<span>Layout controls</span>} onClose={onClose} />
        </ProjectProvider>
      </TooltipProvider>
    ),
  });
  const router = createRouter({
    routeTree: rootRoute,
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  render(
    <QueryClientProvider client={queryClient}>
      <ThemeProvider>
        <RouterProvider router={router} />
      </ThemeProvider>
    </QueryClientProvider>,
  );
  await router.load();
  return { user: userEvent.setup(), onClose };
}

it("shows the rule's name, status, summary and content", async () => {
  await renderPanel();

  const panel = await screen.findByRole("region", { name: "Online rule" });
  expect(await within(panel).findByText(ruleFixture.name)).toBeInTheDocument();
  const header = panel.querySelector("header") as HTMLElement;
  expect(within(header).getByText("Active")).toBeInTheDocument();
  expect(within(header).getByText("Layout controls")).toBeInTheDocument();
  expect(within(header).queryByRole("link", { name: "Open page" })).toBeNull();
  expect(within(panel).getByText("Traces · status = 'ok' · 25 %")).toBeInTheDocument();
  expect(within(panel).getByRole("region", { name: "Last 24 hours" })).toBeInTheDocument();
});

it("offers resume for a paused rule", async () => {
  server.use(
    http.get(RULE, () =>
      HttpResponse.json({
        ...ruleFixture,
        paused_reason: "errors",
        paused_at: "2026-01-01T11:00:00.000000000Z",
        paused_detail: "error rate 60 %",
      }),
    ),
  );
  await renderPanel();

  const panel = await screen.findByRole("region", { name: "Online rule" });
  expect(await within(panel).findByText("Paused: errors")).toBeInTheDocument();
  expect(within(panel).getByRole("button", { name: "Resume" })).toBeInTheDocument();
});

it("hides edit and resume from a viewer", async () => {
  server.use(
    http.get(RULE, () =>
      HttpResponse.json({ ...ruleFixture, paused_reason: "budget", paused_at: undefined }),
    ),
  );
  await renderPanel({ role: "viewer" });

  const panel = await screen.findByRole("region", { name: "Online rule" });
  await within(panel).findByText("Paused: budget");
  expect(within(panel).queryByRole("link", { name: "Edit" })).toBeNull();
  expect(within(panel).queryByRole("button", { name: "Resume" })).toBeNull();
});

it("closes through the close button", async () => {
  const { user, onClose } = await renderPanel();

  const panel = await screen.findByRole("region", { name: "Online rule" });
  await user.click(within(panel).getByRole("button", { name: "Close" }));

  expect(onClose).toHaveBeenCalledOnce();
});

it("says so when the rule no longer exists", async () => {
  server.use(http.get(RULE, () => problemResponse(404, "not_found")));

  await renderPanel();

  expect(await screen.findByText("This rule no longer exists.")).toBeInTheDocument();
});
