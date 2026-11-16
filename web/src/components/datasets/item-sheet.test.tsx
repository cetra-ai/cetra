import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { ProjectProvider } from "@/components/shell/project-context";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import itemFixture from "../../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01/items/01900000-0000-7000-8000-000000000d02.json";
import versionsFixture from "../../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01/versions.json";
import projectFixture from "../../../../fixtures/api/static/project.json";
import { ItemSheet } from "./item-sheet";

const DATASET_ID = "01900000-0000-7000-8000-000000000c01";
// The fixture's JSON is plain; the generated types are stricter about metadata.
const ITEM = itemFixture as unknown as Schema<"DatasetItem">;
const PATH = "/api/v1/projects/:project/datasets/:dataset/items/:item_id";

function renderSheet({ editable = true }: { editable?: boolean } = {}) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const onOpenChange = vi.fn();
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
        <ItemSheet
          datasetId={DATASET_ID}
          item={ITEM}
          editable={editable}
          onOpenChange={onOpenChange}
        />
      </ProjectProvider>
    </QueryClientProvider>,
  );
  return { onOpenChange };
}

/** Replaces the text of a textarea. Pasting keeps `{` and `[` literal for user-event. */
async function setText(user: ReturnType<typeof userEvent.setup>, label: string, text: string) {
  const field = screen.getByRole("textbox", { name: label });
  await user.click(field);
  await user.clear(field);
  await user.paste(text);
}

it("saves only the changed fields", async () => {
  const user = userEvent.setup();
  let captured: unknown;
  server.use(
    http.patch(PATH, async ({ request }) => {
      captured = await request.json();
      return HttpResponse.json(ITEM);
    }),
  );
  const { onOpenChange } = renderSheet();

  await setText(user, "Metadata", '{"topic":"billing"}');
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
  expect(captured).toEqual({ metadata: { topic: "billing" } });
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
  const { onOpenChange } = renderSheet();

  await setText(user, "Expected output", "");
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
  expect(captured).toEqual({ expected_output: null });
});

it("refuses input that is not JSON and sends nothing", async () => {
  const user = userEvent.setup();
  const { onOpenChange } = renderSheet();

  await setText(user, "Input", "{");
  await user.click(screen.getByRole("button", { name: "Save" }));

  expect(await screen.findByText("Not valid JSON")).toBeInTheDocument();
  expect(onOpenChange).not.toHaveBeenCalled();
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
  renderSheet();

  await setText(user, "Metadata", '{"topic":"x"}');
  await user.click(screen.getByRole("button", { name: "Save" }));

  const input = screen.getByRole("textbox", { name: "Input" });
  const message = await screen.findByText('"q" is a required property');
  expect(input.parentElement).toContainElement(message);
});

it("deletes the item after confirming", async () => {
  const user = userEvent.setup();
  let deleted = false;
  server.use(
    http.delete(PATH, () => {
      deleted = true;
      return HttpResponse.json(versionsFixture.data[0]);
    }),
  );
  const { onOpenChange } = renderSheet();

  await user.click(screen.getByRole("button", { name: "Delete item" }));
  const dialog = await screen.findByRole("alertdialog");
  await user.click(within(dialog).getByRole("button", { name: "Delete item" }));

  await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
  expect(deleted).toBe(true);
});

it("is read-only on a past version", () => {
  renderSheet({ editable: false });

  expect(screen.queryByRole("textbox", { name: "Input" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
  expect(screen.getByText(/Can I export my traces/)).toBeInTheDocument();
});
