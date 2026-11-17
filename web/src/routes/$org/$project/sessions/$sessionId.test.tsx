import { screen, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";
import { ADA_ID, PROJECT_ID } from "@/test/fixtures";
import { renderApp } from "@/test/render";
import { serveAnnotationDefinitions } from "@/test/score-definitions";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

const TRACE = "a1000000000000000000000000000001";
const SECOND = "a1000000000000000000000000000002";

/** The caller's saved `resolved: Yes` on the session, so the sheet prefills it. */
const OWN_RESOLVED: Schema<"Score"> = {
  project_id: PROJECT_ID,
  score_id: "01900000-0000-7000-8000-000000000330",
  target: { type: "session", session_id: "sess-001" },
  session_id: "sess-001",
  user_id: "user-42",
  name: "resolved",
  value: { data_type: "boolean", value: true },
  source: "human",
  author_user_id: ADA_ID,
  created_at: "2026-01-01T10:00:30.000000000Z",
  updated_at: "2026-01-01T10:00:30.000000000Z",
  deleted: false,
  ingest_time: "2026-01-01T10:00:30.000000000Z",
};

it("renders sess-001 turns in order and opens the selected trace", async () => {
  const { router, user } = await renderApp("/acme/support-bot/sessions/sess-001");
  const conversation = await screen.findByRole("region", { name: "Conversation" });
  const first = within(conversation).getByText("My order 1234 hasn't arrived.");
  const second = within(conversation).getByText("Can I change the delivery address?");
  expect(first.compareDocumentPosition(second) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  expect(
    within(conversation).getByText("Your order 1234 shipped and arrives tomorrow."),
  ).toBeInTheDocument();
  expect(screen.getByText("2 turns")).toBeInTheDocument();
  expect(screen.getByText("resolved ✓")).toBeInTheDocument();
  expect(screen.queryByRole("region", { name: "Trace" })).not.toBeInTheDocument();

  const turns = within(conversation).getAllByRole("button", { name: /support-agent/ });
  expect(turns).toHaveLength(2);
  const secondTurn = turns[1];
  if (!secondTurn) {
    throw new Error("missing second turn");
  }
  await user.click(secondTurn);
  expect(router.state.location.search).toMatchObject({ trace: SECOND });
  expect(await screen.findByRole("region", { name: "Trace" })).toBeInTheDocument();
});

it("returns from a picked transcript block to the trace summary", async () => {
  const { user } = await renderApp(`/acme/support-bot/sessions/sess-001?trace=${TRACE}`);
  await user.click(await screen.findByRole("radio", { name: "Transcript" }));
  const transcript = await screen.findByRole("region", { name: "Transcript" });
  expect(await screen.findByRole("region", { name: "Trace summary" })).toBeInTheDocument();

  await user.click(within(transcript).getByRole("button", { name: /get_order_status/ }));
  expect(await screen.findByRole("region", { name: "Span" })).toBeInTheDocument();
  expect(screen.queryByRole("region", { name: "Trace summary" })).not.toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: "Trace summary" }));
  expect(await screen.findByRole("region", { name: "Trace summary" })).toBeInTheDocument();
  expect(screen.queryByRole("region", { name: "Span" })).not.toBeInTheDocument();
});

it("follows the session link from a trace header", async () => {
  const { router, user } = await renderApp(`/acme/support-bot/traces?trace=${TRACE}`);
  const header = await screen.findByRole("region", { name: "Trace" });
  await user.click(within(header).getByRole("link", { name: "sess-001" }));
  expect(router.state.location.pathname).toBe("/acme/support-bot/sessions/sess-001");
  expect(await screen.findByRole("region", { name: "Conversation" })).toBeInTheDocument();
});

it("annotates the session from the side sheet", async () => {
  serveAnnotationDefinitions();
  const bodies: unknown[] = [];
  server.use(
    http.get("/api/v1/projects/:project/scores", () =>
      HttpResponse.json({ data: [OWN_RESOLVED], next_cursor: null }),
    ),
    http.post("/api/v1/projects/:project/scores", async ({ request }) => {
      bodies.push(await request.json());
      return HttpResponse.json({ score_ids: ["01900000-0000-7000-8000-000000000320"] });
    }),
  );
  const { user } = await renderApp("/acme/support-bot/sessions/sess-001");
  await screen.findByRole("region", { name: "Conversation" });
  await user.click(screen.getByRole("button", { name: "Annotate" }));

  const sheet = await screen.findByRole("dialog", { name: "Annotate session" });
  const resolved = await within(sheet).findByRole("radiogroup", { name: "resolved" });
  expect(within(resolved).getByRole("radio", { name: "Yes" })).toBeChecked();
  await user.click(within(resolved).getByRole("radio", { name: "No" }));
  await user.click(within(sheet).getByRole("button", { name: "Save" }));

  await within(sheet).findByText("Saved.");
  expect(bodies).toEqual([
    {
      scores: [
        {
          target: { type: "session", session_id: "sess-001" },
          user_id: "user-42",
          name: "resolved",
          value: { data_type: "boolean", value: false },
        },
      ],
    },
  ]);
});

it("does not let a viewer annotate the session", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );
  await renderApp("/acme/support-bot/sessions/sess-001");
  await screen.findByRole("region", { name: "Conversation" });
  const annotate = screen.getByRole("button", { name: "Annotate" });
  expect(annotate).toBeDisabled();
  // The disabled button sits in a focusable wrapper that carries the reason.
  expect(annotate.parentElement).toHaveAccessibleDescription("Only members can annotate");
});
