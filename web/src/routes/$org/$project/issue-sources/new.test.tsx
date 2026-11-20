import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import createFixture from "../../../../../../fixtures/api/requests/issue-source-create.json";
import entitlementsFixture from "../../../../../../fixtures/api/static/org-entitlements.json";

const NEW_URL = "/acme/support-bot/issue-sources/new";
const LIST_URL = "/acme/support-bot/issue-sources";

/** Records the body of the next create request and answers it with `answer`. */
function captureCreate(answer?: () => Response): { body: () => unknown; calls: () => number } {
  let body: unknown;
  let calls = 0;
  server.use(
    http.post("/api/v1/projects/:project/issue-sources", async ({ request }) => {
      calls += 1;
      body = await request.json();
      if (answer) {
        return answer();
      }
      return HttpResponse.json(
        { ...createFixture, id: "01900000-0000-7000-8000-000000001c01" },
        { status: 201 },
      );
    }),
  );
  return { body: () => body, calls: () => calls };
}

it("creates an error source with its filter and returns to the list", async () => {
  const created = captureCreate();
  const { user, router } = await renderApp(NEW_URL);

  await user.type(await screen.findByLabelText("Name"), "Errors");
  await user.type(screen.getByLabelText("Filter"), "status = 'error'");
  await user.click(screen.getByRole("button", { name: "Create source" }));

  await waitFor(() => expect(created.body()).toEqual(createFixture));
  await waitFor(() => expect(router.state.location.pathname).toBe(LIST_URL));
  await waitFor(() => expect(router.state.status).toBe("idle"));
});

it("asks for a name before it sends anything", async () => {
  const created = captureCreate();
  const { user } = await renderApp(NEW_URL);

  await user.click(await screen.findByRole("button", { name: "Create source" }));

  expect(await screen.findByText("Enter a name.")).toBeInTheDocument();
  expect(created.calls()).toBe(0);
});

it("shows a field error from the server under the filter", async () => {
  captureCreate(() =>
    problemResponse(400, "invalid_filter", {
      errors: [{ field: "filter", message: "Unknown column stauts" }],
    }),
  );
  const { user } = await renderApp(NEW_URL);

  await user.type(await screen.findByLabelText("Name"), "Errors");
  await user.type(screen.getByLabelText("Filter"), "stauts = 'error'");
  await user.click(screen.getByRole("button", { name: "Create source" }));

  expect(await screen.findByText("Unknown column stauts")).toBeInTheDocument();
  expect(screen.getByLabelText("Filter")).toHaveAttribute("aria-invalid", "true");
});

it("shows the plan limit with the upgrade link", async () => {
  captureCreate(() =>
    problemResponse(403, "plan_limit", { detail: "Your plan allows 1 active issue source." }),
  );
  const { user } = await renderApp(NEW_URL);

  await user.type(await screen.findByLabelText("Name"), "Errors");
  await user.click(screen.getByRole("button", { name: "Create source" }));

  const alert = await screen.findByRole("alert");
  expect(within(alert).getByText("Your plan allows 1 active issue source.")).toBeInTheDocument();
  expect(await within(alert).findByRole("link", { name: "Upgrade" })).toHaveAttribute(
    "href",
    entitlementsFixture.upgrade_url,
  );
});
