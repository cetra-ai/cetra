import { screen, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import queueFixture from "../../../../fixtures/api/static/annotation-queue.json";
import itemsFixture from "../../../../fixtures/api/static/annotation-queue-items.json";
import projectFixture from "../../../../fixtures/api/static/project.json";

const QUEUE_URL = `/acme/support-bot/annotation-queues/${queueFixture.id}`;

it("says so when the status filter matches no items", async () => {
  server.use(
    http.get("/api/v1/projects/:project/annotation-queues/:id/items", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );
  await renderApp(`${QUEUE_URL}?status=skipped`);

  expect(await screen.findByText("No items with this status")).toBeInTheDocument();
});

it("leaves Reopen and Remove disabled for a viewer and keeps Open", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );
  await renderApp(QUEUE_URL);
  const grid = await screen.findByRole("grid", { name: "Queue items" });

  expect(within(grid).getByRole("button", { name: "Reopen" })).toBeDisabled();
  for (const button of within(grid).getAllByRole("button", { name: "Remove" })) {
    expect(button).toBeDisabled();
  }
  expect(within(grid).getAllByRole("link", { name: "Open" })).toHaveLength(
    itemsFixture.data.length,
  );
});
