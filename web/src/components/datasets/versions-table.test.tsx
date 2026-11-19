import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import changesFixture from "../../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01/versions/2/changes.json";
import datasetFixture from "../../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01.json";
import restoredFixture from "../../../../fixtures/api/static/dataset-restored.json";

const DATASET_ID = "01900000-0000-7000-8000-000000000c01";
const DATASET_URL = `/acme/support-bot/datasets/${DATASET_ID}?tab=versions`;
const VERSIONS_PATH = "/api/v1/projects/:project/datasets/:dataset/versions";
const CHANGES_PATH = "/api/v1/projects/:project/datasets/:dataset/versions/:version/changes";
const RESTORE_PATH = "/api/v1/projects/:project/datasets/:dataset/restore";

const CURRENT = datasetFixture.current_version;
const VERSIONS = [
  {
    version: 5,
    description: "add from trace 0190aaaa",
    created_at: "2026-01-05T00:00:00.000000000Z",
    added: 1,
    edited: 0,
    deleted: 0,
  },
  {
    version: 4,
    description: "import faq.csv (120 items)",
    created_at: "2026-01-04T00:00:00.000000000Z",
    added: 120,
    edited: 0,
    deleted: 0,
  },
  {
    version: 3,
    description: "delete item 01900000-0000-7000-8000-000000000d03",
    created_at: "2026-01-03T00:00:00.000000000Z",
    added: 0,
    edited: 0,
    deleted: 1,
  },
  {
    version: 2,
    description: "edit item 01900000-0000-7000-8000-000000000d02",
    created_at: "2026-01-02T00:00:00.000000000Z",
    added: 0,
    edited: 1,
    deleted: 0,
  },
  {
    version: 1,
    description: "add 12 items",
    created_at: "2026-01-01T00:00:00.000000000Z",
    added: 12,
    edited: 0,
    deleted: 0,
  },
  {
    version: 0,
    description: "rename the dataset",
    created_at: "2025-12-31T00:00:00.000000000Z",
    added: 0,
    edited: 0,
    deleted: 0,
  },
];

function useVersions() {
  server.use(
    http.get(VERSIONS_PATH, () => HttpResponse.json({ data: VERSIONS, next_cursor: null })),
  );
}

/** The timeline row of a version, found by its label. */
function rowOf(version: number): HTMLElement {
  // Scoped to the list: the page header also shows the latest version's number.
  const list = screen.getByRole("list", { name: "Versions" });
  const row = within(list).getByText(`v${version}`).closest("li");
  if (!row) {
    throw new Error(`no row for v${version}`);
  }
  return row;
}

it("shows one row per version, newest first", async () => {
  useVersions();
  await renderApp(DATASET_URL);

  const list = await screen.findByRole("list", { name: "Versions" });
  const rows = within(list).getAllByRole("listitem");
  expect(rows).toHaveLength(VERSIONS.length);
  expect(rows[0]).toHaveTextContent("v5");
  expect(rows[5]).toHaveTextContent("v0");
  expect(within(rowOf(4)).getByText("import faq.csv (120 items)")).toBeInTheDocument();
});

it("gives each row the icon of its description", async () => {
  useVersions();
  await renderApp(DATASET_URL);
  await screen.findByRole("list", { name: "Versions" });

  expect(rowOf(5).querySelector(".lucide-waypoints")).not.toBeNull();
  expect(rowOf(4).querySelector(".lucide-upload")).not.toBeNull();
  expect(rowOf(3).querySelector(".lucide-trash-2")).not.toBeNull();
  expect(rowOf(2).querySelector(".lucide-pencil")).not.toBeNull();
  expect(rowOf(1).querySelector(".lucide-plus")).not.toBeNull();
  expect(rowOf(0).querySelector(".lucide-circle")).not.toBeNull();
});

it("marks the current version and offers View on the others", async () => {
  useVersions();
  await renderApp(DATASET_URL);
  await screen.findByRole("list", { name: "Versions" });

  expect(within(rowOf(CURRENT)).getByText("latest")).toBeInTheDocument();
  expect(within(rowOf(CURRENT)).queryByRole("link", { name: "View" })).toBeNull();
  expect(within(rowOf(1)).getByRole("link", { name: "View" })).toBeInTheDocument();
});

it("View sets the version in the search params", async () => {
  useVersions();
  const { router, user } = await renderApp(DATASET_URL);
  await screen.findByRole("list", { name: "Versions" });

  await user.click(within(rowOf(1)).getByRole("link", { name: "View" }));

  await waitFor(() => expect(router.state.location.search).toMatchObject({ version: 1 }));
});

it("shows the change counts and leaves out the zero ones", async () => {
  useVersions();
  await renderApp(DATASET_URL);
  await screen.findByRole("list", { name: "Versions" });

  expect(within(rowOf(4)).getByText("+120")).toBeInTheDocument();
  expect(within(rowOf(2)).getByText("~1")).toBeInTheDocument();
  expect(within(rowOf(3)).getByText("−1")).toBeInTheDocument();
  expect(within(rowOf(2)).queryByText(/^\+/)).toBeNull();
  expect(within(rowOf(0)).queryByText(/^[+~−]/)).toBeNull();
});

it("expands a version's changes on request", async () => {
  useVersions();
  server.use(http.get(CHANGES_PATH, () => HttpResponse.json(changesFixture)));
  const { user } = await renderApp(DATASET_URL);
  await screen.findByRole("list", { name: "Versions" });

  const toggle = within(rowOf(2)).getByRole("button", { name: "Changes" });
  expect(toggle).toHaveAttribute("aria-expanded", "false");
  expect(within(rowOf(2)).queryByText("Edited")).toBeNull();

  await user.click(toggle);
  await waitFor(() => expect(toggle).toHaveAttribute("aria-expanded", "true"));
  expect(await within(rowOf(2)).findByText("Edited")).toBeInTheDocument();
  expect(within(rowOf(2)).getByRole("link", { name: "00000d02" })).toBeInTheDocument();
});

it("offers no changes button on a version without changes", async () => {
  useVersions();
  await renderApp(DATASET_URL);
  await screen.findByRole("list", { name: "Versions" });

  expect(within(rowOf(0)).queryByRole("button", { name: "Changes" })).toBeNull();
});

it("restores an earlier version after confirming, then shows the latest items", async () => {
  useVersions();
  let captured: unknown;
  server.use(
    http.post(RESTORE_PATH, async ({ request }) => {
      captured = await request.json();
      return HttpResponse.json(restoredFixture);
    }),
  );
  const { router, user } = await renderApp(DATASET_URL);
  await screen.findByRole("list", { name: "Versions" });

  await user.click(within(rowOf(1)).getByRole("button", { name: "Restore this version" }));
  const dialog = await screen.findByRole("alertdialog");
  expect(within(dialog).getByText("Restore version 1?")).toBeInTheDocument();
  expect(
    within(dialog).getByText(`This creates version ${CURRENT + 1} with the items of version 1.`),
  ).toBeInTheDocument();
  await user.click(within(dialog).getByRole("button", { name: "Restore" }));

  await waitFor(() => expect(captured).toEqual({ version: 1 }));
  await waitFor(() => expect(router.state.location.search).toMatchObject({ tab: "items" }));
  expect(router.state.location.search).not.toHaveProperty("version");
});

it("shows the restore refusal in the dialog", async () => {
  useVersions();
  server.use(
    http.post(RESTORE_PATH, () =>
      problemResponse(400, "validation_failed", {
        errors: [{ field: "version", message: "version 1 already equals the latest version" }],
      }),
    ),
  );
  const { user } = await renderApp(DATASET_URL);
  await screen.findByRole("list", { name: "Versions" });

  await user.click(within(rowOf(1)).getByRole("button", { name: "Restore this version" }));
  const dialog = await screen.findByRole("alertdialog");
  await user.click(within(dialog).getByRole("button", { name: "Restore" }));

  expect(
    await within(dialog).findByText("version 1 already equals the latest version"),
  ).toBeInTheDocument();
});

it("offers no restore while a past version is shown", async () => {
  useVersions();
  await renderApp(`${DATASET_URL}&version=1`);
  await screen.findByRole("list", { name: "Versions" });

  expect(screen.queryByRole("button", { name: "Restore this version" })).toBeNull();
});
