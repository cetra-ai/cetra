import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import ruleFixture from "../../../../../../../fixtures/api/static/online-rule.json";
import projectFixture from "../../../../../../../fixtures/api/static/project.json";

const RULE_ID = "01900000-0000-7000-8000-000000000700";
const EDIT_URL = `/acme/support-bot/online-rules/${RULE_ID}/edit`;

/** Records the body of the next PATCH of the rule and answers it with the fixture rule. */
function capturePatch(): { body: () => unknown; calls: () => number } {
  let body: unknown;
  let calls = 0;
  server.use(
    http.patch("/api/v1/projects/:project/online-rules/:id", async ({ request }) => {
      calls += 1;
      body = await request.json();
      return HttpResponse.json({ ...ruleFixture, ...(body as Record<string, unknown>) });
    }),
  );
  return { body: () => body, calls: () => calls };
}

it("sends only the changed fields", async () => {
  const patched = capturePatch();
  const { user, router } = await renderApp(EDIT_URL);

  const name = await screen.findByLabelText("Name");
  await user.clear(name);
  await user.type(name, "Relevance on answers v2");
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => expect(patched.body()).toEqual({ name: "Relevance on answers v2" }));
  await waitFor(() =>
    expect(router.state.location.pathname).toBe("/acme/support-bot/online-rules"),
  );
});

it("sends clear_budget when the spend limit is unchecked", async () => {
  const patched = capturePatch();
  const { user } = await renderApp(EDIT_URL);

  const limit = await screen.findByRole("checkbox", { name: "Limit spend" });
  expect(limit).toBeChecked();
  await user.click(limit);
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => expect(patched.body()).toEqual({ clear_budget: true }));
});

it("disables the evaluator and the target", async () => {
  await renderApp(EDIT_URL);

  expect(await screen.findByRole("combobox", { name: "Evaluator" })).toBeDisabled();
  expect(screen.getByRole("radio", { name: "Traces" })).toBeDisabled();
  expect(screen.getByRole("radio", { name: "Spans" })).toBeDisabled();
});

it("deletes the rule after confirming and opens the list", async () => {
  let deletes = 0;
  server.use(
    http.delete("/api/v1/projects/:project/online-rules/:id", () => {
      deletes += 1;
      return new HttpResponse(null, { status: 204 });
    }),
  );
  const { user, router } = await renderApp(EDIT_URL);

  await user.click(await screen.findByRole("button", { name: "Delete" }));
  const dialog = await screen.findByRole("alertdialog");
  await user.click(within(dialog).getByRole("button", { name: "Delete" }));

  await waitFor(() => expect(deletes).toBe(1));
  await waitFor(() =>
    expect(router.state.location.pathname).toBe("/acme/support-bot/online-rules"),
  );
});

it("shows a viewer a read-only form", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );
  await renderApp(EDIT_URL);

  expect(await screen.findByLabelText("Name")).toBeDisabled();
  expect(screen.getByRole("button", { name: "Delete" })).toBeDisabled();
  const save = screen.getByRole("button", { name: "Save" });
  expect(save).toBeDisabled();
  expect(save.parentElement).toHaveAccessibleDescription("Only members can edit online rules");
});

it("saves an unchanged rule without a PATCH and opens the list", async () => {
  const patched = capturePatch();
  const { user, router } = await renderApp(EDIT_URL);

  await screen.findByLabelText("Name");
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() =>
    expect(router.state.location.pathname).toBe("/acme/support-bot/online-rules"),
  );
  expect(patched.calls()).toBe(0);
});

it("edits a rule whose evaluator is missing from the list", async () => {
  server.use(
    http.get("/api/v1/projects/:project/evaluators", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );
  await renderApp(EDIT_URL);

  const evaluator = await screen.findByRole("combobox", { name: "Evaluator" });
  expect(evaluator).toHaveTextContent("Relevance (LLM judge)");
  expect(evaluator).toBeDisabled();
  expect(screen.getByRole("checkbox", { name: "Limit spend" })).toBeChecked();
  expect(screen.queryByText("Budgets apply to LLM judge evaluators only.")).toBeNull();
});
