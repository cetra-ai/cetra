import { screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import sourceFixture from "../../../../../../fixtures/api/static/issue-source.json";
import listFixture from "../../../../../../fixtures/api/static/issue-sources.json";
import entitlementsFixture from "../../../../../../fixtures/api/static/org-entitlements.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

const LIST_URL = "/acme/support-bot/issue-sources";

/** Serves the fixture list of sources. */
function listed() {
  server.use(
    http.get("/api/v1/projects/:project/issue-sources", () => HttpResponse.json(listFixture)),
  );
}

/** Records the body of the next PATCH of a source and answers it with the fixture, changed. */
function capturePatch(answer?: () => Response): { body: () => unknown; calls: () => number } {
  let body: unknown;
  let calls = 0;
  server.use(
    http.patch("/api/v1/projects/:project/issue-sources/:id", async ({ request }) => {
      calls += 1;
      body = await request.json();
      if (answer) {
        return answer();
      }
      return HttpResponse.json({ ...sourceFixture, ...(body as Record<string, unknown>) });
    }),
  );
  return { body: () => body, calls: () => calls };
}

/** The project as a role, for the permission checks. */
function projectAs(role: string) {
  return http.get("/api/v1/projects/:project", () =>
    HttpResponse.json({ ...projectFixture, role }),
  );
}

it("lists each source with its filter, its open issues and its enabled switch", async () => {
  listed();
  await renderApp(LIST_URL);

  expect(await screen.findByText("status = 'error'")).toBeInTheDocument();
  const issues = screen.getByRole("link", { name: "2 open issues" });
  expect(issues.getAttribute("href")).toContain("/acme/support-bot/issues");
  expect(issues.getAttribute("href")).toContain(`source=${sourceFixture.id}`);
  expect(screen.getByRole("switch", { name: "Enable Errors" })).toBeChecked();
});

it("sends a PATCH with the new enabled flag when the switch is toggled", async () => {
  listed();
  const patched = capturePatch();
  const { user } = await renderApp(LIST_URL);

  await user.click(await screen.findByRole("switch", { name: "Enable Errors" }));

  await waitFor(() => expect(patched.body()).toEqual({ enabled: false }));
});

it("shows the plan limit and the upgrade link when enabling is refused", async () => {
  listed();
  capturePatch(() =>
    problemResponse(403, "plan_limit", { detail: "Your plan allows 1 active issue source." }),
  );
  const { user } = await renderApp(LIST_URL);

  await user.click(await screen.findByRole("switch", { name: "Enable Errors" }));

  expect(await screen.findByText("Your plan allows 1 active issue source.")).toBeInTheDocument();
  expect(screen.getByText("Upgrade your plan or disable another source.")).toBeInTheDocument();
  expect(await screen.findByRole("link", { name: "Upgrade" })).toHaveAttribute(
    "href",
    entitlementsFixture.upgrade_url,
  );
});

it("keeps New source enabled for a member, since the server answers the plan limit", async () => {
  listed();
  await renderApp(LIST_URL);

  const link = await screen.findByRole("link", { name: "New source" });
  expect(link.getAttribute("href")).toContain("/acme/support-bot/issue-sources/new");
});

it("disables New source and the switch for a viewer", async () => {
  listed();
  server.use(projectAs("viewer"));
  await renderApp(LIST_URL);

  expect(await screen.findByRole("button", { name: "New source" })).toBeDisabled();
  expect(screen.queryByRole("link", { name: "New source" })).toBeNull();
  expect(await screen.findByRole("switch", { name: "Enable Errors" })).toBeDisabled();
});

it("shows the empty state when the project has no sources", async () => {
  server.use(
    http.get("/api/v1/projects/:project/issue-sources", () =>
      HttpResponse.json({ data: [], next_cursor: null, org_active_sources: 0 }),
    ),
  );
  await renderApp(LIST_URL);

  expect(await screen.findByText("No issue sources yet")).toBeInTheDocument();
});
