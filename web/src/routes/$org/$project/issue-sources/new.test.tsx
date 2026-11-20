import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { SLACK_CHANNEL_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import createFixture from "../../../../../../fixtures/api/requests/issue-source-create.json";
import channelsFixture from "../../../../../../fixtures/api/static/channels.json";
import entitlementsFixture from "../../../../../../fixtures/api/static/org-entitlements.json";

const NEW_URL = "/acme/support-bot/issue-sources/new";
const LIST_URL = "/acme/support-bot/issue-sources";
const EMAIL_ID = channelsFixture.data[0].id;

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

  await waitFor(() => expect(created.body()).toEqual({ ...createFixture, channel_ids: [] }));
  await waitFor(() => expect(router.state.location.pathname).toBe(LIST_URL));
  await waitFor(() => expect(router.state.status).toBe("idle"));
});

it("creates a source with the channels it sends to, in the order they were ticked", async () => {
  const created = captureCreate();
  const { user } = await renderApp(NEW_URL);

  await user.type(await screen.findByLabelText("Name"), "Errors");
  await user.click(await screen.findByRole("checkbox", { name: "On-call email (Email)" }));
  await user.click(screen.getByRole("checkbox", { name: "Team Slack (Slack)" }));
  await user.click(screen.getByRole("button", { name: "Create source" }));

  await waitFor(() =>
    expect(created.body()).toEqual({
      name: "Errors",
      kind: "error",
      enabled: true,
      channel_ids: [EMAIL_ID, SLACK_CHANNEL_ID],
    }),
  );
});

it("sends an empty channel list when no channel is ticked", async () => {
  const created = captureCreate();
  const { user } = await renderApp(NEW_URL);

  await user.type(await screen.findByLabelText("Name"), "Errors");
  await user.click(screen.getByRole("button", { name: "Create source" }));

  await waitFor(() =>
    expect(created.body()).toEqual({
      name: "Errors",
      kind: "error",
      enabled: true,
      channel_ids: [],
    }),
  );
});

it("shows the notifications for the topic kind too", async () => {
  const { user } = await renderApp(NEW_URL);

  expect(await screen.findByRole("group", { name: "Notifications" })).toBeInTheDocument();
  await user.click(screen.getByRole("combobox", { name: "Kind" }));
  await user.click(await screen.findByRole("option", { name: "Topics" }));

  expect(screen.getByRole("group", { name: "Notifications" })).toBeInTheDocument();
});

it("shows a channel_ids error from the server under the channels", async () => {
  captureCreate(() =>
    problemResponse(400, "invalid_request", {
      errors: [{ field: "channel_ids", message: "Unknown channel" }],
    }),
  );
  const { user } = await renderApp(NEW_URL);

  await user.type(await screen.findByLabelText("Name"), "Errors");
  await user.click(screen.getByRole("button", { name: "Create source" }));

  const group = await screen.findByRole("group", { name: "Notifications" });
  expect(await within(group).findByText("Unknown channel")).toBeInTheDocument();
});

it("caps the notification channels at ten", async () => {
  const channels = Array.from({ length: 11 }, (_, index) => ({
    ...channelsFixture.data[0],
    kind: "slack",
    id: `01900000-0000-7000-8000-0000000006${10 + index}`,
    name: `Channel ${index}`,
  }));
  server.use(
    http.get("/api/v1/projects/:project/channels", () =>
      HttpResponse.json({ data: channels, next_cursor: null }),
    ),
  );
  const { user } = await renderApp(NEW_URL);

  for (let index = 0; index < 10; index += 1) {
    await user.click(await screen.findByRole("checkbox", { name: `Channel ${index} (Slack)` }));
  }
  expect(await screen.findByRole("checkbox", { name: "Channel 10 (Slack)" })).toBeDisabled();
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
