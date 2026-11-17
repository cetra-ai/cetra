import { screen, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

function projectAs(role: string) {
  return http.get("/api/v1/projects/:project", () =>
    HttpResponse.json({ ...projectFixture, role }),
  );
}

it("redirects nothing and shows the project settings as cards", async () => {
  const { router } = await renderApp("/acme/support-bot/settings");

  const main = await screen.findByRole("main");
  expect(
    within(main).getByRole("heading", { level: 1, name: "Project settings" }),
  ).toBeInTheDocument();
  expect(router.state.location.pathname).toBe("/acme/support-bot/settings");
  expect(await within(main).findByText("Retention 30 days")).toBeInTheDocument();
  expect(await within(main).findByText("1 key · never used")).toBeInTheDocument();
  expect(await within(main).findByText("1 active")).toBeInTheDocument();
  expect(await within(main).findByText("3 channels, 1 failing")).toBeInTheDocument();
  expect(await within(main).findByText("Last request: Pending")).toBeInTheDocument();
  expect(within(main).getByRole("link", { name: /Score definitions/ })).toHaveAttribute(
    "href",
    "/acme/support-bot/settings/score-definitions",
  );
});

it("lists the organization cards below the project cards", async () => {
  await renderApp("/acme/support-bot/settings");

  const main = await screen.findByRole("main");
  expect(within(main).getByRole("heading", { name: "Organization" })).toBeInTheDocument();
  expect(within(main).getByText("Slug: acme")).toBeInTheDocument();
  expect(await within(main).findByText("Enabled")).toBeInTheDocument();
});

it("hides the admin-only cards from members and does not read their data", async () => {
  let apiKeyReads = 0;
  let deletionReads = 0;
  server.use(
    projectAs("member"),
    http.get("/api/v1/projects/:project/api-keys", () => {
      apiKeyReads += 1;
      return HttpResponse.json({ data: [], next_cursor: null });
    }),
    http.get("/api/v1/projects/:project/deletions", () => {
      deletionReads += 1;
      return HttpResponse.json({ data: [], next_cursor: null });
    }),
  );

  await renderApp("/acme/support-bot/settings");
  const main = await screen.findByRole("main");
  expect(await within(main).findByText("Retention 30 days")).toBeInTheDocument();
  expect(within(main).queryByRole("link", { name: /API keys/ })).toBeNull();
  expect(within(main).queryByRole("link", { name: /Data/ })).toBeNull();
  // The project and organization cards both link to members.
  expect(within(main).getAllByRole("link", { name: /Members/ }).length).toBeGreaterThan(0);
  expect(apiKeyReads).toBe(0);
  expect(deletionReads).toBe(0);
});
