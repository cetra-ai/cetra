import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { SLACK_CHANNEL_ID } from "@/test/fixtures";
import { renderApp } from "@/test/render";
import channelsFixture from "../../../../../../../fixtures/api/static/channels.json";
import sourceFixture from "../../../../../../../fixtures/api/static/issue-source.json";
import projectFixture from "../../../../../../../fixtures/api/static/project.json";

const SOURCE_ID = sourceFixture.id;
const EDIT_URL = `/acme/support-bot/issue-sources/${SOURCE_ID}/edit`;
const LIST_URL = "/acme/support-bot/issue-sources";
const EMAIL_ID = channelsFixture.data[0].id;

/** Serves the fixture source on its detail route. */
function found() {
  server.use(
    http.get("/api/v1/projects/:project/issue-sources/:id", () => HttpResponse.json(sourceFixture)),
  );
}

/** Serves the fixture source with the given notification channels on its detail route. */
function foundWithChannels(channelIds: string[]) {
  server.use(
    http.get("/api/v1/projects/:project/issue-sources/:id", () =>
      HttpResponse.json({ ...sourceFixture, channel_ids: channelIds }),
    ),
  );
}

/** Records the body of the next PATCH of the source and answers it with the fixture, changed. */
function capturePatch(): { body: () => unknown; calls: () => number } {
  let body: unknown;
  let calls = 0;
  server.use(
    http.patch("/api/v1/projects/:project/issue-sources/:id", async ({ request }) => {
      calls += 1;
      body = await request.json();
      return HttpResponse.json({ ...sourceFixture, ...(body as Record<string, unknown>) });
    }),
  );
  return { body: () => body, calls: () => calls };
}

/** Records the next DELETE of the source. */
function captureDelete(): { calls: () => number } {
  let calls = 0;
  server.use(
    http.delete("/api/v1/projects/:project/issue-sources/:id", () => {
      calls += 1;
      return new HttpResponse(null, { status: 204 });
    }),
  );
  return { calls: () => calls };
}

it("sends only the changed fields", async () => {
  found();
  const patched = capturePatch();
  const { user, router } = await renderApp(EDIT_URL);

  const name = await screen.findByLabelText("Name");
  await user.clear(name);
  await user.type(name, "Errors v2");
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => expect(patched.body()).toEqual({ name: "Errors v2" }));
  await waitFor(() => expect(router.state.location.pathname).toBe(LIST_URL));
  await waitFor(() => expect(router.state.status).toBe("idle"));
});

it("shows the stored channels ticked", async () => {
  foundWithChannels([EMAIL_ID, SLACK_CHANNEL_ID]);
  await renderApp(EDIT_URL);

  expect(await screen.findByRole("checkbox", { name: "On-call email (Email)" })).toBeChecked();
  expect(screen.getByRole("checkbox", { name: "Team Slack (Slack)" })).toBeChecked();
  expect(screen.getByRole("checkbox", { name: "Pager webhook (Webhook)" })).not.toBeChecked();
});

it("sends the channels that are left when one is unticked", async () => {
  foundWithChannels([EMAIL_ID, SLACK_CHANNEL_ID]);
  const patched = capturePatch();
  const { user } = await renderApp(EDIT_URL);

  await user.click(await screen.findByRole("checkbox", { name: "Team Slack (Slack)" }));
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => expect(patched.body()).toEqual({ channel_ids: [EMAIL_ID] }));
});

it("sends no channel list when the channels are unchanged", async () => {
  foundWithChannels([EMAIL_ID, SLACK_CHANNEL_ID]);
  const patched = capturePatch();
  const { user } = await renderApp(EDIT_URL);

  const name = await screen.findByLabelText("Name");
  await user.clear(name);
  await user.type(name, "Errors v2");
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => expect(patched.body()).toEqual({ name: "Errors v2" }));
});

it("sends an empty filter to clear it", async () => {
  found();
  const patched = capturePatch();
  const { user } = await renderApp(EDIT_URL);

  await user.clear(await screen.findByLabelText("Filter"));
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => expect(patched.body()).toEqual({ filter: "" }));
});

it("shows the kind as read-only text, not a selector", async () => {
  found();
  await renderApp(EDIT_URL);

  expect(await screen.findByText("Errors", { selector: "p" })).toBeInTheDocument();
  expect(screen.queryByRole("combobox", { name: "Kind" })).toBeNull();
});

it("asks before deleting, with the open issue count, and deletes on confirm", async () => {
  found();
  const deleted = captureDelete();
  const { user, router } = await renderApp(EDIT_URL);

  await user.click(await screen.findByRole("button", { name: "Delete" }));
  const dialog = await screen.findByRole("alertdialog");
  expect(
    within(dialog).getByText(
      "Delete this source and its 2 open issues? Their occurrence scores stay on the traces.",
    ),
  ).toBeInTheDocument();
  await user.click(within(dialog).getByRole("button", { name: "Delete" }));

  await waitFor(() => expect(deleted.calls()).toBe(1));
  await waitFor(() => expect(router.state.location.pathname).toBe(LIST_URL));
});

it("disables Save and Delete for a viewer", async () => {
  found();
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );
  await renderApp(EDIT_URL);

  expect(await screen.findByRole("button", { name: "Delete" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
});
