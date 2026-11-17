import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { ProjectProvider } from "@/components/shell/project-context";
import { TooltipProvider } from "@/components/ui/tooltip";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import itemFixture from "../../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01/items/01900000-0000-7000-8000-000000000d02.json";
import versionsFixture from "../../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01/versions.json";
import projectFixture from "../../../../fixtures/api/static/project.json";
import { ItemPanel } from "./item-panel";

const DATASET_ID = "01900000-0000-7000-8000-000000000c01";
// The fixture's JSON is plain; the generated types are stricter about metadata.
const ITEM = itemFixture as unknown as Schema<"DatasetItem">;
const NEXT = { ...ITEM, id: "01900000-0000-7000-8000-000000000d09" } as Schema<"DatasetItem">;
const PATH = "/api/v1/projects/:project/datasets/:dataset/items/:item_id";

function renderPanel({
  editable = true,
  next,
  item = ITEM,
}: {
  editable?: boolean;
  next?: Schema<"DatasetItem">;
  item?: Schema<"DatasetItem">;
} = {}) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const onClose = vi.fn();
  const onMove = vi.fn();
  const onDirtyChange = vi.fn();
  render(
    <QueryClientProvider client={queryClient}>
      <ProjectProvider
        value={{
          org: "acme",
          project: "support-bot",
          ref: "acme/support-bot",
          data: projectFixture as Schema<"Project">,
        }}
      >
        <TooltipProvider>
          <ItemPanel
            datasetId={DATASET_ID}
            item={item}
            editable={editable}
            previous={undefined}
            next={next}
            onMove={onMove}
            onClose={onClose}
            onDirtyChange={onDirtyChange}
          />
        </TooltipProvider>
      </ProjectProvider>
    </QueryClientProvider>,
  );
  return { onClose, onMove, onDirtyChange };
}

/** Opens the editable form of the panel. */
async function startEditing(user: ReturnType<typeof userEvent.setup>) {
  await user.click(screen.getByRole("button", { name: "Edit item" }));
}

/** Replaces the text of a textarea. Pasting keeps `{` and `[` literal for user-event. */
async function setText(user: ReturnType<typeof userEvent.setup>, label: string, text: string) {
  const field = screen.getByRole("textbox", { name: label });
  await user.click(field);
  await user.clear(field);
  await user.paste(text);
}

it("shows the input, expected output and metadata", () => {
  renderPanel();

  expect(screen.getByRole("heading", { name: "Input" })).toBeInTheDocument();
  expect(screen.getByRole("heading", { name: "Expected output" })).toBeInTheDocument();
  expect(screen.getByRole("heading", { name: "Metadata" })).toBeInTheDocument();
  expect(screen.getByText(/Can I export my traces/)).toBeInTheDocument();
});

it("saves only the changed fields and shows the item again", async () => {
  const user = userEvent.setup();
  let captured: unknown;
  server.use(
    http.patch(PATH, async ({ request }) => {
      captured = await request.json();
      return HttpResponse.json(ITEM);
    }),
  );
  renderPanel();

  await startEditing(user);
  await setText(user, "Metadata", '{"topic":"billing"}');
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => expect(screen.queryByRole("button", { name: "Save" })).toBeNull());
  expect(captured).toEqual({ metadata: { topic: "billing" } });
  expect(screen.getByRole("button", { name: "Edit item" })).toBeInTheDocument();
});

it("clears the expected output", async () => {
  const user = userEvent.setup();
  let captured: unknown;
  server.use(
    http.patch(PATH, async ({ request }) => {
      captured = await request.json();
      return HttpResponse.json(ITEM);
    }),
  );
  renderPanel();

  await startEditing(user);
  await setText(user, "Expected output", "");
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => expect(captured).toEqual({ expected_output: null }));
});

it("refuses input that is not JSON and sends nothing", async () => {
  const user = userEvent.setup();
  let sent = false;
  server.use(
    http.patch(PATH, () => {
      sent = true;
      return HttpResponse.json(ITEM);
    }),
  );
  renderPanel();

  await startEditing(user);
  await setText(user, "Input", "{");
  await user.click(screen.getByRole("button", { name: "Save" }));

  expect(await screen.findByText("Not valid JSON")).toBeInTheDocument();
  expect(sent).toBe(false);
});

it("places a schema error under the input", async () => {
  const user = userEvent.setup();
  server.use(
    http.patch(PATH, () =>
      problemResponse(400, "validation_failed", {
        errors: [{ field: "input", message: '"q" is a required property' }],
      }),
    ),
  );
  renderPanel();

  await startEditing(user);
  await setText(user, "Metadata", '{"topic":"x"}');
  await user.click(screen.getByRole("button", { name: "Save" }));

  const input = screen.getByRole("textbox", { name: "Input" });
  const message = await screen.findByText('"q" is a required property');
  expect(input.parentElement).toContainElement(message);
});

it("reports a changed form as dirty", async () => {
  const user = userEvent.setup();
  const { onDirtyChange } = renderPanel();

  await startEditing(user);
  await setText(user, "Metadata", '{"topic":"x"}');

  expect(onDirtyChange).toHaveBeenCalledWith(true);
});

it("deletes the item after confirming and closes the panel", async () => {
  const user = userEvent.setup();
  let deleted = false;
  server.use(
    http.delete(PATH, () => {
      deleted = true;
      return HttpResponse.json(versionsFixture.data[0]);
    }),
  );
  const { onClose } = renderPanel();

  await startEditing(user);
  await user.click(screen.getByRole("button", { name: "Delete item" }));
  const dialog = await screen.findByRole("alertdialog");
  await user.click(within(dialog).getByRole("button", { name: "Delete item" }));

  await waitFor(() => expect(onClose).toHaveBeenCalled());
  expect(deleted).toBe(true);
});

it("is read-only on a past version", () => {
  renderPanel({ editable: false });

  expect(screen.queryByRole("button", { name: "Edit item" })).toBeNull();
  expect(screen.queryByRole("textbox", { name: "Input" })).toBeNull();
  expect(screen.getByText(/Can I export my traces/)).toBeInTheDocument();
});

it("moves to the next item", async () => {
  const user = userEvent.setup();
  const { onMove } = renderPanel({ next: NEXT });

  expect(screen.getByRole("button", { name: "Previous item" })).toBeDisabled();
  await user.click(screen.getByRole("button", { name: "Next item" }));

  expect(onMove).toHaveBeenCalledWith(NEXT);
});
