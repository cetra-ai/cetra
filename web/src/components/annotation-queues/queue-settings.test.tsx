import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import queueFixture from "../../../../fixtures/api/static/annotation-queue.json";
import projectFixture from "../../../../fixtures/api/static/project.json";

it("leaves Edit and Delete queue disabled for a viewer", async () => {
  const user = userEvent.setup();
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );
  await renderApp(`/acme/support-bot/annotation-queues/${queueFixture.id}`);

  await user.click(await screen.findByRole("tab", { name: "Settings" }));

  expect(await screen.findByRole("button", { name: "Edit" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Delete queue" })).toBeDisabled();
});
