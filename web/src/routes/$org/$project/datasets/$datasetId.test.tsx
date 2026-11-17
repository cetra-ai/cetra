import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { previewJson } from "@/lib/datasets";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import itemsFixture from "../../../../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01/items.json";
import itemsV1Fixture from "../../../../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01/items-v1.json";
import versionsFixture from "../../../../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01/versions.json";
import datasetFixture from "../../../../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01.json";
import experimentsFixture from "../../../../../../fixtures/api/static/experiments.json";

const DATASET_ID = "01900000-0000-7000-8000-000000000c01";
const DATASET_URL = `/acme/support-bot/datasets/${DATASET_ID}`;
const ITEMS_PATH = "/api/v1/projects/:project/datasets/:dataset/items";
const VERSIONS_PATH = "/api/v1/projects/:project/datasets/:dataset/versions";

function firstItem() {
  const item = itemsFixture.data[0];
  if (!item) {
    throw new Error("the items fixture has no item");
  }
  return item;
}

it("shows the dataset and its items", async () => {
  await renderApp(DATASET_URL);

  expect(await screen.findByRole("heading", { name: datasetFixture.name })).toBeInTheDocument();
  const table = await screen.findByRole("table", { name: "Items" });
  const rows = within(table).getAllByRole("row").slice(1);
  expect(rows).toHaveLength(itemsFixture.data.length);
  expect(within(rows[0] as HTMLElement).getByText(previewJson(firstItem().input))).toBeVisible();
});

it("answers not found for an unknown dataset", async () => {
  server.use(
    http.get("/api/v1/projects/:project/datasets/:dataset", () =>
      problemResponse(404, "not_found", { detail: "no such dataset" }),
    ),
  );

  await renderApp(DATASET_URL);

  expect(await screen.findByText("Page not found")).toBeInTheDocument();
});

it("reads a past version read-only", async () => {
  const requested: (string | null)[] = [];
  server.use(
    http.get(ITEMS_PATH, ({ request }) => {
      requested.push(new URL(request.url).searchParams.get("version"));
      return HttpResponse.json(itemsV1Fixture);
    }),
  );

  await renderApp(`${DATASET_URL}?version=1`);

  expect(
    await screen.findByText("You are viewing version 1. Items are read-only."),
  ).toBeInTheDocument();
  await waitFor(() => {
    expect(requested).toContain("1");
  });
  expect(screen.getByRole("button", { name: "Add item" })).toBeDisabled();
});

it("switches version from the selector", async () => {
  const user = userEvent.setup();
  const { router } = await renderApp(DATASET_URL);

  await user.click(await screen.findByRole("combobox", { name: "Version" }));
  await user.click(await screen.findByRole("option", { name: "v1 — add 3 items" }));

  await waitFor(() => expect(router.state.location.href).toContain("version=1"));
});

it("loads more versions", async () => {
  const user = userEvent.setup();
  const cursors: (string | null)[] = [];
  server.use(
    http.get(VERSIONS_PATH, ({ request }) => {
      const cursor = new URL(request.url).searchParams.get("cursor");
      cursors.push(cursor);
      return cursor === "c1"
        ? HttpResponse.json({ data: [versionsFixture.data[1]], next_cursor: null })
        : HttpResponse.json({ data: [versionsFixture.data[0]], next_cursor: "c1" });
    }),
  );

  await renderApp(DATASET_URL);

  await user.click(await screen.findByRole("combobox", { name: "Version" }));
  await user.click(await screen.findByRole("option", { name: "Load more versions…" }));
  await waitFor(() => expect(cursors).toContain("c1"));

  await user.click(screen.getByRole("combobox", { name: "Version" }));
  expect(await screen.findByRole("option", { name: "v1 — add 3 items" })).toBeInTheDocument();
});

it("lists the versions", async () => {
  await renderApp(`${DATASET_URL}?tab=versions`);

  const table = await screen.findByRole("table", { name: "Versions" });
  for (const version of versionsFixture.data) {
    expect(within(table).getByText(`v${version.version}`)).toBeInTheDocument();
    expect(within(table).getByText(version.description)).toBeInTheDocument();
  }
});

it("links an item to its source trace", async () => {
  const sourced = itemsFixture.data[2];
  if (!sourced) {
    throw new Error("the items fixture has no third item");
  }
  server.use(http.get(ITEMS_PATH, () => HttpResponse.json({ data: [sourced], next_cursor: null })));

  await renderApp(DATASET_URL);

  const table = await screen.findByRole("table", { name: "Items" });
  expect(within(table).getByRole("link", { name: "trace" })).toHaveAttribute(
    "href",
    `/acme/support-bot/traces/${sourced.source_trace_id}?span=${sourced.source_span_id}`,
  );
});

it("deletes the dataset after typing its name", async () => {
  const user = userEvent.setup();
  let deleted = false;
  server.use(
    http.delete("/api/v1/projects/:project/datasets/:dataset", () => {
      deleted = true;
      return new HttpResponse(null, { status: 204 });
    }),
  );
  const { router } = await renderApp(`${DATASET_URL}?tab=settings`);

  await user.click(await screen.findByRole("button", { name: "Delete dataset" }));
  const dialog = await screen.findByRole("alertdialog");
  await user.type(within(dialog).getByRole("textbox"), datasetFixture.name);
  await user.click(within(dialog).getByRole("button", { name: "Delete dataset" }));

  await waitFor(() => expect(router.state.location.pathname).toBe("/acme/support-bot/datasets"));
  expect(deleted).toBe(true);
});

it("keeps the latest version out of the URL", async () => {
  const user = userEvent.setup();
  const { router } = await renderApp(`${DATASET_URL}?tab=versions`);

  const table = await screen.findByRole("table", { name: "Versions" });
  await user.click(within(table).getByRole("link", { name: "v2" }));

  await waitFor(() => expect(router.state.location.href).toContain("tab=items"));
  expect(router.state.location.href).not.toContain("version");
});

it("drops a version param that names the latest version", async () => {
  const { router } = await renderApp(`${DATASET_URL}?version=2`);

  await waitFor(() => expect(router.state.location.href).not.toContain("version"));
  expect(await screen.findByRole("combobox", { name: "Version" })).toHaveTextContent("Latest (v2)");
  expect(screen.queryByText(/Items are read-only/)).not.toBeInTheDocument();
});

it("treats a version above the latest one as the latest", async () => {
  const { router } = await renderApp(`${DATASET_URL}?version=9`);

  await waitFor(() => expect(router.state.location.href).not.toContain("version"));
  expect(screen.queryByText(/Items are read-only/)).not.toBeInTheDocument();
});

it("shows the new latest version, editable, after an item is added", async () => {
  const user = userEvent.setup();
  let latest = 2;
  server.use(
    http.get("/api/v1/projects/:project/datasets/:dataset", () =>
      HttpResponse.json({ ...datasetFixture, current_version: latest, item_count: latest + 1 }),
    ),
    http.post(ITEMS_PATH, () => {
      latest = 3;
      return HttpResponse.json({ version: 3, item_ids: [] }, { status: 201 });
    }),
  );
  await renderApp(DATASET_URL);

  expect(await screen.findByText(/version 2/)).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Add item" }));
  const dialog = await screen.findByRole("dialog");
  await user.click(within(dialog).getByRole("textbox", { name: "Input" }));
  await user.paste('{"question":"new"}');
  await user.click(within(dialog).getByRole("button", { name: "Add item" }));

  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  expect(await screen.findByText(/version 3/)).toBeInTheDocument();
  expect(screen.queryByText(/Items are read-only/)).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Add item" })).toBeEnabled();
});

it("lists the dataset's experiments on the Experiments tab", async () => {
  const requested: (string | null)[] = [];
  server.use(
    http.get("/api/v1/projects/:project/experiments", ({ request }) => {
      requested.push(new URL(request.url).searchParams.get("dataset"));
      return HttpResponse.json(experimentsFixture);
    }),
  );

  await renderApp(`${DATASET_URL}?tab=experiments`);

  const table = await screen.findByRole("table", { name: "Experiments" });
  expect(requested).toContain(DATASET_ID);
  expect(within(table).queryByRole("columnheader", { name: "Dataset" })).toBeNull();
  expect(within(table).getAllByRole("row").slice(1)).toHaveLength(experimentsFixture.data.length);
});
