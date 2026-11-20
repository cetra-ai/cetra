import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import { recordRequests } from "@/test/requests";
import queueFixture from "../../../../../../../fixtures/api/static/annotation-queue.json";
import itemsFixture from "../../../../../../../fixtures/api/static/annotation-queue-items.json";

const QUEUE_URL = `/acme/support-bot/annotation-queues/${queueFixture.id}`;
const QUEUE_PATH = "/api/v1/projects/:project/annotation-queues/:id";
const ITEMS_PATH = "/api/v1/projects/:project/annotation-queues/:id/items";
const ITEM_PATH = "/api/v1/projects/:project/annotation-queues/:id/items/:item_id";

it("lists the items with their status", async () => {
  await renderApp(QUEUE_URL);

  const grid = await screen.findByRole("grid", { name: "Queue items" });
  expect(within(grid).getByText("Pending")).toBeInTheDocument();
  expect(within(grid).getByText("Completed")).toBeInTheDocument();
});

it("sends the status filter and writes it to the URL", async () => {
  const user = userEvent.setup();
  const statuses: (string | null)[] = [];
  server.use(
    http.get(ITEMS_PATH, ({ request }) => {
      statuses.push(new URL(request.url).searchParams.get("status"));
      return HttpResponse.json(itemsFixture);
    }),
  );
  const { router } = await renderApp(QUEUE_URL);
  await screen.findByRole("grid", { name: "Queue items" });

  await user.click(screen.getByRole("combobox", { name: "Status" }));
  await user.click(await screen.findByRole("option", { name: "Pending" }));

  await waitFor(() => expect(router.state.location.search).toMatchObject({ status: "pending" }));
  await waitFor(() => expect(statuses).toContain("pending"));
});

it("reopens a completed item and refetches the items", async () => {
  const user = userEvent.setup();
  const { calls, stop } = recordRequests();
  server.use(
    http.post(`${ITEM_PATH}/reopen`, () =>
      HttpResponse.json({ ...itemsFixture.data[1], status: "pending" }),
    ),
  );
  await renderApp(QUEUE_URL);
  const grid = await screen.findByRole("grid", { name: "Queue items" });
  const listed = () =>
    calls.filter((call) => call.method === "GET" && call.path.endsWith("/items")).length;
  const before = listed();

  await user.click(within(grid).getByRole("button", { name: "Reopen" }));

  await waitFor(() =>
    expect(calls.some((call) => call.method === "POST" && call.path.endsWith("/reopen"))).toBe(
      true,
    ),
  );
  await waitFor(() => expect(listed()).toBeGreaterThan(before));
  stop();
});

it("removes an item only after the confirmation", async () => {
  const user = userEvent.setup();
  const { calls, stop } = recordRequests();
  server.use(http.delete(ITEM_PATH, () => new HttpResponse(null, { status: 204 })));
  await renderApp(QUEUE_URL);
  const grid = await screen.findByRole("grid", { name: "Queue items" });

  await user.click(within(grid).getAllByRole("button", { name: "Remove" })[0] as HTMLElement);
  expect(calls.some((call) => call.method === "DELETE")).toBe(false);
  const dialog = await screen.findByRole("alertdialog");
  await user.click(within(dialog).getByRole("button", { name: "Remove" }));

  await waitFor(() =>
    expect(calls.some((call) => call.method === "DELETE" && call.path.includes("/items/"))).toBe(
      true,
    ),
  );
  stop();
});

it("opens the queue editor from the settings tab", async () => {
  const user = userEvent.setup();
  await renderApp(QUEUE_URL);

  await user.click(await screen.findByRole("tab", { name: "Settings" }));
  await user.click(await screen.findByRole("button", { name: "Edit" }));

  expect(await screen.findByRole("dialog")).toHaveTextContent("Edit queue");
});

it("deletes the queue only once its name is typed", async () => {
  const user = userEvent.setup();
  await renderApp(QUEUE_URL);

  await user.click(await screen.findByRole("tab", { name: "Settings" }));
  await user.click(await screen.findByRole("button", { name: "Delete queue" }));
  const dialog = await screen.findByRole("alertdialog");
  const confirm = within(dialog).getByRole("button", { name: "Delete queue" });
  expect(confirm).toBeDisabled();

  fireEvent.change(within(dialog).getByRole("textbox"), {
    target: { value: queueFixture.name },
  });
  expect(confirm).toBeEnabled();
});

it("answers not found for an unknown queue", async () => {
  server.use(
    http.get(QUEUE_PATH, () =>
      problemResponse(404, "not_found", { detail: "no such annotation queue" }),
    ),
  );
  await renderApp(QUEUE_URL);

  expect(await screen.findByText("Page not found")).toBeInTheDocument();
});
