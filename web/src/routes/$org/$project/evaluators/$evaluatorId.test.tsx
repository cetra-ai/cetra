import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import evaluatorFixture from "../../../../../../fixtures/api/static/evaluator.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

const EVALUATOR_URL = "/acme/support-bot/evaluators/01900000-0000-7000-8000-000000000500";
const CONFIG_URL = `${EVALUATOR_URL}?tab=config`;

const WEBHOOK = {
  ...evaluatorFixture,
  id: "01900000-0000-7000-8000-000000000502",
  name: "Hook",
  kind: "webhook",
  config: { url_host: "hooks.example.com", timeout_ms: 5000, header_names: ["x-token"] },
  output_score: { name: "hook", data_type: "boolean" },
  has_signing_secret: true,
  version: 3,
};

/** Records the body of the next PATCH of an evaluator. */
function capturePatch(answer: (body: Record<string, unknown>) => Record<string, unknown>) {
  let body: Record<string, unknown> | undefined;
  server.use(
    http.patch("/api/v1/projects/:project/evaluators/:id", async ({ request }) => {
      body = (await request.json()) as Record<string, unknown>;
      return HttpResponse.json(answer(body));
    }),
  );
  return () => body;
}

function projectAs(role: string) {
  return http.get("/api/v1/projects/:project", () =>
    HttpResponse.json({ ...projectFixture, role }),
  );
}

it("opens on the overview tab and switches tabs in the URL", async () => {
  const { user, router } = await renderApp(EVALUATOR_URL);

  expect(await screen.findByRole("tab", { name: "Overview" })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  expect(await screen.findByLabelText("Prompt")).not.toBeVisible();

  await user.click(screen.getByRole("tab", { name: "Configuration" }));
  await waitFor(() => expect(screen.getByLabelText("Prompt")).toBeVisible());
  expect(router.state.location.search).toEqual({ tab: "config" });

  await user.click(screen.getByRole("tab", { name: "Overview" }));
  await waitFor(() => expect(screen.getByLabelText("Prompt")).not.toBeVisible());
  expect(router.state.location.search).toEqual({ tab: "overview" });
});

it("keeps unsaved edits when switching tabs", async () => {
  const { user } = await renderApp(CONFIG_URL);

  const prompt = await screen.findByLabelText("Prompt");
  await user.clear(prompt);
  await user.type(prompt, "Unsaved draft");
  await user.click(screen.getByRole("tab", { name: "Overview" }));
  await user.click(screen.getByRole("tab", { name: "Configuration" }));

  expect(screen.getByLabelText("Prompt")).toHaveValue("Unsaved draft");
});

it("opens the configuration tab named in the URL", async () => {
  await renderApp(CONFIG_URL);

  expect(await screen.findByRole("tab", { name: "Configuration" })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  expect(await screen.findByLabelText("Prompt")).toBeInTheDocument();
});

it("puts the test panel after the form, in the layout grid", async () => {
  await renderApp(CONFIG_URL);

  const save = await screen.findByRole("button", { name: "Save" });
  const heading = screen.getByRole("heading", { name: "Test" });

  expect(save.compareDocumentPosition(heading) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  expect(heading.closest("section")).toHaveClass("@6xl:border-l");
  expect(heading.closest("section")?.parentElement).toHaveClass("@6xl:grid-cols-2");
});

it("sends the whole config when the prompt changes", async () => {
  const patched = capturePatch((body) => ({ ...evaluatorFixture, ...body }));
  const { user } = await renderApp(CONFIG_URL);

  const prompt = await screen.findByLabelText("Prompt");
  await user.clear(prompt);
  await user.type(prompt, "Rate the answer.");
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() =>
    expect(patched()).toEqual({
      config: {
        connection_id: evaluatorFixture.config.connection_id,
        model: "gpt-5-mini",
        prompt: "Rate the answer.",
        output: { type: "numeric", min: 0, max: 1 },
        include_reason: true,
        max_output_tokens: 1024,
        timeout_ms: 60000,
      },
    }),
  );
});

it("renames only when just the name changes", async () => {
  const patched = capturePatch((body) => ({ ...evaluatorFixture, ...body }));
  const { user } = await renderApp(CONFIG_URL);

  const name = await screen.findByLabelText("Name");
  await user.clear(name);
  await user.type(name, "Relevance v2");
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => expect(patched()).toEqual({ name: "Relevance v2" }));
});

it("sends no webhook settings or secret unless they are replaced", async () => {
  server.use(
    http.get("/api/v1/projects/:project/evaluators/:id", () => HttpResponse.json(WEBHOOK)),
  );
  const patched = capturePatch((body) => ({ ...WEBHOOK, ...body }));
  const { user } = await renderApp(`/acme/support-bot/evaluators/${WEBHOOK.id}?tab=config`);

  expect(await screen.findByText("Endpoint host: hooks.example.com")).toBeInTheDocument();
  const score = screen.getByLabelText("Score name");
  await user.clear(score);
  await user.type(score, "hook_ok");
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() =>
    expect(patched()).toEqual({
      output_score: { name: "hook_ok", data_type: "boolean" },
    }),
  );
});

it("replaces the webhook settings and the secret together", async () => {
  server.use(
    http.get("/api/v1/projects/:project/evaluators/:id", () => HttpResponse.json(WEBHOOK)),
  );
  const patched = capturePatch((body) => ({ ...WEBHOOK, ...body }));
  const { user } = await renderApp(`/acme/support-bot/evaluators/${WEBHOOK.id}?tab=config`);

  await user.click(await screen.findByRole("checkbox", { name: "Replace webhook settings" }));
  await user.type(screen.getByLabelText("URL"), "https://hooks.example.com/v2");
  await user.type(screen.getByLabelText("Header value 1"), "abc");
  await user.type(screen.getByLabelText("Signing secret"), "fedcba9876543210");
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() =>
    expect(patched()).toEqual({
      config: {
        url: "https://hooks.example.com/v2",
        timeout_ms: 5000,
        headers: { "x-token": "abc" },
      },
      webhook_secret: "fedcba9876543210",
    }),
  );
});

it("deletes after confirming and returns to the list", async () => {
  let deleted = false;
  server.use(
    http.delete("/api/v1/projects/:project/evaluators/:id", () => {
      deleted = true;
      return new Response(null, { status: 204 });
    }),
  );
  const { user, router } = await renderApp(EVALUATOR_URL);

  await user.click(await screen.findByRole("button", { name: "Delete" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Delete Relevance?" });
  await user.click(within(dialog).getByRole("button", { name: "Delete" }));

  await waitFor(() => expect(deleted).toBe(true));
  await waitFor(() => expect(router.state.location.pathname).toBe("/acme/support-bot/evaluators"));
});

it("shows a viewer a read-only form", async () => {
  server.use(projectAs("viewer"));
  await renderApp(CONFIG_URL);

  expect(await screen.findByLabelText("Name")).toBeDisabled();
  expect(screen.getByLabelText("Prompt")).toBeDisabled();
  expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Delete" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Save" }).parentElement).toHaveAccessibleDescription(
    "Only members can edit evaluators",
  );
});

it("shows the not-found page for an unknown evaluator", async () => {
  server.use(
    http.get("/api/v1/projects/:project/evaluators/:id", () => problemResponse(404, "not_found")),
  );
  await renderApp(EVALUATOR_URL);

  expect(await screen.findByRole("heading", { name: "Page not found" })).toBeInTheDocument();
});

const CONFLICT_RULE = "01900000-0000-7000-8000-000000000700";
const CONFLICT_MESSAGE = "Relevance on answers uses this evaluator";
const CONFLICT = {
  detail: "online rules use this evaluator",
  errors: [{ field: `rules.${CONFLICT_RULE}`, message: CONFLICT_MESSAGE }],
};

it("lists the rules that refuse a delete", async () => {
  server.use(
    http.delete("/api/v1/projects/:project/evaluators/:id", () =>
      problemResponse(409, "conflict", CONFLICT),
    ),
  );
  const { user } = await renderApp(EVALUATOR_URL);

  await user.click(await screen.findByRole("button", { name: "Delete" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Delete Relevance?" });
  await user.click(within(dialog).getByRole("button", { name: "Delete" }));

  expect(await within(dialog).findByText("online rules use this evaluator")).toBeInTheDocument();
  expect(within(dialog).getByRole("link", { name: CONFLICT_MESSAGE })).toHaveAttribute(
    "href",
    `/acme/support-bot/online-rules?rule=${CONFLICT_RULE}`,
  );
});

it("lists the rules that refuse a save", async () => {
  server.use(
    http.patch("/api/v1/projects/:project/evaluators/:id", () =>
      problemResponse(409, "conflict", CONFLICT),
    ),
  );
  const { user } = await renderApp(CONFIG_URL);

  const name = await screen.findByLabelText("Name");
  await user.clear(name);
  await user.type(name, "Relevance v2");
  await user.click(screen.getByRole("button", { name: "Save" }));

  // The save's message is the rule's message (no detail): it is the form error, and the link to the rule.
  expect(await screen.findByRole("link", { name: CONFLICT_MESSAGE })).toHaveAttribute(
    "href",
    `/acme/support-bot/online-rules?rule=${CONFLICT_RULE}`,
  );
});

it("opens the history tab from the URL", async () => {
  await renderApp(`${EVALUATOR_URL}?tab=history`);

  expect(await screen.findByRole("button", { name: /^v1/ })).toBeInTheDocument();
});

it("opens the history tab from the tab list", async () => {
  const { user, router } = await renderApp(EVALUATOR_URL);

  await user.click(await screen.findByRole("tab", { name: "History" }));
  expect(await screen.findByRole("button", { name: /^v1/ })).toBeInTheDocument();
  expect(router.state.location.search).toEqual({ tab: "history" });
});

it("links back to the evaluators list when opened directly", async () => {
  await renderApp(EVALUATOR_URL);

  expect(await screen.findByRole("link", { name: "Back to evaluators" })).toHaveAttribute(
    "href",
    "/acme/support-bot/evaluators",
  );
});

it("returns to the filtered list it was opened from", async () => {
  const { user, router } = await renderApp("/acme/support-bot/evaluators?q=rel");

  await user.click(await screen.findByRole("link", { name: "Relevance" }));
  await user.click(await screen.findByRole("link", { name: "Back to Evaluators" }));

  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/evaluators");
    expect(router.state.location.search).toEqual({ q: "rel" });
  });
  await waitFor(() => {
    expect(router.state.status).toBe("idle");
  });
});

it("returns to the online rule it was opened from", async () => {
  const { user, router } = await renderApp(
    "/acme/support-bot/online-rules/01900000-0000-7000-8000-000000000700",
  );

  const details = await screen.findByRole("region", { name: "Details" });
  await user.click(await within(details).findByRole("link", { name: "Relevance" }));
  await user.click(await screen.findByRole("link", { name: "Back to Relevance on answers" }));

  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/online-rules");
  });
  expect(router.state.location.search).toMatchObject({
    rule: "01900000-0000-7000-8000-000000000700",
  });
  await waitFor(() => {
    expect(router.state.status).toBe("idle");
  });
});

it("keeps the way back when switching tabs", async () => {
  const { user, router } = await renderApp("/acme/support-bot/evaluators?q=rel");

  await user.click(await screen.findByRole("link", { name: "Relevance" }));
  await user.click(await screen.findByRole("tab", { name: "Configuration" }));

  expect(await screen.findByRole("link", { name: "Back to Evaluators" })).toBeInTheDocument();
  await waitFor(() => {
    expect(router.state.status).toBe("idle");
  });
});

it("names the evaluator in the breadcrumb", async () => {
  await renderApp(EVALUATOR_URL);

  // The top bar comes first in the document; a page may have its own breadcrumb navigation.
  const [crumbs] = await screen.findAllByRole("navigation", { name: "Breadcrumb" });
  if (!crumbs) {
    throw new Error("no breadcrumb navigation");
  }
  expect(await within(crumbs).findByText(evaluatorFixture.name)).toBeInTheDocument();
});
