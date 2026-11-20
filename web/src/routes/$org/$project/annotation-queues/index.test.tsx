import { screen, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import queuesFixture from "../../../../../../fixtures/api/static/annotation-queues.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

// The fixture queue: 4 completed, 1 skipped and 12 pending items, so 5 of 17 are done.
it("lists the queues with their progress and pending count", async () => {
  const queue = queuesFixture.data[0];
  if (!queue) {
    throw new Error("the annotation queues fixture has no queue");
  }
  await renderApp("/acme/support-bot/annotation-queues");

  expect(
    screen.getAllByRole("heading", { level: 1 }).map((heading) => heading.textContent),
  ).toEqual(["Annotation queues"]);
  const grid = await screen.findByRole("grid", { name: "Annotation queues" });
  expect(within(grid).getByText(queue.name)).toBeInTheDocument();
  expect(within(grid).getByText("5 of 17 done")).toBeInTheDocument();
  expect(within(grid).getByText(String(queue.counts.pending))).toBeInTheDocument();
});

it("disables New queue for a viewer", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );
  await renderApp("/acme/support-bot/annotation-queues");

  expect(await screen.findByRole("button", { name: "New queue" })).toBeDisabled();
});

it("shows the empty state when the project has no queues", async () => {
  server.use(
    http.get("/api/v1/projects/:project/annotation-queues", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );
  await renderApp("/acme/support-bot/annotation-queues");

  expect(await screen.findByText("No annotation queues yet")).toBeInTheDocument();
  expect(screen.queryByRole("grid", { name: "Annotation queues" })).toBeNull();
});
