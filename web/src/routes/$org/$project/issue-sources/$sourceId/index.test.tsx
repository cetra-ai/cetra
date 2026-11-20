import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import listFixture from "../../../../../../../fixtures/api/static/issue-sources.json";
import refitsFixture from "../../../../../../../fixtures/api/static/issue-refits.json";
import projectFixture from "../../../../../../../fixtures/api/static/project.json";

const TOPIC = listFixture.data[1] as (typeof listFixture.data)[number];
const PAGE_URL = `/acme/support-bot/issue-sources/${TOPIC.id}`;

/** Serves the topic source and its refits. */
function found() {
  server.use(
    http.get("/api/v1/projects/:project/issue-sources/:id", () => HttpResponse.json(TOPIC)),
    http.get("/api/v1/projects/:project/issue-sources/:id/refits", () =>
      HttpResponse.json(refitsFixture),
    ),
  );
}

/** Counts the refit requests and answers them with `answer`, or 202 with the source. */
function refitted(answer?: () => Response): { calls: () => number } {
  let calls = 0;
  server.use(
    http.post("/api/v1/projects/:project/issue-sources/:id/refit", () => {
      calls += 1;
      return answer ? answer() : HttpResponse.json(TOPIC, { status: 202 });
    }),
  );
  return { calls: () => calls };
}

it("shows the refit history, newest first, with its counts and error", async () => {
  found();
  await renderApp(PAGE_URL);

  const table = await screen.findByRole("table", { name: "Refit history" });
  const rows = within(table).getAllByRole("row");
  expect(rows).toHaveLength(3);
  expect(within(rows[1] as HTMLElement).getByText("25")).toBeInTheDocument();
  expect(within(rows[2] as HTMLElement).getByText("busy")).toBeInTheDocument();
});

it("shows the candidate counts and today's spend against the budget", async () => {
  found();
  await renderApp(PAGE_URL);

  expect(await screen.findByText("12")).toBeInTheDocument();
  expect(screen.getByText(/spent today of/)).toBeInTheDocument();
});

it("posts a refit when Refit now is pressed", async () => {
  found();
  const posted = refitted();
  const { user } = await renderApp(PAGE_URL);

  await user.click(await screen.findByRole("button", { name: "Refit now" }));

  await waitFor(() => expect(posted.calls()).toBe(1));
});

it("shows the 409 when a refit started less than 10 minutes ago", async () => {
  found();
  refitted(() =>
    problemResponse(409, "conflict", { detail: "A refit started less than 10 minutes ago." }),
  );
  const { user } = await renderApp(PAGE_URL);

  await user.click(await screen.findByRole("button", { name: "Refit now" }));

  expect(await screen.findByText("A refit started less than 10 minutes ago.")).toBeInTheDocument();
});

it("disables Refit now for a viewer", async () => {
  found();
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );
  await renderApp(PAGE_URL);

  expect(await screen.findByRole("button", { name: "Refit now" })).toBeDisabled();
});

it("explains a connection pause with its detail in a banner", async () => {
  found();
  server.use(
    http.get("/api/v1/projects/:project/issue-sources/:id", () =>
      HttpResponse.json({
        ...TOPIC,
        paused_reason: "connection",
        paused_detail: "the key was refused",
      }),
    ),
  );
  await renderApp(PAGE_URL);

  expect(await screen.findByRole("alert")).toHaveTextContent(
    "the LLM connection is failing (the key was refused)",
  );
});

it("links to the edit page", async () => {
  found();
  await renderApp(PAGE_URL);

  const link = await screen.findByRole("link", { name: "Edit" });
  expect(link.getAttribute("href")).toContain(`/acme/support-bot/issue-sources/${TOPIC.id}/edit`);
});
