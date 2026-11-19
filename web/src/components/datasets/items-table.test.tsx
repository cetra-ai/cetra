import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { previewFields } from "@/lib/datasets";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import itemsFixture from "../../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01/items.json";
import deletedFixture from "../../../../fixtures/api/static/dataset-items-deleted.json";
import editedFixture from "../../../../fixtures/api/static/dataset-items-edited.json";

const DATASET_ID = "01900000-0000-7000-8000-000000000c01";
const DATASET_URL = `/acme/support-bot/datasets/${DATASET_ID}`;
const ITEMS_PATH = "/api/v1/projects/:project/datasets/:dataset/items";
const DELETE_PATH = "/api/v1/projects/:project/datasets/:dataset/items/delete";

function firstItem() {
  const item = itemsFixture.data[0];
  if (!item) {
    throw new Error("the items fixture has no item");
  }
  return item;
}

/** The row of the first item, found by its input preview. */
async function firstRow() {
  const grid = await screen.findByRole("grid", { name: "Items" });
  const row = within(grid).getByText(previewFields(firstItem().input)).closest('[role="row"]');
  if (!(row instanceof HTMLElement)) {
    throw new Error("no row for the first item");
  }
  return row;
}

/** Replaces the text of the JSON editor in the open popover. */
async function setEditorText(
  user: Awaited<ReturnType<typeof renderApp>>["user"],
  label: string,
  text: string,
) {
  const field = screen.getByRole("textbox", { name: label });
  await user.click(field);
  await user.clear(field);
  await user.paste(text);
}

it("checks rows and deletes the checked items in one batch after confirming", async () => {
  const { user } = await renderApp(DATASET_URL);
  const row = await firstRow();
  let body: unknown;
  server.use(
    http.post(DELETE_PATH, async ({ request }) => {
      body = await request.json();
      return HttpResponse.json(deletedFixture);
    }),
  );

  await user.click(within(row).getByRole("checkbox", { name: "Select row" }));
  expect(screen.getByText("1 selected")).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Delete" }));
  const dialog = await screen.findByRole("alertdialog");
  await user.click(within(dialog).getByRole("button", { name: "Delete items" }));

  await waitFor(() => expect(body).toEqual({ ids: [firstItem().id] }));
  await waitFor(() => expect(screen.queryByText("1 selected")).toBeNull());
});

it("does not open the item when its checkbox is clicked", async () => {
  const { user, router } = await renderApp(DATASET_URL);
  const row = await firstRow();

  await user.click(within(row).getByRole("checkbox", { name: "Select row" }));

  expect(router.state.location.search).not.toHaveProperty("item");
});

it("has no checkboxes and no edit toggle on a past version", async () => {
  await renderApp(`${DATASET_URL}?version=1`);

  await screen.findByRole("grid", { name: "Items" });
  expect(screen.queryByRole("checkbox", { name: "Select row" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Edit cells" })).toBeNull();
});

it("edits a cell and saves the changes as one batch", async () => {
  const { user } = await renderApp(DATASET_URL);
  const row = await firstRow();
  let body: unknown;
  server.use(
    http.patch(ITEMS_PATH, async ({ request }) => {
      body = await request.json();
      return HttpResponse.json(editedFixture);
    }),
  );

  await user.click(screen.getByRole("button", { name: "Edit cells" }));
  await user.click(within(row).getByRole("button", { name: /^Edit Metadata of item/ }));
  await setEditorText(user, "Metadata", '{"topic":"billing"}');
  await user.click(screen.getByRole("button", { name: "Apply" }));

  expect(screen.getByText("1 unsaved change")).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Save as new version" }));

  await waitFor(() => expect(screen.queryByText("1 unsaved change")).toBeNull());
  expect(body).toEqual({ items: [{ id: firstItem().id, metadata: { topic: "billing" } }] });
  // A save leaves edit mode.
  expect(screen.getByRole("button", { name: "Edit cells" })).toHaveAttribute(
    "aria-pressed",
    "false",
  );
});

it("keeps the edit mode open and puts a field error on the cell it belongs to", async () => {
  const { user } = await renderApp(DATASET_URL);
  const row = await firstRow();
  server.use(
    http.patch(ITEMS_PATH, () =>
      problemResponse(400, "validation_failed", {
        errors: [{ field: "items[0].input", message: '"q" is a required property' }],
      }),
    ),
  );

  await user.click(screen.getByRole("button", { name: "Edit cells" }));
  await user.click(within(row).getByRole("button", { name: /^Edit Metadata of item/ }));
  await setEditorText(user, "Metadata", '{"topic":"x"}');
  await user.click(screen.getByRole("button", { name: "Apply" }));
  await user.click(screen.getByRole("button", { name: "Save as new version" }));

  expect(await within(row).findByText('"q" is a required property')).toBeInTheDocument();
  expect(screen.getByText("1 unsaved change")).toBeInTheDocument();
  expect(screen.queryByRole("alert")).toBeNull();
});

it("refuses metadata that is not a JSON object and keeps the change out", async () => {
  const { user } = await renderApp(DATASET_URL);
  const row = await firstRow();

  await user.click(screen.getByRole("button", { name: "Edit cells" }));
  await user.click(within(row).getByRole("button", { name: /^Edit Metadata of item/ }));
  await setEditorText(user, "Metadata", "[1]");
  await user.click(screen.getByRole("button", { name: "Apply" }));

  expect(await screen.findByText("Must be a JSON object")).toBeInTheDocument();
  expect(screen.queryByText("1 unsaved change")).toBeNull();
});

it("asks before leaving edit mode with unsaved changes, and discards on confirm", async () => {
  const user = (await renderApp(DATASET_URL)).user;
  const row = await firstRow();
  let saved = false;
  server.use(
    http.patch(ITEMS_PATH, () => {
      saved = true;
      return HttpResponse.json(editedFixture);
    }),
  );

  await user.click(screen.getByRole("button", { name: "Edit cells" }));
  await user.click(within(row).getByRole("button", { name: /^Edit Metadata of item/ }));
  await setEditorText(user, "Metadata", '{"topic":"x"}');
  await user.click(screen.getByRole("button", { name: "Apply" }));
  await user.click(screen.getByRole("button", { name: "Edit cells" }));

  const dialog = await screen.findByRole("alertdialog");
  await user.click(within(dialog).getByRole("button", { name: "Discard changes" }));

  await waitFor(() => expect(screen.queryByText("1 unsaved change")).toBeNull());
  expect(screen.getByRole("button", { name: "Edit cells" })).toHaveAttribute(
    "aria-pressed",
    "false",
  );
  expect(saved).toBe(false);
});
