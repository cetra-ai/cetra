import { screen, waitFor, within } from "@testing-library/react";
import type userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { itemShortId } from "@/lib/datasets";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";

const DATASET_ID = "01900000-0000-7000-8000-000000000c01";
const DATASET_URL = `/acme/support-bot/datasets/${DATASET_ID}?tab=versions`;
const VERSIONS_PATH = "/api/v1/projects/:project/datasets/:dataset/versions";
const CHANGES_PATH = "/api/v1/projects/:project/datasets/:dataset/versions/:version/changes";

const ADDED_ID = "01900000-0000-7000-8000-000000000e01";
const EDITED_ID = "01900000-0000-7000-8000-000000000e02";
const DELETED_ID = "01900000-0000-7000-8000-000000000e03";

/** A revision of an item, with only the fields the list reads. */
function revision(id: string, version: number, input: unknown, metadata: unknown) {
  return {
    id,
    revision_id: `${id}-r${version}`,
    version,
    input,
    metadata,
    created_at: "2026-01-02T00:00:00.000000000Z",
  };
}

/** Version 2 changed three items; the first page holds two, the second the deleted one. */
const FIRST_PAGE = {
  data: [
    {
      item_id: ADDED_ID,
      change: "added",
      after: revision(ADDED_ID, 2, { question: "How do I reset my key?" }, {}),
    },
    {
      item_id: EDITED_ID,
      change: "edited",
      before: revision(EDITED_ID, 1, { question: "Export?" }, { topic: "data" }),
      after: revision(EDITED_ID, 2, { question: "Export?" }, { topic: "billing" }),
    },
  ],
  next_cursor: "page-2",
};
const SECOND_PAGE = {
  data: [
    {
      item_id: DELETED_ID,
      change: "deleted",
      before: revision(DELETED_ID, 1, { question: "Old question about invoices" }, {}),
    },
  ],
  next_cursor: null,
};

function useChanges() {
  server.use(
    http.get(VERSIONS_PATH, () =>
      HttpResponse.json({
        data: [
          {
            version: 2,
            description: "import mixed.csv",
            created_at: "2026-01-02T00:00:00.000000000Z",
            added: 1,
            edited: 1,
            deleted: 1,
          },
        ],
        next_cursor: null,
      }),
    ),
    http.get(CHANGES_PATH, ({ request }) => {
      const cursor = new URL(request.url).searchParams.get("cursor");
      return HttpResponse.json(cursor === "page-2" ? SECOND_PAGE : FIRST_PAGE);
    }),
  );
}

async function openChanges(user: ReturnType<typeof userEvent.setup>) {
  const list = await screen.findByRole("list", { name: "Versions" });
  await user.click(within(list).getByRole("button", { name: "Changes" }));
  return list;
}

it("shows each change with its item link", async () => {
  useChanges();
  const { user } = await renderApp(DATASET_URL);
  const list = await openChanges(user);

  expect(await within(list).findByText("question: How do I reset my key?")).toBeInTheDocument();
  const added = within(list).getByRole("link", { name: itemShortId(ADDED_ID) });
  expect(added.getAttribute("href")).toContain(`item=${ADDED_ID}`);
  expect(added.getAttribute("href")).not.toContain("version=");
});

it("diffs an edit's input and shows its metadata only when it changed", async () => {
  useChanges();
  const { user } = await renderApp(DATASET_URL);
  const list = await openChanges(user);

  await within(list).findByText("Edited");
  expect(within(list).getByText(/"topic": "billing"/)).toBeInTheDocument();
  expect(within(list).getByText("Before")).toBeInTheDocument();
  expect(within(list).getByText("After")).toBeInTheDocument();
});

it("hides the metadata block of an edit whose metadata did not change", async () => {
  server.use(
    http.get(VERSIONS_PATH, () =>
      HttpResponse.json({
        data: [
          {
            version: 2,
            description: "edit item",
            created_at: "2026-01-02T00:00:00.000000000Z",
            added: 0,
            edited: 1,
            deleted: 0,
          },
        ],
        next_cursor: null,
      }),
    ),
    http.get(CHANGES_PATH, () =>
      HttpResponse.json({
        data: [
          {
            item_id: EDITED_ID,
            change: "edited",
            before: revision(EDITED_ID, 1, { question: "a" }, { topic: "data" }),
            after: revision(EDITED_ID, 2, { question: "b" }, { topic: "data" }),
          },
        ],
        next_cursor: null,
      }),
    ),
  );
  const { user } = await renderApp(DATASET_URL);
  const list = await openChanges(user);

  await within(list).findByText("Edited");
  expect(within(list).queryByText("Metadata")).toBeNull();
  expect(within(list).queryByText("Before")).toBeNull();
});

it("links a deleted item to the version before the deletion, and pages the rest", async () => {
  useChanges();
  const { user } = await renderApp(DATASET_URL);
  const list = await openChanges(user);

  await within(list).findByText("question: How do I reset my key?");
  expect(within(list).queryByRole("link", { name: itemShortId(DELETED_ID) })).toBeNull();

  await user.click(within(list).getByRole("button", { name: "Load more changes" }));

  const deleted = await within(list).findByRole("link", { name: itemShortId(DELETED_ID) });
  expect(deleted.getAttribute("href")).toContain("version=1");
  expect(deleted.getAttribute("href")).toContain(`item=${DELETED_ID}`);
  expect(within(list).getByText("question: Old question about invoices")).toBeInTheDocument();
  await waitFor(() =>
    expect(within(list).queryByRole("button", { name: "Load more changes" })).toBeNull(),
  );
});
