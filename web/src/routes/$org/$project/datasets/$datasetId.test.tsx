import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { previewFields } from "@/lib/datasets";
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
  const grid = await screen.findByRole("grid", { name: "Items" });
  // One header row and one row per item.
  expect(grid).toHaveAttribute("aria-rowcount", String(itemsFixture.data.length + 1));
  expect(within(grid).getByText(previewFields(firstItem().input))).toBeVisible();
});

it("opens the item panel on a row click and sets the item search param", async () => {
  const { router, user } = await renderApp(DATASET_URL);
  const grid = await screen.findByRole("grid", { name: "Items" });
  const row = within(grid).getByText(previewFields(firstItem().input)).closest('[role="row"]');
  if (!row) {
    throw new Error("no row for the first item");
  }

  await user.click(row);

  await waitFor(() => expect(router.state.location.search).toMatchObject({ item: firstItem().id }));
  expect(await screen.findByRole("region", { name: "Item" })).toBeInTheDocument();
});

it("shows the item, version, experiment and update counts as header chips", async () => {
  await renderApp(DATASET_URL);

  await screen.findByRole("heading", { name: datasetFixture.name });
  const chip = (label: string) => screen.getByText(label, { selector: ".sr-only" }).parentElement;
  expect(chip("Items")).toHaveTextContent(String(datasetFixture.item_count));
  expect(chip("Version")).toHaveTextContent(`v${datasetFixture.current_version}`);
  expect(chip("Experiments")).toHaveTextContent(String(datasetFixture.experiment_count));
  expect(chip("Updated")).toBeInTheDocument();
});

it("offers the first items on an empty latest version", async () => {
  const user = userEvent.setup();
  server.use(http.get(ITEMS_PATH, () => HttpResponse.json({ data: [], next_cursor: null })));
  await renderApp(DATASET_URL);

  expect(await screen.findByRole("heading", { name: "No items yet" })).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Import a file" }));
  expect(await screen.findByRole("dialog")).toBeInTheDocument();
});

it("says a past version has no items", async () => {
  server.use(http.get(ITEMS_PATH, () => HttpResponse.json({ data: [], next_cursor: null })));
  await renderApp(`${DATASET_URL}?version=1`);

  expect(await screen.findByText("No items in this version")).toBeInTheDocument();
  expect(screen.queryByRole("heading", { name: "No items yet" })).toBeNull();
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

  const list = await screen.findByRole("list", { name: "Versions" });
  for (const version of versionsFixture.data) {
    expect(within(list).getByText(`v${version.version}`)).toBeInTheDocument();
    expect(within(list).getByText(version.description)).toBeInTheDocument();
  }
});

it("links an item to its source trace", async () => {
  const sourced = itemsFixture.data[2];
  if (!sourced) {
    throw new Error("the items fixture has no third item");
  }
  server.use(http.get(ITEMS_PATH, () => HttpResponse.json({ data: [sourced], next_cursor: null })));

  await renderApp(DATASET_URL);

  const grid = await screen.findByRole("grid", { name: "Items" });
  expect(within(grid).getByRole("link", { name: /^trace / })).toHaveAttribute(
    "href",
    `/acme/support-bot/traces/${sourced.source_trace_id}?span=${sourced.source_span_id}`,
  );
});

/** The source item of the fixture, for the trace tooltip tests. */
function sourcedItem() {
  const sourced = itemsFixture.data[2];
  if (!sourced) {
    throw new Error("the items fixture has no third item");
  }
  return sourced;
}

it("shows the source trace's name and duration on hover", async () => {
  const sourced = sourcedItem();
  server.use(
    http.get(ITEMS_PATH, () => HttpResponse.json({ data: [sourced], next_cursor: null })),
    http.get("/api/v1/projects/:project/traces/:trace_id", () =>
      HttpResponse.json({
        trace: {
          name: "support-chat",
          start_time: "2026-01-02T03:04:05.000000000Z",
          end_time: "2026-01-02T03:04:07.000000000Z",
          duration_ns: 2_000_000_000,
          error_count: 0,
          has_root: true,
          llm_count: 1,
          metadata: {},
          models: [],
        },
        spans: [],
        truncated: false,
      }),
    ),
  );
  const { user } = await renderApp(DATASET_URL);

  await screen.findByRole("link", { name: `trace ${sourced.source_trace_id?.slice(0, 8)}` });
  // The tooltip's trigger is the span around the link. The first pointer enter re-renders it, so
  // each hover looks the span up again.
  const trigger = () => screen.getByRole("link", { name: /^trace / }).parentElement as HTMLElement;
  await user.hover(trigger());
  await user.unhover(trigger());
  await user.hover(trigger());

  expect(await screen.findByText("support-chat")).toBeInTheDocument();
});

it("says the source trace is gone when it no longer exists", async () => {
  const sourced = sourcedItem();
  server.use(
    http.get(ITEMS_PATH, () => HttpResponse.json({ data: [sourced], next_cursor: null })),
    http.get("/api/v1/projects/:project/traces/:trace_id", () =>
      problemResponse(404, "not_found", {}),
    ),
  );
  const { user } = await renderApp(DATASET_URL);

  await screen.findByRole("link", { name: `trace ${sourced.source_trace_id?.slice(0, 8)}` });
  // The tooltip's trigger is the span around the link. The first pointer enter re-renders it, so
  // each hover looks the span up again.
  const trigger = () => screen.getByRole("link", { name: /^trace / }).parentElement as HTMLElement;
  await user.hover(trigger());
  await user.unhover(trigger());
  await user.hover(trigger());

  expect(await screen.findByText("Trace no longer available")).toBeInTheDocument();
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

it.each([2, 9])(
  "drops a version at or above the latest one from the URL (version %i)",
  async (version) => {
    const { router } = await renderApp(`${DATASET_URL}?tab=versions&version=${version}`);

    await waitFor(() => expect(router.state.location.search).not.toHaveProperty("version"));
    const list = await screen.findByRole("list", { name: "Versions" });
    const latestRow = within(list).getByText("v2").closest("li");
    if (!latestRow) {
      throw new Error("no row for v2");
    }
    expect(within(latestRow).getByText("latest")).toBeInTheDocument();
    expect(within(latestRow).queryByRole("link", { name: "View" })).toBeNull();
  },
);

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

  expect(await screen.findByText("v2")).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Add item" }));
  const dialog = await screen.findByRole("dialog");
  await user.click(within(dialog).getByRole("textbox", { name: "Input" }));
  await user.paste('{"question":"new"}');
  await user.click(within(dialog).getByRole("button", { name: "Add item" }));

  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  expect(await screen.findByText("v3")).toBeInTheDocument();
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

it("sends the item search from the q param and says when no item matches", async () => {
  const searches: (string | null)[] = [];
  server.use(
    http.get(ITEMS_PATH, ({ request }) => {
      searches.push(new URL(request.url).searchParams.get("q"));
      return HttpResponse.json({ data: [], next_cursor: null });
    }),
  );
  await renderApp(`${DATASET_URL}?q=refund`);

  expect(await screen.findByText("No item matches")).toBeInTheDocument();
  expect(searches).toContain("refund");
  expect(searches).not.toContain(null);
});

it("writes the typed item search into the q param", async () => {
  const { router, user } = await renderApp(DATASET_URL);
  await screen.findByRole("grid", { name: "Items" });

  await user.type(screen.getByRole("searchbox", { name: "Search items" }), "refund{Enter}");

  await waitFor(() => expect(router.state.location.search).toMatchObject({ q: "refund" }));
});

it("links back to the datasets list with the dataset open after a direct visit", async () => {
  await renderApp(DATASET_URL);

  const back = await screen.findByRole("link", { name: "Back to datasets" });
  const target = new URL(back.getAttribute("href") ?? "", "http://localhost");
  expect(target.pathname).toBe("/acme/support-bot/datasets");
  expect(target.searchParams.get("dataset")).toBe(DATASET_ID);
});

it("goes back to the datasets list from a dataset opened there", async () => {
  const { router, user } = await renderApp(`/acme/support-bot/datasets?dataset=${DATASET_ID}`);

  await user.click(await screen.findByRole("link", { name: "Open page" }));
  expect(await screen.findByRole("heading", { name: datasetFixture.name })).toBeInTheDocument();
  await user.click(screen.getByRole("link", { name: "Back to Datasets" }));

  await waitFor(() => expect(router.state.location.pathname).toBe("/acme/support-bot/datasets"));
  expect(router.state.location.search).toMatchObject({ dataset: DATASET_ID });
  await waitFor(() => expect(router.state.status).toBe("idle"));
});

it("keeps the way back to the list after a tab and a version change", async () => {
  const { router, user } = await renderApp(`/acme/support-bot/datasets?dataset=${DATASET_ID}`);

  await user.click(await screen.findByRole("link", { name: "Open page" }));
  await screen.findByRole("heading", { name: datasetFixture.name });

  await user.click(screen.getByRole("tab", { name: "Versions" }));
  await waitFor(() => expect(router.state.location.search).toMatchObject({ tab: "versions" }));
  expect(screen.getByRole("link", { name: "Back to Datasets" })).toBeInTheDocument();

  await user.click(screen.getByRole("combobox", { name: "Version" }));
  await user.click(await screen.findByRole("option", { name: "v1 — add 3 items" }));
  await waitFor(() => expect(router.state.location.href).toContain("version=1"));
  expect(screen.getByRole("link", { name: "Back to Datasets" })).toBeInTheDocument();
  await waitFor(() => expect(router.state.status).toBe("idle"));
});
