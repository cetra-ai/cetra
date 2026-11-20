import { screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it, vi } from "vitest";
import { orgAssistantKey } from "@/api/queries-assistant";
import { server } from "@/mocks/node";
import { ORG_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import meFixture from "../../../../../fixtures/api/static/me.json";

const PAGE = "/acme/settings/assistant";
const CONNECTION_ID = "01900000-0000-7000-8000-000000000400";
const MEMBER_REASON = "Only admins and owners can change the assistant.";

function meAs(role: string) {
  return http.get("/api/auth/me", () =>
    HttpResponse.json({
      user: meFixture.user,
      orgs: [{ id: ORG_ID, slug: "acme", name: "Acme", role }],
    }),
  );
}

it("shows the setting read-only to members", async () => {
  server.use(meAs("member"));

  await renderApp(PAGE);

  expect(await screen.findByRole("switch", { name: "Turn on the assistant" })).toBeDisabled();
  expect(screen.getByRole("combobox", { name: "Connection" })).toBeDisabled();
  expect(screen.getByRole("textbox", { name: "Model" })).toBeDisabled();
  const save = screen.getByRole("button", { name: "Save" });
  expect(save).toBeDisabled();
  expect(save.parentElement).toHaveAccessibleDescription(MEMBER_REASON);
});

it("turns the assistant on and saves the connection and model", async () => {
  let current: Record<string, unknown> = { enabled: false };
  let body: unknown;
  server.use(
    http.get("/api/v1/orgs/:org/assistant", () => HttpResponse.json(current)),
    http.put("/api/v1/orgs/:org/assistant", async ({ request }) => {
      body = await request.json();
      current = {
        ...(body as Record<string, unknown>),
        updated_at: "2026-01-02T00:00:00.000000000Z",
      };
      return HttpResponse.json(current);
    }),
  );

  const { user, queryClient } = await renderApp(PAGE);
  const invalidate = vi.spyOn(queryClient, "invalidateQueries");

  await user.click(await screen.findByRole("switch", { name: "Turn on the assistant" }));
  await user.click(screen.getByRole("combobox", { name: "Connection" }));
  await user.click(await screen.findByRole("option", { name: "OpenAI" }));
  const model = screen.getByRole("textbox", { name: "Model" });
  await user.clear(model);
  await user.type(model, "gpt-4o");
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() =>
    expect(body).toEqual({ enabled: true, connection_id: CONNECTION_ID, model: "gpt-4o" }),
  );
  expect(await screen.findByRole("status")).toHaveTextContent("Saved");
  expect(invalidate).toHaveBeenCalledWith({ queryKey: orgAssistantKey("acme") });
});

it("shows a failed test under the model", async () => {
  server.use(
    http.put("/api/v1/orgs/:org/assistant", () =>
      problemResponse(400, "bad_request", {
        errors: [{ field: "model", message: "the model must support tool calling" }],
      }),
    ),
  );

  const { user } = await renderApp(PAGE);
  const model = await screen.findByRole("textbox", { name: "Model" });
  await user.clear(model);
  await user.type(model, "gpt-3.5-turbo");
  await user.click(screen.getByRole("button", { name: "Save" }));

  expect(await screen.findByText("the model must support tool calling")).toBeInTheDocument();
  expect(model).toHaveAttribute("aria-invalid", "true");
  expect(screen.getByRole("switch", { name: "Turn on the assistant" })).toBeChecked();
  expect(screen.getByRole("combobox", { name: "Connection" })).toHaveTextContent("OpenAI");
});

it("runs the tool-calling test and shows a failure with its hint", async () => {
  server.use(
    http.post("/api/v1/orgs/:org/assistant/test", () =>
      HttpResponse.json({
        ok: false,
        model: "gpt-4o",
        latency_ms: 12,
        error: { class: "permanent", message: "the model does not accept tools" },
      }),
    ),
  );

  const { user } = await renderApp(PAGE);
  await user.click(await screen.findByRole("button", { name: "Test" }));

  expect(await screen.findByText("Test failed")).toBeInTheDocument();
  expect(screen.getByText("the model does not accept tools")).toBeInTheDocument();
  expect(
    screen.getByText(
      "The request was rejected. Check the model name, base URL and structured output mode.",
    ),
  ).toBeInTheDocument();
});

it("links to LLM connections when the org has none", async () => {
  server.use(
    http.get("/api/v1/orgs/:org/llm-connections", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );

  await renderApp(PAGE);

  expect(await screen.findByText(/Add an LLM connection first\./)).toBeInTheDocument();
  expect(screen.getByRole("combobox", { name: "Connection" })).toBeDisabled();
  expect(screen.getByRole("link", { name: "Open LLM connections" })).toHaveAttribute(
    "href",
    "/acme/settings/llm-connections",
  );
});
